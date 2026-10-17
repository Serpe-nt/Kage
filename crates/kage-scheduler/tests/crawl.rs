//! The politeness of a crawl (spec §13, §11), with a scripted executor on a
//! paused clock: the pages of a crawl are fetches like any other, so the
//! scheduler paces them, keeps the visit with one persona, rests between
//! two pages, waits out a paused site and a visitor's cooldown, and never
//! lets more personas read a site at once than its pacing allows.
//!
//! The reader here is the job worker's loop in small: the crawl's own
//! frontier (`kage-crawl`) says which page comes next and how it is
//! reached, and every page is asked of the pipeline the way the worker
//! asks, one per visitor at a time.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Duration as Span, Utc};
use common::{DEFAULT_DOMAINS, World, blocked};
use kage_core::config::DEFAULT_SKIPPED_EXTENSIONS;
use kage_core::domain::{BreakerState, RenderMode};
use kage_core::ids::{PersonaId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::{Refusal, Visitors};
use kage_core::schedule::queue::Priority;
use kage_core::urlnorm::NormalizeOptions;
use kage_crawl::frontier::{EntryId, FoundLink, Frontier, Limits, PageRead, Way};
use kage_crawl::scope::{Scope, ScopeRequest};
use kage_scheduler::{Arrival, FetchOrder, JobFetch, Stop};
use tokio::sync::watch;
use url::Url;

const SITE: &str = "https://shop.example";

fn url(path: &str) -> Url {
    Url::parse(&format!("{SITE}{path}")).unwrap()
}

/// A stop signal that never says stop, and its sender.
fn never() -> (watch::Sender<bool>, Stop) {
    watch::channel(false)
}

/// A site of `pages` pages: page `n` links to the two after it, so a
/// reader finds a link on the open page most of the time and goes back
/// for one now and then.
fn links_of(page: &Url, pages: usize) -> Vec<FoundLink> {
    let number: usize = page.path().trim_start_matches("/p/").parse().unwrap_or(0);
    [number * 2 + 1, number * 2 + 2]
        .into_iter()
        .filter(|next| *next < pages)
        .map(|next| FoundLink {
            url: url(&format!("/p/{next}")),
            hidden: false,
            furniture: false,
        })
        .collect()
}

fn frontier(max_pages: u32) -> Frontier {
    let seed = url("/p/0");
    let scope = Scope::new(
        &ScopeRequest {
            allow: Vec::new(),
            deny: Vec::new(),
            same_site_only: true,
            max_depth: 32,
            skipped_extensions: DEFAULT_SKIPPED_EXTENSIONS
                .iter()
                .map(|extension| (*extension).to_owned())
                .collect(),
        },
        std::slice::from_ref(&seed),
    )
    .unwrap();
    let mut frontier = Frontier::new(
        scope,
        Limits {
            max_pages,
            max_entries: 10_000,
            back_limit: 3,
            near_duplicate_bits: 6,
            sitemap_depth: 0,
        },
        NormalizeOptions::default(),
    );
    frontier.add_seed(&seed).unwrap();
    frontier
}

/// One visitor of a crawl as the job worker drives it.
struct Reader {
    frontier: Frontier,
    pages: usize,
    tab: Vec<EntryId>,
    persona: Option<PersonaId>,
    /// Personas that never read a page of the crawl: its scouts.
    exclude: Vec<PersonaId>,
    /// Every wait the scheduler asked for, and why.
    waits: Vec<(Refusal, Span)>,
    /// How each page was to be reached, in order.
    ways: Vec<&'static str>,
}

impl Reader {
    fn new(pages: usize) -> Self {
        Self {
            frontier: frontier(u32::try_from(pages).unwrap()),
            pages,
            tab: Vec::new(),
            persona: None,
            exclude: Vec::new(),
            waits: Vec::new(),
            ways: Vec::new(),
        }
    }

    /// The order of the next page, as the worker makes it.
    fn next(&mut self) -> Option<(EntryId, FetchOrder)> {
        let pick = self.frontier.next("shop.example", &self.tab)?;
        let page = pick.url.clone();
        let arrival = match &pick.way {
            Way::Typed => Arrival::Typed,
            Way::Link { from, .. } => Arrival::Link {
                from: from.clone(),
                max_backs: 3,
            },
        };
        self.ways.push(match &pick.way {
            Way::Typed => "typed",
            Way::Link { backs: 0, .. } => "click",
            Way::Link { .. } => "back",
        });
        self.frontier.begin(pick.entry);
        let order = FetchOrder {
            render: RenderMode::Browser,
            visitors: Some(Visitors {
                known: self.persona.into_iter().collect(),
                max: 1,
            }),
            patience: Some(Duration::from_mins(5)),
            arrival: Some(arrival),
            exclude: self.exclude.clone(),
            ..FetchOrder::get(page, Priority::Job)
        };
        Some((pick.entry, order))
    }

    /// Reads one page to its end, waiting where the scheduler says to.
    /// Returns the outcome, or `None` when the crawl has no page left.
    async fn read(&mut self, world: &World, stop: &Stop) -> Option<Outcome> {
        let (entry, order) = self.next()?;
        let request = RequestId::new();
        loop {
            let fetch = world
                .pipeline
                .fetch_for_job(request, order.clone(), None, stop.clone())
                .await
                .unwrap();
            match fetch {
                JobFetch::Finished(report) => {
                    for attempt in &report.attempts {
                        if attempt.outcome.is_block() && self.persona == Some(attempt.persona_id) {
                            self.persona = None;
                            self.tab.clear();
                        }
                    }
                    if report.outcome.is_success() {
                        let reader = report.persona.as_ref().unwrap().id;
                        if self.persona != Some(reader) {
                            self.tab.clear();
                        }
                        self.persona = Some(reader);
                        let page = self.frontier.entry(entry).unwrap().url.clone();
                        self.frontier.record_read(
                            entry,
                            &PageRead {
                                links: links_of(&page, self.pages),
                                final_url: page,
                                links_truncated: false,
                                text: String::new(),
                            },
                        );
                        self.tab.push(entry);
                        if self.tab.len() > 4 {
                            self.tab.remove(0);
                        }
                    } else {
                        self.frontier.record_failed(entry);
                    }
                    return Some(report.outcome);
                }
                JobFetch::Deferred(wait) => {
                    let back = wait.retry_at.unwrap();
                    self.waits.push((wait.reason, back - world.now()));
                    tokio::time::sleep((back - world.now()).to_std().unwrap()).await;
                }
                JobFetch::Stopped => panic!("nothing said stop"),
            }
        }
    }
}

/// When every attempt the script saw began and ended, with its persona.
fn spans(world: &World, took: Duration) -> Vec<(PersonaId, DateTime<Utc>, DateTime<Utc>)> {
    world
        .script
        .seen()
        .iter()
        .map(|seen| {
            (
                seen.job.persona.id,
                seen.at,
                seen.at + Span::from_std(took).unwrap(),
            )
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_crawl_is_one_visitor_reading_at_a_readers_pace_and_resting_between_pages() {
    let mut world = World::with(8, false, DEFAULT_DOMAINS, &[]);
    let (_keep, stop) = never();
    // A page is read for longer than the gap between two pages: without
    // a rest the next one would be asked for the instant it is left.
    let took = Duration::from_secs(20);
    world.script.takes(took);
    let mut reader = Reader::new(40);

    let mut read = 0;
    while let Some(outcome) = reader.read(&world, &stop).await {
        assert_eq!(outcome, Outcome::Ok);
        read += 1;
    }

    assert_eq!(read, 40);
    assert_eq!(reader.frontier.counts().read, 40);
    // One address read all of it, in more than one sitting: its long
    // breaks were waited out and nobody was sent in its place.
    let readers: BTreeSet<PersonaId> = world.script.personas().into_iter().collect();
    assert_eq!(readers.len(), 1, "{readers:?}");
    assert!(
        reader
            .waits
            .iter()
            .any(|(reason, wait)| *reason == Refusal::NoPersona && *wait > Span::minutes(5)),
        "{:?}",
        reader.waits
    );
    // Every page was a fetch of its own in the scheduler's order, one at
    // a time: no two of them overlap.
    let spans = spans(&world, took);
    assert_eq!(spans.len(), 40);
    let rests: Vec<Span> = spans.windows(2).map(|pair| pair[1].1 - pair[0].2).collect();
    // The rest between leaving a page and asking for the next (F43): the
    // profile's floor at the pace of the fastest persona, never nothing,
    // and never the same twice in a row.
    assert!(
        rests.iter().all(|rest| *rest >= Span::milliseconds(560)),
        "{:?}",
        rests.iter().min()
    );
    let distinct: BTreeSet<i64> = rests.iter().map(Span::num_milliseconds).collect();
    assert!(distinct.len() > 35, "{}", distinct.len());
    // Within a sitting the rest is seconds, not the gap of a timer.
    let short = rests
        .iter()
        .filter(|rest| **rest < Span::seconds(30))
        .count();
    assert!(short >= 25, "{short} of {}", rests.len());

    // How the pages were to be reached is the reader's order: the seed
    // typed, most pages by a link of the open page, some after going
    // back, and an address typed only when the tab leads nowhere new.
    let seen = world.script.seen();
    assert_eq!(seen[0].job.arrival, Some(Arrival::Typed));
    let ways: BTreeMap<&str, usize> = reader.ways.iter().fold(BTreeMap::new(), |mut ways, way| {
        *ways.entry(*way).or_default() += 1;
        ways
    });
    assert_eq!(ways["typed"] + ways["click"] + ways["back"], 40);
    assert!(ways["click"] >= 15, "{ways:?}");
    assert!(ways["back"] >= 5, "{ways:?}");
    assert!(ways["typed"] <= 12, "{ways:?}");
    for (seen, way) in seen.iter().zip(&reader.ways) {
        match (&seen.job.arrival, *way) {
            (Some(Arrival::Typed), "typed") => {}
            (Some(Arrival::Link { from, max_backs: 3 }), "click" | "back") => {
                assert!(from.as_str().starts_with(SITE), "{from}");
            }
            other => panic!("the order does not say what the frontier said: {other:?}"),
        }
    }
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_crawl_waits_out_a_paused_site_and_a_visitor_that_was_turned_away() {
    let rules = "defaults: { breaker_threshold: 2, breaker_pause: 30m, retries: 1 }\ndomains: {}\n";
    let mut world = World::with(8, false, rules, &[]);
    let (_keep, stop) = never();
    world.script.takes(Duration::from_secs(8));
    let mut reader = Reader::new(30);
    // The persona that read the crawl's sitemaps never reads a page.
    let scout = common::id(0);
    reader.exclude = vec![scout];

    for _ in 0..5 {
        assert_eq!(reader.read(&world, &stop).await, Some(Outcome::Ok));
    }
    let first = reader.persona.unwrap();
    assert_ne!(first, scout);

    // The site turns the visitor away, and whoever comes in its place:
    // the page fails, and the breaker opens.
    world
        .script
        .then(blocked(Outcome::Forbidden, 1.0))
        .then(blocked(Outcome::Forbidden, 1.0));
    let before = world.script.seen().len();
    assert_eq!(reader.read(&world, &stop).await, Some(Outcome::Forbidden));
    let tried: Vec<PersonaId> = world.script.personas()[before..].to_vec();
    assert_eq!(tried.len(), 2);
    assert_eq!(tried[0], first);
    assert!(!tried.contains(&scout));
    assert_ne!(tried[0], tried[1]);
    let breaker = world.backend.breaker("shop.example").unwrap();
    assert_eq!(breaker.state, BreakerState::Open);

    // The next page is not attempted and not failed: the crawl is told
    // when the pause ends, and nothing is asked of the site meanwhile.
    let asked = world.script.seen().len();
    let paused_at = world.now();
    assert_eq!(reader.read(&world, &stop).await, Some(Outcome::Ok));
    let (reason, wait) = *reader
        .waits
        .iter()
        .find(|(reason, _)| *reason == Refusal::DomainPaused)
        .expect("the crawl waited for the site");
    assert_eq!(reason, Refusal::DomainPaused);
    assert!(
        wait > Span::minutes(25) && wait <= Span::minutes(31),
        "{wait}"
    );
    let trial = &world.script.seen()[asked];
    assert!(trial.at >= paused_at + Span::minutes(25), "{}", trial.at);
    assert_eq!(world.script.seen().len(), asked + 1);

    // Neither of the two that were turned away comes back while it cools
    // (a day for a 403), and the scout never does: a third persona is the
    // crawl's visitor from here on, with a tab of its own.
    let third = reader.persona.unwrap();
    assert!(!tried.contains(&third));
    assert_ne!(third, scout);
    for _ in 0..8 {
        assert_eq!(reader.read(&world, &stop).await, Some(Outcome::Ok));
        assert_eq!(reader.persona, Some(third));
    }
    let later: BTreeSet<PersonaId> = world.script.personas()[asked..].iter().copied().collect();
    assert_eq!(later, BTreeSet::from([third]));
    // The page after the change of hands was typed by the newcomer: it
    // has nothing open to click on.
    assert_eq!(trial.job.arrival, Some(Arrival::Typed));
    assert!(world.now() < paused_at + Span::hours(20));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn no_more_personas_read_a_site_at_once_than_its_pacing_allows() {
    // Five crawls of one site side by side, each with one visitor of its
    // own: the site's pacing allows three personas at a time (`human`).
    let mut world = World::with(10, false, DEFAULT_DOMAINS, &[]);
    let (_keep, stop) = never();
    let took = Duration::from_secs(15);
    world.script.takes(took);

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..5 {
        let pipeline = world.pipeline.clone();
        let clock = std::sync::Arc::clone(&world.clock);
        let stop = stop.clone();
        set.spawn(async move {
            let mut visitor: Option<PersonaId> = None;
            let mut read = 0;
            let mut page = 0;
            while read < 6 {
                let order = FetchOrder {
                    render: RenderMode::Browser,
                    visitors: Some(Visitors {
                        known: visitor.into_iter().collect(),
                        max: 1,
                    }),
                    patience: Some(Duration::from_mins(5)),
                    arrival: Some(Arrival::Typed),
                    ..FetchOrder::get(url(&format!("/p/{page}")), Priority::Job)
                };
                match pipeline
                    .fetch_for_job(RequestId::new(), order, None, stop.clone())
                    .await
                    .unwrap()
                {
                    JobFetch::Finished(report) => {
                        assert_eq!(report.outcome, Outcome::Ok);
                        visitor = Some(report.persona.as_ref().unwrap().id);
                        read += 1;
                        page += 1;
                    }
                    JobFetch::Deferred(wait) => {
                        let back = wait.retry_at.unwrap();
                        let wait = (back - clock.now()).to_std().unwrap_or_default();
                        tokio::time::sleep(wait).await;
                    }
                    JobFetch::Stopped => panic!("nothing said stop"),
                }
            }
        });
    }
    while let Some(joined) = set.join_next().await {
        joined.unwrap();
    }

    // At no moment were more than three personas reading the site, and
    // more than three read it in all: the others waited for a place.
    let spans = spans(&world, took);
    assert_eq!(spans.len(), 30);
    let mut most = 0;
    for (_, at, _) in &spans {
        let reading: BTreeSet<PersonaId> = spans
            .iter()
            .filter(|(_, from, to)| from <= at && at < to)
            .map(|(persona, _, _)| *persona)
            .collect();
        most = most.max(reading.len());
    }
    assert!(most <= 3, "{most} personas read the site at once");
    assert!(most >= 2, "{most}");
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn what_a_crawl_takes_from_a_page_is_stored_with_its_result() {
    let mut world = World::new(3, DEFAULT_DOMAINS);
    let url = "https://shop.example/list";
    let html = "<html><head><title>List</title></head><body><nav><a href=\"/menu\">Menu</a></nav>\
                <main><p>Some words a reader reads.</p><a href=\"/a\">a</a>\
                <a href=\"/hid\" style=\"display:none\">hid</a></main></body></html>";
    let crawling = || FetchOrder {
        render: RenderMode::Browser,
        read_page: true,
        ..FetchOrder::get(Url::parse(url).unwrap(), Priority::Job)
    };

    // The browser brought its links: those are what the crawl takes, with
    // whether a reader can see each.
    world.script.then(common::page_with(
        url,
        &serde_json::json!({
            "dom_html": html,
            "links": [
                { "url": "https://shop.example/menu", "text": "Menu", "rel": [], "visible": true, "furniture": true },
                { "url": "https://shop.example/a", "text": "a", "rel": [], "visible": true, "furniture": false },
                { "url": "https://shop.example/hid", "text": "hid", "rel": [], "visible": false, "furniture": false },
            ],
            "links_truncated": false,
        }),
    ));
    let report = world.pipeline.fetch(crawling()).await.unwrap();
    assert_eq!(report.outcome, Outcome::Ok);
    let reading = report
        .reading
        .clone()
        .expect("a crawl's page has a reading");
    assert_eq!(reading.final_url.as_deref(), Some(url));
    let seen: Vec<(&str, bool, bool)> = reading
        .links
        .iter()
        .map(|link| (link.url.as_str(), link.hidden, link.furniture))
        .collect();
    assert_eq!(
        seen,
        [
            ("https://shop.example/menu", false, true),
            ("https://shop.example/a", false, false),
            ("https://shop.example/hid", true, false),
        ]
    );
    assert!(
        reading.text.contains("Some words a reader reads."),
        "{reading:?}"
    );
    // The text two pages are compared by: the page's main text.
    assert_eq!(reading.text, report.page.as_ref().unwrap().main_text);
    // The same is stored with the result, for a crawl that changes hands.
    let stored = world.backend.records().pop().unwrap();
    assert_eq!(stored.request_id, report.request);
    assert_eq!(
        stored.reading,
        Some(serde_json::to_value(&reading).unwrap())
    );

    // Without the browser's links, the markup's, with its own hiding.
    world.script.then(common::document(url, html));
    let report = world.pipeline.fetch(crawling()).await.unwrap();
    let marked: Vec<(String, bool, bool)> = report
        .reading
        .unwrap()
        .links
        .into_iter()
        .map(|link| (link.url, link.hidden, link.furniture))
        .collect();
    assert_eq!(
        marked,
        [
            ("https://shop.example/menu".to_owned(), false, true),
            ("https://shop.example/a".to_owned(), false, false),
            ("https://shop.example/hid".to_owned(), true, false),
        ]
    );

    // A fetch that is no crawl's keeps nothing of the kind, and neither does
    // a page that was not got.
    world.script.then(common::document(url, html));
    let plain = world
        .pipeline
        .fetch(FetchOrder {
            read_page: false,
            ..crawling()
        })
        .await
        .unwrap();
    assert_eq!(plain.reading, None);
    assert_eq!(world.backend.records().pop().unwrap().reading, None);
    world.script.then(blocked(Outcome::Forbidden, 0.95));
    let walled = world
        .pipeline
        .fetch(FetchOrder {
            max_retries: Some(0),
            ..crawling()
        })
        .await
        .unwrap();
    assert_eq!(walled.outcome, Outcome::Forbidden);
    assert_eq!(walled.reading, None);
    world.shut_down().await;
}
