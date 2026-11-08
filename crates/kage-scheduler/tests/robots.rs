//! `robots.txt` on the way of a fetch (spec §14), with a scripted executor
//! on a paused clock: nobody asks unless the site's policy says so; where
//! it does, a scout asks and the reader never; what the file says is
//! reported, and kept to only where the policy enforces it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::time::Duration;

use chrono::Duration as Span;
use common::{DEFAULT_DOMAINS, World, blocked, file, id, page_with, server_error};
use kage_browser::contract::FlowAction;
use kage_core::domain::{Header, Level, Render, RobotsVerdict, Timing};
use kage_core::ids::{PersonaId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::Refusal;
use kage_core::schedule::queue::Priority;
use kage_http::Fetched;
use kage_scheduler::{
    Arrival, AttemptOutput, FetchOrder, FetchReport, FlowPlan, JobFetch, Peek, ROBOTS_NO_SCOUT,
    ROBOTS_UNAVAILABLE, RobotsRead, RobotsWish, ScoutNeed, Stop,
};
use tokio::sync::watch;
use url::Url;

/// One site whose file is enforced, one whose file is reported, and every
/// other site left alone.
const SITES: &str = "defaults: { pacing: fast, active_hours: ignore }\n\
                     domains:\n  enforced.example: { robots: enforce }\n  \
                     reported.example: { robots: report }\n";

/// A file that closes one directory and asks for half a minute between
/// two pages.
const RULES: &str = "User-agent: *\nDisallow: /private\nCrawl-delay: 30\n\
                     Sitemap: https://enforced.example/map.xml\n";

fn get(url: &str) -> FetchOrder {
    FetchOrder {
        level: Level::Human,
        ..FetchOrder::get(Url::parse(url).unwrap(), Priority::Sync)
    }
}

/// Every `robots.txt` that is asked for is answered with what `answer`
/// makes of its address.
fn robots_txt(world: &World, answer: impl Fn(&str) -> AttemptOutput + Send + 'static) {
    world
        .script
        .when(move |job| (job.url.path() == "/robots.txt").then(|| answer(job.url.as_str())));
}

/// An address and who asked for it.
type Ask = (String, PersonaId);

/// The attempts asked for so far that were for a `robots.txt`, and those
/// that were for a page: who asked for each.
fn asked(world: &World) -> (Vec<Ask>, Vec<Ask>) {
    world
        .script
        .seen()
        .into_iter()
        .map(|seen| (seen.job.url.to_string(), seen.job.persona.id))
        .partition(|(url, _)| url.ends_with("/robots.txt"))
}

fn allowed(crawl_delay: Option<f64>) -> RobotsVerdict {
    RobotsVerdict {
        fetched: true,
        allowed: true,
        matched_rule: None,
        crawl_delay,
    }
}

const UNASKED: RobotsVerdict = RobotsVerdict {
    fetched: false,
    allowed: true,
    matched_rule: None,
    crawl_delay: None,
};

/// A file kept as unreachable: not the file's verdict, and never `allowed`
/// (QA round three's level0-04).
const UNREACHABLE: RobotsVerdict = RobotsVerdict {
    fetched: false,
    allowed: false,
    matched_rule: None,
    crawl_delay: None,
};

fn never() -> (watch::Sender<bool>, Stop) {
    watch::channel(false)
}

fn finished(fetch: JobFetch) -> FetchReport {
    match fetch {
        JobFetch::Finished(report) => *report,
        other => panic!("the fetch did not finish: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn nobody_asks_for_a_robots_txt_unless_the_sites_policy_says_so() {
    let mut world = World::new(3, DEFAULT_DOMAINS);
    robots_txt(&world, |url| file(url, 200, "User-agent: *\nDisallow: /\n"));

    for path in ["/", "/a", "/private/b"] {
        let report = world
            .pipeline
            .fetch(get(&format!("https://quiet.example{path}")))
            .await
            .unwrap();
        assert_eq!(report.outcome, Outcome::Ok);
        // Nothing was asked for and nothing is known: the result says so.
        assert_eq!(report.robots, Some(UNASKED));
    }
    let (files, pages) = asked(&world);
    assert_eq!(files, []);
    assert_eq!(pages.len(), 3);
    assert!(
        world
            .backend
            .robots_entry("https://quiet.example")
            .is_none()
    );
    assert_eq!(world.backend.scout_of("quiet.example"), None);
    assert!(
        world
            .backend
            .records()
            .iter()
            .all(|record| record.robots == Some(UNASKED))
    );
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn where_a_file_is_enforced_a_scout_asks_first_and_the_reader_never() {
    let mut world = World::new(4, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));

    let first = world
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(first.outcome, Outcome::Ok);
    assert_eq!(first.robots, Some(allowed(Some(30.0))));

    // The file was asked for before the page, by another persona, as a
    // person who typed its address into a browser, once.
    let seen = world.script.seen();
    assert_eq!(
        world.script.urls(),
        [
            "https://enforced.example/robots.txt",
            "https://enforced.example/a"
        ]
    );
    let scouting = &seen[0].job;
    let scout = scouting.persona.id;
    assert_eq!(scouting.render, Render::Browser);
    assert_eq!(scouting.arrival, Some(Arrival::Typed));
    assert!(scouting.capture.raw);
    assert_eq!(scouting.max_body_bytes, Some(512 * 1024));
    assert_eq!(scouting.n, 1);
    assert_ne!(seen[1].job.persona.id, scout);
    assert_eq!(world.backend.scout_of("enforced.example"), Some(scout));

    // What it brought back is kept for about a day, and its own request is
    // Kage's, with no verdict about itself.
    let entry = world
        .backend
        .robots_entry("https://enforced.example")
        .unwrap();
    assert_eq!(entry.site, "enforced.example");
    assert_eq!(entry.body.as_deref(), Some(RULES));
    assert_eq!((entry.status, entry.unreachable), (Some(200), false));
    let keep = entry.expires_at - entry.fetched_at;
    assert!(
        keep >= Span::minutes(1296) && keep < Span::minutes(1584),
        "{keep}"
    );
    let requests = world.backend.requests();
    let own = requests
        .iter()
        .find(|request| request.url == "https://enforced.example/robots.txt")
        .unwrap();
    assert_eq!((own.api_key_id, own.job_id), (None, None));
    assert_eq!(own.spec["robots"], true);
    let records = world.backend.records();
    let own_result = records
        .iter()
        .find(|record| record.request_id == own.id)
        .unwrap();
    assert_eq!(own_result.robots, None);

    // What the file disallows is not asked for: no persona, no attempt,
    // and the result names the rule.
    let denied = world
        .pipeline
        .fetch(get("https://enforced.example/private/ledger"))
        .await
        .unwrap();
    assert_eq!(denied.outcome, Outcome::RobotsDenied);
    assert_eq!((denied.reason.as_deref(), denied.retry_at), (None, None));
    assert!(denied.attempts.is_empty() && denied.persona.is_none());
    assert_eq!(
        denied.robots,
        Some(RobotsVerdict {
            fetched: true,
            allowed: false,
            matched_rule: Some("Disallow: /private".to_owned()),
            crawl_delay: Some(30.0),
        })
    );
    assert_eq!(world.script.seen().len(), 2);

    // Every later page of the origin is judged by the file that is kept:
    // one request for it, however many pages, and the scout reads none.
    for number in 0..6 {
        let report = world
            .pipeline
            .fetch(get(&format!("https://enforced.example/page/{number}")))
            .await
            .unwrap();
        assert_eq!(report.outcome, Outcome::Ok);
        assert_eq!(report.robots, Some(allowed(Some(30.0))));
    }
    let (files, pages) = asked(&world);
    assert_eq!(files.len(), 1);
    assert_eq!(pages.len(), 7);
    assert!(pages.iter().all(|(_, persona)| *persona != scout));

    // Another origin of the site has a file of its own, and the same
    // scout asks for it: one address is marked, and no other.
    let other = world
        .pipeline
        .fetch(get("https://www.enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(other.outcome, Outcome::Ok);
    let (files, pages) = asked(&world);
    assert_eq!(
        files,
        [
            ("https://enforced.example/robots.txt".to_owned(), scout),
            ("https://www.enforced.example/robots.txt".to_owned(), scout),
        ]
    );
    assert!(pages.iter().all(|(_, persona)| *persona != scout));

    // A day and a half later the file is due: the same scout asks again,
    // before the page.
    tokio::time::sleep(Duration::from_hours(36)).await;
    world
        .pipeline
        .fetch(get("https://enforced.example/later"))
        .await
        .unwrap();
    let (files, _) = asked(&world);
    assert_eq!(files.len(), 3);
    assert_eq!(
        files[2],
        ("https://enforced.example/robots.txt".to_owned(), scout)
    );
    assert_eq!(
        world.script.urls().last().map(String::as_str),
        Some("https://enforced.example/later")
    );
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_crawl_delay_is_kept_to_where_the_file_is_enforced_and_only_there() {
    // A scout and one reader.
    let mut world = World::new(2, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));

    let starts = |world: &World, host: &str| -> Vec<chrono::DateTime<chrono::Utc>> {
        world
            .script
            .seen()
            .iter()
            .filter(|seen| {
                seen.job.url.host_str() == Some(host) && seen.job.url.path() != "/robots.txt"
            })
            .map(|seen| seen.at)
            .collect()
    };

    for number in 0..5 {
        let report = world
            .pipeline
            .fetch(get(&format!("https://enforced.example/{number}")))
            .await
            .unwrap();
        assert_eq!(report.outcome, Outcome::Ok);
    }
    let enforced = starts(&world, "enforced.example");
    assert_eq!(enforced.len(), 5);
    for pair in enforced.windows(2) {
        let gap = pair[1] - pair[0];
        // What the site asked for, and up to a fifth more: never less,
        // and never the same twice.
        assert!(
            gap >= Span::seconds(30) && gap <= Span::seconds(37),
            "{gap}"
        );
    }
    let mut gaps: Vec<_> = enforced.windows(2).map(|pair| pair[1] - pair[0]).collect();
    gaps.dedup();
    assert_eq!(gaps.len(), 4);

    // The same file where it is only reported: the verdict says what the
    // site asks for, and the pace is the profile's.
    for number in 0..5 {
        world
            .pipeline
            .fetch(get(&format!("https://reported.example/{number}")))
            .await
            .unwrap();
    }
    let reported = starts(&world, "reported.example");
    assert_eq!(reported.len(), 5);
    for pair in reported.windows(2) {
        assert!(
            pair[1] - pair[0] < Span::seconds(10),
            "{}",
            pair[1] - pair[0]
        );
    }
    let last = world.backend.records().last().cloned().unwrap();
    assert_eq!(last.robots.unwrap().crawl_delay, Some(30.0));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn where_a_file_is_reported_nothing_waits_and_nothing_is_refused() {
    let mut world = World::new(3, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));

    // The first fetch has no verdict and does not wait for one.
    let first = world
        .pipeline
        .fetch(get("https://reported.example/private/a"))
        .await
        .unwrap();
    assert_eq!(first.outcome, Outcome::Ok);
    assert_eq!(first.robots, Some(UNASKED));
    assert_eq!(world.script.urls()[0], "https://reported.example/private/a");

    // The scout follows; the next result of the origin has the verdict,
    // and the page is fetched whatever it says.
    tokio::time::sleep(Duration::from_mins(2)).await;
    let (files, pages) = asked(&world);
    assert_eq!(files.len(), 1);
    let scout = files[0].1;
    assert_ne!(pages[0].1, scout);
    assert_eq!(world.backend.scout_of("reported.example"), Some(scout));

    let second = world
        .pipeline
        .fetch(get("https://reported.example/private/b"))
        .await
        .unwrap();
    assert_eq!(second.outcome, Outcome::Ok);
    assert_eq!(
        second.robots,
        Some(RobotsVerdict {
            fetched: true,
            allowed: false,
            matched_rule: Some("Disallow: /private".to_owned()),
            crawl_delay: Some(30.0),
        })
    );
    assert_eq!(second.attempts.len(), 1);
    let (files, pages) = asked(&world);
    assert_eq!((files.len(), pages.len()), (1, 2));
    assert!(pages.iter().all(|(_, persona)| *persona != scout));

    // Many fetches at once while nothing is known: one scout, one request.
    let mut burst = Vec::new();
    for number in 0..6 {
        let pipeline = world.pipeline.clone();
        burst.push(tokio::spawn(async move {
            pipeline
                .fetch(get(&format!("https://www.reported.example/{number}")))
                .await
                .unwrap()
        }));
    }
    for fetch in burst {
        assert_eq!(fetch.await.unwrap().outcome, Outcome::Ok);
    }
    tokio::time::sleep(Duration::from_mins(2)).await;
    let (files, _) = asked(&world);
    assert_eq!(
        files[1..],
        [("https://www.reported.example/robots.txt".to_owned(), scout)]
    );
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_file_that_is_not_there_allows_everything_and_one_that_cannot_be_had_nothing() {
    let mut world = World::new(4, SITES);
    robots_txt(&world, |url| {
        if url.starts_with("https://enforced.example") {
            file(url, 404, "Not found")
        } else {
            server_error(503)
        }
    });

    // No file: asked once, everything allowed for about a day.
    for path in ["/a", "/private/b"] {
        let report = world
            .pipeline
            .fetch(get(&format!("https://enforced.example{path}")))
            .await
            .unwrap();
        assert_eq!(report.outcome, Outcome::Ok);
        assert_eq!(report.robots, Some(allowed(None)));
    }
    let missing = world
        .backend
        .robots_entry("https://enforced.example")
        .unwrap();
    assert_eq!(
        (missing.status, missing.body, missing.unreachable),
        (Some(404), None, false)
    );
    assert_eq!(asked(&world).0.len(), 1);

    // A server error: nothing is known, so nothing of the origin is
    // fetched (RFC 9309: unreachable means disallowed) until the file is
    // asked for again an hour later. A caller that waits is told so.
    let before = world.script.seen().len();
    let refused = world
        .pipeline
        .fetch(get("https://down.enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(refused.outcome, Outcome::RobotsDenied);
    assert_eq!(refused.reason.as_deref(), Some(ROBOTS_UNAVAILABLE));
    assert_eq!(refused.robots, Some(UNREACHABLE));
    assert_eq!(refused.attempts.len(), 0);
    let down = world
        .backend
        .robots_entry("https://down.enforced.example")
        .unwrap();
    assert_eq!(
        (down.status, down.body.clone(), down.unreachable),
        (Some(503), None, true)
    );
    assert_eq!(down.expires_at - down.fetched_at, Span::hours(1));
    assert_eq!(refused.retry_at, Some(down.expires_at));
    assert_eq!(world.script.seen().len(), before + 1);

    // Inside the hour nobody asks again, and a job's page waits for it.
    tokio::time::sleep(Duration::from_mins(20)).await;
    let again = world
        .pipeline
        .fetch(get("https://down.enforced.example/b"))
        .await
        .unwrap();
    assert_eq!(again.reason.as_deref(), Some(ROBOTS_UNAVAILABLE));
    assert_eq!(again.robots, Some(UNREACHABLE));
    let (_keep, stop) = never();
    let waiting = world
        .pipeline
        .fetch_for_job(
            RequestId::new(),
            get("https://down.enforced.example/c"),
            None,
            stop.clone(),
        )
        .await
        .unwrap();
    let JobFetch::Deferred(deferred) = waiting else {
        panic!("the page did not wait: {waiting:?}");
    };
    assert_eq!(deferred.reason, Refusal::RobotsUnavailable);
    assert_eq!(deferred.retry_at, Some(down.expires_at));
    assert_eq!(world.script.seen().len(), before + 1);

    // After it the same scout asks once more.
    tokio::time::sleep(Duration::from_mins(41)).await;
    let later = world
        .pipeline
        .fetch(get("https://down.enforced.example/d"))
        .await
        .unwrap();
    assert_eq!(later.outcome, Outcome::RobotsDenied);
    assert_eq!(later.robots, Some(UNREACHABLE));
    let (files, _) = asked(&world);
    assert_eq!(files.len(), 3);
    assert_eq!(files[1].1, files[2].1);
    assert_eq!(files[2].0, "https://down.enforced.example/robots.txt");

    // Where the file is reported the page is fetched all the same, and
    // once the scout found the file unreachable the result says so.
    let first = world
        .pipeline
        .fetch(get("https://down.reported.example/a"))
        .await
        .unwrap();
    assert_eq!(first.outcome, Outcome::Ok);
    assert_eq!(first.robots, Some(UNASKED));
    tokio::time::sleep(Duration::from_mins(2)).await;
    let second = world
        .pipeline
        .fetch(get("https://down.reported.example/b"))
        .await
        .unwrap();
    assert_eq!(second.outcome, Outcome::Ok);
    assert_eq!(second.robots, Some(UNREACHABLE));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_scout_that_is_turned_away_cools_down_like_any_persona() {
    let mut world = World::new(4, SITES);
    robots_txt(&world, |_| blocked(Outcome::Forbidden, 1.0));

    let report = world
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::RobotsDenied);
    assert_eq!(report.reason.as_deref(), Some(ROBOTS_UNAVAILABLE));
    let (files, pages) = asked(&world);
    // Asked once: a file that did not come is not asked for from another
    // address.
    assert_eq!((files.len(), pages.len()), (1, 0));
    let scout = files[0].1;
    let entry = world
        .backend
        .robots_entry("https://enforced.example")
        .unwrap();
    assert!(entry.unreachable);
    assert_eq!(entry.status, Some(403));
    // The site refused one of our addresses: it cools down for the site,
    // and the block counts on the site's breaker.
    let state = world
        .backend
        .domain_state(scout, "enforced.example")
        .unwrap();
    assert!(
        state
            .cooldown_until
            .is_some_and(|until| until > world.now())
    );
    assert_eq!(state.last_outcome, Some(Outcome::Forbidden));
    assert!(world.backend.breaker("enforced.example").is_some());
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_may_ask_for_more_care_than_the_policy_and_never_for_less() {
    let mut world = World::new(4, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));
    let wishing = |url: &str, robots| FetchOrder { robots, ..get(url) };

    // The policy leaves the file alone: a request cannot have it enforced,
    // and is told so in place of a page that would not have been checked.
    let ignored = world
        .pipeline
        .fetch(wishing("https://quiet.example/a", RobotsWish::Enforce))
        .await
        .unwrap();
    assert_eq!(ignored.outcome, Outcome::RobotsDenied);
    assert_eq!(ignored.reason.as_deref(), Some("robots_ignored"));
    assert_eq!(ignored.robots, Some(UNASKED));
    // Asking for a report there asks nobody: nothing will ever be known.
    let nothing = world
        .pipeline
        .fetch(wishing("https://quiet.example/a", RobotsWish::Report))
        .await
        .unwrap();
    assert_eq!(nothing.outcome, Outcome::Ok);
    assert_eq!(nothing.robots, Some(UNASKED));
    assert_eq!(asked(&world).0, []);

    // The policy enforces: a request cannot ask for less.
    for wish in [RobotsWish::Report, RobotsWish::Ignore] {
        let refused = world
            .pipeline
            .fetch(wishing("https://enforced.example/private/a", wish))
            .await
            .unwrap();
        assert_eq!(refused.outcome, Outcome::RobotsDenied);
        assert_eq!(refused.reason.as_deref(), Some("robots_enforced"));
        assert_eq!(refused.attempts.len(), 0);
    }
    assert_eq!(world.script.seen().len(), 1);

    // The policy reports: a request may have the file enforced, and then
    // waits for the verdict like any enforced fetch.
    let enforced = world
        .pipeline
        .fetch(wishing(
            "https://reported.example/private/a",
            RobotsWish::Enforce,
        ))
        .await
        .unwrap();
    assert_eq!(enforced.outcome, Outcome::RobotsDenied);
    assert_eq!(enforced.reason, None);
    assert_eq!(
        enforced.robots.unwrap().matched_rule.as_deref(),
        Some("Disallow: /private")
    );
    assert_eq!(
        world.script.urls(),
        [
            "https://quiet.example/a",
            "https://reported.example/robots.txt"
        ]
    );
    // And one that wants no verdict gets none, with the file known or not.
    let unasked = world
        .pipeline
        .fetch(wishing(
            "https://reported.example/private/a",
            RobotsWish::Ignore,
        ))
        .await
        .unwrap();
    assert_eq!(unasked.outcome, Outcome::Ok);
    assert_eq!(unasked.robots, Some(UNASKED));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn nobody_scouts_a_site_that_every_persona_reads() {
    // One persona: it reads, so it cannot ask.
    let mut alone = World::new(1, SITES);
    robots_txt(&alone, |url| file(url, 200, RULES));
    let report = alone
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::RobotsDenied);
    assert_eq!(report.reason.as_deref(), Some(ROBOTS_NO_SCOUT));
    assert!(report.retry_at.is_some());
    assert_eq!(alone.script.seen().len(), 0);
    assert_eq!(alone.backend.scout_of("enforced.example"), None);
    // Where the file is only reported, the page is fetched without it.
    let reported = alone
        .pipeline
        .fetch(get("https://reported.example/a"))
        .await
        .unwrap();
    assert_eq!(reported.outcome, Outcome::Ok);
    assert_eq!(reported.robots, Some(UNASKED));
    tokio::time::sleep(Duration::from_mins(5)).await;
    assert_eq!(alone.script.urls(), ["https://reported.example/a"]);
    alone.shut_down().await;

    // Three personas that have all read a page of the site.
    let mut world = World::new(3, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));
    for number in 0..3 {
        world
            .backend
            .with_domain_state(id(number), "shop.enforced.example", |state| {
                state.last_request_at = Some(common::noon());
            });
    }
    let (_keep, stop) = never();
    let waiting = world
        .pipeline
        .fetch_for_job(
            RequestId::new(),
            get("https://enforced.example/a"),
            None,
            stop,
        )
        .await
        .unwrap();
    let JobFetch::Deferred(deferred) = waiting else {
        panic!("the page did not wait: {waiting:?}");
    };
    assert_eq!(deferred.reason, Refusal::RobotsUnavailable);
    assert_eq!(deferred.retry_at, Some(world.now() + Span::hours(1)));
    assert_eq!(world.script.seen().len(), 0);

    // A fourth persona that never read it can ask.
    world
        .backend
        .put_persona(common::persona(3, true), true, common::network(3));
    tokio::time::sleep(Duration::from_secs(10)).await;
    let report = world
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok);
    let (files, pages) = asked(&world);
    assert_eq!(
        files,
        [("https://enforced.example/robots.txt".to_owned(), id(3))]
    );
    assert_ne!(pages[0].1, id(3));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_persona_that_has_not_read_a_site_for_a_month_may_scout_it() {
    // Both personas read the site; one of them 29 days ago. Nobody asks.
    let mut world = World::new(2, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));
    let read = |world: &World, number: u8, days: i64| {
        let at = world.now() - Span::days(days);
        world
            .backend
            .with_domain_state(id(number), "enforced.example", |state| {
                state.last_request_at = Some(at);
            });
    };
    read(&world, 0, 29);
    read(&world, 1, 1);
    let report = world
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(report.reason.as_deref(), Some(ROBOTS_NO_SCOUT));
    assert_eq!(world.script.seen().len(), 0);
    world.shut_down().await;

    // A day later than that its reading is 30 days old
    // (`robots.scout_quarantine`): it may ask, and the other one reads.
    let mut world = World::new(2, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));
    read(&world, 0, 30);
    read(&world, 1, 1);
    let report = world
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok);
    let (files, pages) = asked(&world);
    assert_eq!(
        files,
        [("https://enforced.example/robots.txt".to_owned(), id(0))]
    );
    assert_eq!(pages[0].1, id(1));
    assert_eq!(world.backend.scout_of("enforced.example"), Some(id(0)));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn the_operator_gives_a_site_whose_scout_is_not_ready_another_one() {
    let mut world = World::new(3, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));
    let report = world
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(report.outcome, Outcome::Ok);
    let first = world.backend.scout_of("enforced.example").unwrap();
    let reader = asked(&world).1[0].1;

    // The scout's proxy is down: nothing of another origin of the site is
    // fetched, for nobody else may ask it.
    let number = (0..3).find(|number| id(*number) == first).unwrap();
    world.backend.put_persona(
        common::persona(number, true),
        false,
        common::network(number),
    );
    let closed = world
        .pipeline
        .fetch(get("https://www.enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(closed.outcome, Outcome::RobotsDenied);
    assert_eq!(closed.reason.as_deref(), Some(ROBOTS_UNAVAILABLE));

    // The operator gives the site another scout: neither the one it had
    // nor its reader. It asks from then on.
    let new = world
        .pipeline
        .replace_scout("enforced.example")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.previous, Some(first));
    assert_ne!(new.scout, first);
    assert_ne!(new.scout, reader);
    assert_eq!(world.backend.scout_of("enforced.example"), Some(new.scout));
    let open = world
        .pipeline
        .fetch(get("https://www.enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(open.outcome, Outcome::Ok);
    let (files, pages) = asked(&world);
    assert_eq!(
        files.last(),
        Some(&(
            "https://www.enforced.example/robots.txt".to_owned(),
            new.scout
        ))
    );
    assert_ne!(pages.last().unwrap().1, new.scout);

    // Nobody else may ask now (the first scout is not ready, the third
    // persona reads): the site keeps the scout it has.
    assert_eq!(
        world
            .pipeline
            .replace_scout("enforced.example")
            .await
            .unwrap(),
        None
    );
    assert_eq!(world.backend.scout_of("enforced.example"), Some(new.scout));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_crawl_is_told_what_is_known_and_reads_the_sitemaps_a_file_names() {
    let mut world = World::new(4, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));
    let url = |text: &str| Url::parse(text).unwrap();
    let need = || ScoutNeed {
        readers: vec![id(0)],
        readers_needed: 2,
        priority: Priority::Job,
        patience: None,
        level: Level::Human,
    };
    let (_keep, stop) = never();

    // Before anybody asked, nothing speaks against a page, and nothing is
    // asked for by looking.
    let private = url("https://enforced.example/private/a");
    assert_eq!(
        world
            .pipeline
            .robots_peek(&private, RobotsWish::Policy)
            .await
            .unwrap(),
        Peek::Go
    );
    assert_eq!(world.script.seen().len(), 0);

    // A site whose policy leaves the file alone is not asked for it.
    assert_eq!(
        world
            .pipeline
            .robots_read(
                &url("https://quiet.example/"),
                RobotsWish::Policy,
                need(),
                stop.clone()
            )
            .await
            .unwrap(),
        RobotsRead::NotAsked
    );
    assert_eq!(world.script.seen().len(), 0);

    // The file is read by the site's scout, who is none of the crawl's
    // readers, and names the site's sitemaps.
    let read = world
        .pipeline
        .robots_read(&private, RobotsWish::Policy, need(), stop.clone())
        .await
        .unwrap();
    assert_eq!(
        read,
        RobotsRead::Read {
            found: true,
            sitemaps: vec!["https://enforced.example/map.xml".to_owned()],
        }
    );
    let (files, _) = asked(&world);
    assert_eq!(files.len(), 1);
    let scout = files[0].1;
    assert_ne!(scout, id(0));
    // Read again, nothing is asked.
    world
        .pipeline
        .robots_read(&private, RobotsWish::Policy, need(), stop.clone())
        .await
        .unwrap();
    assert_eq!(world.script.seen().len(), 1);

    // Now a crawl is told what the file disallows before it asks.
    let Peek::Denied(verdict) = world
        .pipeline
        .robots_peek(&private, RobotsWish::Policy)
        .await
        .unwrap()
    else {
        panic!("the address is disallowed");
    };
    assert_eq!(verdict.matched_rule.as_deref(), Some("Disallow: /private"));
    assert_eq!(
        world
            .pipeline
            .robots_peek(&url("https://enforced.example/open"), RobotsWish::Policy)
            .await
            .unwrap(),
        Peek::Go
    );
    // Where the file is only reported nothing is denied; a crawl that asks
    // to have it enforced is told.
    world
        .pipeline
        .robots_read(
            &url("https://reported.example/"),
            RobotsWish::Policy,
            need(),
            stop.clone(),
        )
        .await
        .unwrap();
    let reported = url("https://reported.example/private/a");
    assert_eq!(
        world
            .pipeline
            .robots_peek(&reported, RobotsWish::Policy)
            .await
            .unwrap(),
        Peek::Go
    );
    assert!(matches!(
        world
            .pipeline
            .robots_peek(&reported, RobotsWish::Enforce)
            .await
            .unwrap(),
        Peek::Denied(_)
    ));

    // The scout is the one who asks for the site's sitemaps too, whatever
    // the policy says of who may read the site; a page it would be sent
    // to read goes to somebody else.
    assert_eq!(
        world
            .pipeline
            .scout_for(&url("https://www.enforced.example/map.xml"), &[], 1)
            .await
            .unwrap(),
        Some(scout)
    );
    let sitemap = FetchOrder {
        pin: Some(scout),
        scout: true,
        max_retries: Some(0),
        ..get("https://enforced.example/map.xml")
    };
    let report = finished(
        world
            .pipeline
            .fetch_for_job(RequestId::new(), sitemap, None, stop)
            .await
            .unwrap(),
    );
    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(report.attempts[0].persona_id, Some(scout));
    assert_eq!(report.robots, None);
    for number in 0..8 {
        let page = world
            .pipeline
            .fetch(get(&format!("https://enforced.example/{number}")))
            .await
            .unwrap();
        assert_ne!(page.attempts[0].persona_id, Some(scout));
    }
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_scout_may_be_a_persona_the_policy_keeps_from_reading_the_site() {
    // Only Firefox reads the site, and every persona is a Chrome: nobody
    // reads it, and anybody may scout it.
    let rules = "defaults: { pacing: fast, active_hours: ignore }\n\
                 domains:\n  enforced.example: { robots: enforce, engines: [firefox] }\n";
    let mut world = World::new(2, rules);
    robots_txt(&world, |url| file(url, 200, RULES));

    let report = world
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    // The file was asked for, by a persona that costs the site no reader;
    // the page itself has nobody.
    let (files, pages) = asked(&world);
    assert_eq!((files.len(), pages.len()), (1, 0));
    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some("no_persona"));
    assert_eq!(report.robots, Some(allowed(Some(30.0))));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_jobs_scout_is_a_jobs_request_and_waits_out_a_paused_site() {
    // Review of F109: the scout a job's page waits for runs as the job's
    // fetch does. A site that turned the first scout away is paused for six
    // hours by its breaker; an hour on, when the file may be asked for
    // again, the next scout's request is refused `domain_paused`. A job's
    // scout waits that out with the job: nothing of it is concluded, and the
    // page waits until the site is open again. Before, the scout's request
    // ended `provider_error` with a stored result, and the page waited for
    // a file nobody had asked for.
    let sites = "defaults: { pacing: fast, active_hours: ignore }\n\
                 domains:\n  enforced.example: { robots: enforce, breaker_threshold: 1, \
                 breaker_pause: 6h }\n";
    let mut world = World::new(4, sites);
    robots_txt(&world, |_| blocked(Outcome::Forbidden, 1.0));
    let first = world
        .pipeline
        .fetch(get("https://enforced.example/a"))
        .await
        .unwrap();
    assert_eq!(first.reason.as_deref(), Some(ROBOTS_UNAVAILABLE));
    let breaker = world.backend.breaker("enforced.example").unwrap();
    let reopens = breaker.paused_until.unwrap();
    assert!(reopens > world.now() + Span::hours(5), "{reopens}");

    tokio::time::sleep(Duration::from_mins(61)).await;
    let concluded = |world: &World| {
        let scouting: Vec<RequestId> = world
            .backend
            .requests()
            .into_iter()
            .filter(|request| request.url == "https://enforced.example/robots.txt")
            .map(|request| request.id)
            .collect();
        world
            .backend
            .records()
            .iter()
            .filter(|record| scouting.contains(&record.request_id))
            .count()
    };
    let before = concluded(&world);
    let (_keep, stop) = never();
    let waiting = world
        .pipeline
        .fetch_for_job(
            RequestId::new(),
            get("https://enforced.example/b"),
            None,
            stop,
        )
        .await
        .unwrap();
    let JobFetch::Deferred(deferred) = waiting else {
        panic!("the page did not wait: {waiting:?}");
    };
    assert_eq!(deferred.reason, Refusal::DomainPaused);
    assert_eq!(deferred.retry_at, Some(reopens));
    assert_eq!(
        concluded(&world),
        before,
        "the scout's request was concluded"
    );
    // Nobody asked the paused site for anything.
    let (files, pages) = asked(&world);
    assert_eq!((files.len(), pages.len()), (1, 0));
    world.shut_down().await;
}

/// What the plain client of level 0 brings back of a file: no page, the
/// body as the site sent it in what the http path fetched.
fn plain_file(url: &str, status: u16, body: &str) -> AttemptOutput {
    let mut output = file(url, status, body);
    output.page = None;
    output.fetched = Some(Box::new(Fetched {
        status,
        final_url: Url::parse(url).unwrap(),
        http_version: "http/1.1",
        headers: vec![Header {
            name: "content-type".to_owned(),
            value: "text/plain".to_owned(),
        }],
        body: body.as_bytes().to_vec().into(),
        truncated: false,
        hops: Vec::new(),
        downgraded: false,
        timing: Timing::default(),
        subresources: None,
        text_cache: kage_http::TextCache::default(),
    }));
    output
}

/// QA round one's api-01 and crawl-02 (F99): the file a level 0 request
/// asked for first was kept empty for a day, and `enforce` enforced
/// nothing at any level. It is kept as the site sent it: the plain client
/// asks for it once, a page it disallows is not asked for at level 0 or in
/// a browser, its `Crawl-delay` is kept, and a crawl reads its sitemaps.
#[tokio::test(start_paused = true)]
async fn a_robots_txt_a_plain_client_brought_is_kept_as_sent_and_enforced_at_every_level() {
    let mut world = World::new(4, SITES);
    world.script.when_plain(|job| {
        (job.url.path() == "/robots.txt").then(|| plain_file(job.url.as_str(), 200, RULES))
    });
    // A browser would bring back a file that allows everything: it is
    // never asked.
    robots_txt(&world, |url| file(url, 200, "User-agent: *\nAllow: /\n"));
    let plain = |path: &str| FetchOrder {
        level: Level::Plain,
        ..get(&format!("https://enforced.example{path}"))
    };

    let first = world.pipeline.fetch(plain("/private/a")).await.unwrap();
    assert_eq!(first.outcome, Outcome::RobotsDenied);
    assert_eq!(first.attempts, []);
    assert_eq!(
        first.robots,
        Some(RobotsVerdict {
            fetched: true,
            allowed: false,
            matched_rule: Some("Disallow: /private".to_owned()),
            crawl_delay: Some(30.0),
        })
    );
    let entry = world
        .backend
        .robots_entry("https://enforced.example")
        .unwrap();
    assert_eq!(entry.body.as_deref(), Some(RULES));
    assert_eq!(entry.status, Some(200));
    assert!(!entry.unreachable);

    // In a browser too, what the file disallows is not asked for, and no
    // browser asked for the file.
    let browser = world
        .pipeline
        .fetch(get("https://enforced.example/private/b"))
        .await
        .unwrap();
    assert_eq!(browser.outcome, Outcome::RobotsDenied);
    assert!(world.script.seen().is_empty());

    // What it allows is read, with its verdict, at the pace it asks for.
    let open = world.pipeline.fetch(plain("/a")).await.unwrap();
    assert_eq!(open.outcome, Outcome::Ok);
    assert_eq!(open.robots, Some(allowed(Some(30.0))));
    let next = world.pipeline.fetch(plain("/b")).await.unwrap();
    assert_eq!(next.outcome, Outcome::Ok);
    let paths: Vec<String> = world
        .script
        .plain_seen()
        .iter()
        .map(|seen| seen.job.url.path().to_owned())
        .collect();
    assert_eq!(paths, ["/robots.txt", "/a", "/b"]);
    let seen = world.script.plain_seen();
    let gap = seen[2].at - seen[1].at;
    assert!(gap >= Span::seconds(30), "{gap}");

    // A crawl at level 0 reads the sitemaps it names, from what is kept.
    let (_keep, stop) = never();
    let read = world
        .pipeline
        .robots_read(
            &Url::parse("https://enforced.example/").unwrap(),
            RobotsWish::Policy,
            ScoutNeed {
                readers: Vec::new(),
                readers_needed: 1,
                priority: Priority::Job,
                patience: None,
                level: Level::Plain,
            },
            stop,
        )
        .await
        .unwrap();
    assert_eq!(
        read,
        RobotsRead::Read {
            found: true,
            sitemaps: vec!["https://enforced.example/map.xml".to_owned()],
        }
    );
    assert_eq!(world.script.plain_seen().len(), 3);
    world.shut_down().await;
}

/// The `revalidated` field of every "a robots.txt was asked for" line
/// written while it is the thread's subscriber.
#[derive(Clone, Default)]
struct AskedLines(std::sync::Arc<std::sync::Mutex<Vec<bool>>>);

impl AskedLines {
    fn read(&self) -> Vec<bool> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// One line's message and `revalidated` field.
#[derive(Default)]
struct AskedLine {
    message: String,
    revalidated: Option<bool>,
}

impl tracing::field::Visit for AskedLine {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        if field.name() == "revalidated" {
            self.revalidated = Some(value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for AskedLines {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = AskedLine::default();
        event.record(&mut line);
        if line.message == "a robots.txt was asked for" {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(line.revalidated.unwrap_or_default());
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

/// F209 (F90 at level 0, QA round C's tls level 0 check): a `304` to the
/// scout once the kept file expired keeps the file and its rules for
/// another day; before, the file was dropped and the site was
/// `robots_unavailable` for an hour. A 304 with nothing kept tells
/// nothing: the file cannot be had, as before, and the log does not say
/// it was revalidated (#73's review).
#[tokio::test(start_paused = true)]
async fn a_304_to_the_scout_keeps_the_file_it_had_for_another_ttl() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU16, Ordering};

    let lines = AskedLines::default();
    let _lines = tracing::subscriber::set_default(lines.clone());
    // While one dispatcher is registered, tracing-core takes a callsite's
    // interest from the subscriber of the thread that reaches it first and
    // keeps it for every thread: a test running beside this one that writes
    // the line first, with no subscriber, would leave it off here. With a
    // second one alive every registration asks all of them.
    let _second = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let mut world = World::new(4, SITES);
    let answering = Arc::new(AtomicU16::new(200));
    let status = Arc::clone(&answering);
    world.script.when_plain(move |job| {
        (job.url.path() == "/robots.txt").then(|| match status.load(Ordering::SeqCst) {
            304 => plain_file(job.url.as_str(), 304, ""),
            code => plain_file(job.url.as_str(), code, RULES),
        })
    });
    let plain = |url: &str| FetchOrder {
        level: Level::Plain,
        ..get(url)
    };

    let first = world
        .pipeline
        .fetch(plain("https://enforced.example/private/a"))
        .await
        .unwrap();
    assert_eq!(first.outcome, Outcome::RobotsDenied);
    let kept = world
        .backend
        .robots_entry("https://enforced.example")
        .unwrap();

    // The kept answer runs out (a day, moved by up to a tenth) and the
    // site answers the scout 304.
    tokio::time::advance(Duration::from_hours(27)).await;
    assert!(world.now() > kept.expires_at);
    answering.store(304, Ordering::SeqCst);
    let again = world
        .pipeline
        .fetch(plain("https://enforced.example/private/b"))
        .await
        .unwrap();
    assert_eq!(again.outcome, Outcome::RobotsDenied, "{again:?}");
    assert_eq!(
        again
            .robots
            .as_ref()
            .and_then(|v| v.matched_rule.as_deref()),
        Some("Disallow: /private")
    );
    let renewed = world
        .backend
        .robots_entry("https://enforced.example")
        .unwrap();
    assert_eq!(renewed.body.as_deref(), Some(RULES));
    assert_eq!(renewed.status, Some(200));
    assert!(!renewed.unreachable);
    assert_eq!(renewed.fetched_at, world.now());
    assert!(renewed.expires_at - world.now() > Span::hours(21));
    let files: Vec<String> = world
        .script
        .plain_seen()
        .iter()
        .map(|seen| seen.job.url.as_str().to_owned())
        .collect();
    assert_eq!(
        files,
        [
            "https://enforced.example/robots.txt",
            "https://enforced.example/robots.txt"
        ]
    );

    // An origin with nothing kept: the 304 is no answer.
    let unknown = world
        .pipeline
        .fetch(plain("https://enforced.example:8443/a"))
        .await
        .unwrap();
    assert_eq!(unknown.reason.as_deref(), Some(ROBOTS_UNAVAILABLE));
    let none = world
        .backend
        .robots_entry("https://enforced.example:8443")
        .unwrap();
    assert!(none.unreachable);
    assert_eq!(none.status, Some(304));
    // Revalidated is the kept file's 304 only.
    assert_eq!(lines.read(), [false, true, false]);
    world.shut_down().await;
}

/// A flow at level 3 through the given addresses, one `goto` each.
fn flow_through(urls: &[&str]) -> FetchOrder {
    let actions: Vec<FlowAction> = urls
        .iter()
        .map(|url| FlowAction::Goto {
            human: None,
            timeout_ms: None,
            url: (*url).to_owned(),
        })
        .collect();
    FetchOrder {
        level: Level::Human,
        flow: Some(FlowPlan {
            actions,
            typos: false,
        }),
        ..FetchOrder::get(Url::parse(urls[0]).unwrap(), Priority::Sync)
    }
}

#[tokio::test(start_paused = true)]
async fn every_goto_of_a_flow_meets_the_file_of_its_own_address() {
    let mut world = World::new(4, SITES);
    robots_txt(&world, |url| file(url, 200, RULES));
    let disallowed = RobotsVerdict {
        fetched: true,
        allowed: false,
        matched_rule: Some("Disallow: /private".to_owned()),
        crawl_delay: Some(30.0),
    };

    // A later goto to what the enforced file disallows, on the flow's own
    // site: nothing of the flow is asked for, and the result names the
    // rule of that address (jobs-05).
    let same_site = world
        .pipeline
        .fetch(flow_through(&[
            "https://enforced.example/landing",
            "https://enforced.example/private/later",
        ]))
        .await
        .unwrap();
    assert_eq!(same_site.outcome, Outcome::RobotsDenied);
    assert!(same_site.attempts.is_empty() && same_site.persona.is_none());
    assert_eq!(same_site.robots, Some(disallowed.clone()));
    let (_, pages) = asked(&world);
    assert_eq!(pages, []);

    // The same from a site whose file nobody asks for: the later site's
    // own policy enforces its file.
    let other_site = world
        .pipeline
        .fetch(flow_through(&[
            "https://free.example/start",
            "https://enforced.example/private/second",
        ]))
        .await
        .unwrap();
    assert_eq!(other_site.outcome, Outcome::RobotsDenied);
    assert_eq!(other_site.attempts.len(), 0);
    assert_eq!(other_site.robots, Some(disallowed.clone()));
    // The later site named with its root dot is the same site (F104): its
    // file, asked for once, disallows the address.
    let dotted = world
        .pipeline
        .fetch(flow_through(&[
            "https://free.example/start",
            "https://enforced.example./private/third",
        ]))
        .await
        .unwrap();
    assert_eq!(dotted.outcome, Outcome::RobotsDenied);
    assert_eq!(dotted.attempts.len(), 0);
    assert_eq!(dotted.robots, Some(disallowed));
    let (files, pages) = asked(&world);
    assert_eq!(
        files
            .iter()
            .map(|(url, _)| url.as_str())
            .collect::<Vec<_>>(),
        ["https://enforced.example/robots.txt"]
    );
    assert_eq!(pages, []);

    // A flow whose every address the files allow runs, in one attempt, by
    // a persona that is the scout of none of its sites. It ends on the
    // later site.
    world.script.then(page_with(
        "https://enforced.example/open",
        &serde_json::json!({}),
    ));
    let allowed_flow = world
        .pipeline
        .fetch(flow_through(&[
            "https://free.example/start",
            "https://enforced.example/open",
        ]))
        .await
        .unwrap();
    assert_eq!(
        allowed_flow.outcome,
        Outcome::Ok,
        "{:?}",
        allowed_flow.reason
    );
    let [attempt] = allowed_flow.attempts.as_slice() else {
        panic!("one attempt, not {:?}", allowed_flow.attempts);
    };
    assert_ne!(
        attempt.persona_id,
        world.backend.scout_of("enforced.example")
    );
    // Its result reports the verdict of the address it ended on, under
    // that site's policy, with its `Crawl-delay`; not the first site's,
    // whose file nobody asks for (F186, flows-01). The later site's file
    // was asked for once.
    assert_eq!(allowed_flow.robots, Some(allowed(Some(30.0))));
    let (files, pages) = asked(&world);
    assert_eq!(files.len(), 1);
    assert_eq!(pages.len(), 1);

    // A flow a click took to what the later site's file disallows (a
    // click is no `goto`, and nothing stopped it): its result never says
    // the address is allowed.
    world.script.then(page_with(
        "https://enforced.example/private/clicked",
        &serde_json::json!({}),
    ));
    let clicked = world
        .pipeline
        .fetch(flow_through(&["https://free.example/start"]))
        .await
        .unwrap();
    assert_eq!(clicked.outcome, Outcome::Ok, "{:?}", clicked.reason);
    let verdict = clicked.robots.unwrap();
    assert!(verdict.fetched && !verdict.allowed, "{verdict:?}");
    assert_eq!(verdict.matched_rule.as_deref(), Some("Disallow: /private"));
    // One that ended where it began reports its own address's.
    let stayed = world
        .pipeline
        .fetch(flow_through(&["https://free.example/start"]))
        .await
        .unwrap();
    assert_eq!(stayed.robots, Some(UNASKED));
    world.shut_down().await;
}
