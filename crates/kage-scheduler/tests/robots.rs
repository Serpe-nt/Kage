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
use common::{DEFAULT_DOMAINS, World, blocked, file, id, server_error};
use kage_core::domain::{Render, RenderMode, RobotsVerdict};
use kage_core::ids::{PersonaId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::Refusal;
use kage_core::schedule::queue::Priority;
use kage_scheduler::{
    Arrival, AttemptOutput, FetchOrder, FetchReport, JobFetch, Peek, ROBOTS_NO_SCOUT,
    ROBOTS_UNAVAILABLE, RobotsRead, RobotsWish, ScoutNeed, Stop,
};
use tokio::sync::watch;
use url::Url;

/// One site whose file is enforced, one whose file is reported, and every
/// other site left alone.
const SITES: &str = "defaults: { render: browser, pacing: fast, active_hours: ignore }\n\
                     domains:\n  enforced.example: { robots: enforce }\n  \
                     reported.example: { robots: report }\n";

/// A file that closes one directory and asks for half a minute between
/// two pages.
const RULES: &str = "User-agent: *\nDisallow: /private\nCrawl-delay: 30\n\
                     Sitemap: https://enforced.example/map.xml\n";

fn get(url: &str) -> FetchOrder {
    FetchOrder {
        render: RenderMode::Browser,
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
    assert_eq!(refused.robots, Some(UNASKED));
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
    let (files, _) = asked(&world);
    assert_eq!(files.len(), 3);
    assert_eq!(files[1].1, files[2].1);
    assert_eq!(files[2].0, "https://down.enforced.example/robots.txt");
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
    assert_eq!(report.attempts[0].persona_id, scout);
    assert_eq!(report.robots, None);
    for number in 0..8 {
        let page = world
            .pipeline
            .fetch(get(&format!("https://enforced.example/{number}")))
            .await
            .unwrap();
        assert_ne!(page.attempts[0].persona_id, scout);
    }
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_scout_may_be_a_persona_the_policy_keeps_from_reading_the_site() {
    // Only Firefox reads the site, and every persona is a Chrome: nobody
    // reads it, and anybody may scout it.
    let rules = "defaults: { render: browser, pacing: fast, active_hours: ignore }\n\
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
