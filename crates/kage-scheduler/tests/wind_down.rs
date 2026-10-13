//! What happens to a page that is being read when the server stops
//! (follow-up F39), with a scripted executor on a paused clock: it is
//! finished and asked for once; what outlasts the wait is cut and written
//! down as cut, so that the record says the site was asked.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::time::Duration;

use common::{DEFAULT_DOMAINS, World};
use kage_core::domain::RenderMode;
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::Refusal;
use kage_core::schedule::queue::Priority;
use kage_scheduler::{FetchOrder, INTERRUPTED, JobFetch, Stop, Synchronous, WoundDown};
use tokio::sync::watch;
use url::Url;

const WAIT: Duration = Duration::from_secs(150);

fn page(url: &str, priority: Priority) -> FetchOrder {
    FetchOrder {
        render: RenderMode::Browser,
        ..FetchOrder::get(Url::parse(url).unwrap(), priority)
    }
}

/// A world whose pages take `took` to read, and a synchronous fetch of
/// `url` in it that outlasted its wall time: a carried fetch, with its
/// first attempt under way.
async fn carried(took: Duration, url: &str) -> (World, kage_scheduler::Running) {
    let world = World::with(
        3,
        true,
        DEFAULT_DOMAINS,
        &[("KAGE__SCHEDULER__SYNC_WALL_TIME", "5s")],
    );
    world.script.takes(took);
    let Synchronous::Overflowed(running) = world
        .pipeline
        .fetch_sync(page(url, Priority::Sync))
        .await
        .unwrap()
    else {
        panic!("a page of {took:?} outlasts a wall time of 5 s");
    };
    (world, running)
}

#[tokio::test(start_paused = true)]
async fn nothing_under_way_is_nothing_to_wait_for() {
    let mut world = World::new(3, DEFAULT_DOMAINS);
    assert_eq!(world.pipeline.attempts_under_way(), 0);
    let before = world.now();
    assert_eq!(
        world.pipeline.wind_down(WAIT).await,
        WoundDown {
            finished: 0,
            cut: 0
        }
    );
    assert_eq!(world.now(), before);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_page_that_is_being_read_when_the_server_stops_is_finished_and_asked_for_once() {
    let (mut world, running) = carried(Duration::from_secs(60), "https://shop.example/a").await;
    let request = running.request;
    assert_eq!(world.pipeline.attempts_under_way(), 1);
    let stopped_at = world.now();

    // The server stops: the scheduler grants nothing more, and the page
    // that is being read is waited for.
    world.shut_down().await;
    let wound = world.pipeline.wind_down(WAIT).await;
    assert_eq!(
        wound,
        WoundDown {
            finished: 1,
            cut: 0
        }
    );
    // It took what was left of the page, not the whole wait.
    let waited = (world.now() - stopped_at).to_std().unwrap();
    assert!(
        waited > Duration::from_secs(50) && waited < Duration::from_secs(56),
        "{waited:?}"
    );
    assert_eq!(world.pipeline.attempts_under_way(), 0);

    // The fetch ended as any fetch ends: its page is stored, with the one
    // attempt that got it, and the site was asked once.
    let JobFetch::Finished(report) = running.wait().await.unwrap() else {
        panic!("the fetch did not finish");
    };
    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(report.attempts.len(), 1);
    assert_eq!(report.attempts[0].reason, None);
    assert_eq!(world.script.seen().len(), 1);
    assert!(
        world
            .backend
            .records()
            .iter()
            .any(|record| record.request_id == request && record.outcome == Outcome::Ok)
    );
}

#[tokio::test(start_paused = true)]
async fn a_page_that_outlasts_the_wait_is_cut_and_written_down_as_cut() {
    let (mut world, running) = carried(Duration::from_secs(400), "https://shop.example/a").await;
    let request = running.request;
    let stopped_at = world.now();

    world.shut_down().await;
    let wound = world.pipeline.wind_down(WAIT).await;
    assert_eq!(
        wound,
        WoundDown {
            finished: 0,
            cut: 1
        }
    );
    assert_eq!((world.now() - stopped_at).to_std().unwrap(), WAIT);
    assert_eq!(world.pipeline.attempts_under_way(), 0);

    // The fetch is a job's: it is not ended, it is given back for whoever
    // takes the request up again.
    let JobFetch::Deferred(deferred) = running.wait().await.unwrap() else {
        panic!("a carried fetch that was cut is given back");
    };
    assert_eq!(
        (deferred.reason, deferred.retry_at),
        (Refusal::ShuttingDown, None)
    );
    assert!(
        world
            .backend
            .records()
            .iter()
            .all(|record| record.request_id != request)
    );
    // What was cut is on the record: the site was asked, by this persona,
    // from then until the cut, and nothing came of it that says anything
    // about the site or the persona.
    let attempts = world.backend.attempts();
    assert_eq!(attempts.len(), 1);
    let cut = &attempts[0];
    assert_eq!(cut.request_id, request);
    assert_eq!(cut.outcome, Outcome::ProviderError);
    assert_eq!(cut.reason.as_deref(), Some(INTERRUPTED));
    assert_eq!(cut.reason.as_deref(), Some("interrupted"));
    assert_eq!(cut.finished_at, world.now());
    assert!(cut.started_at < stopped_at);
    assert_eq!((cut.status, cut.block.clone()), (None, None));
    assert_eq!(world.script.seen().len(), 1);
    assert_eq!(world.backend.breaker("shop.example"), None);

    // Taken up again after the restart, the cut attempt counts: with a
    // budget of two retries the request has two attempts left, not three.
    let mut again = World::new(3, DEFAULT_DOMAINS);
    let (_keep, stop): (watch::Sender<bool>, Stop) = watch::channel(false);
    again.script.then(common::timed_out());
    again.script.then(common::timed_out());
    let taken_up = again
        .pipeline
        .fetch_for_job(
            request,
            page("https://shop.example/a", Priority::Job),
            Some(attempts.clone()),
            stop,
        )
        .await
        .unwrap();
    let JobFetch::Finished(report) = taken_up else {
        panic!("the request ends when its budget is spent");
    };
    assert_eq!(report.attempts.len(), 3);
    assert_eq!(report.attempts[0].reason.as_deref(), Some(INTERRUPTED));
    assert_eq!(again.script.seen().len(), 2);
    again.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_fetch_whose_caller_still_waits_is_answered_with_what_was_cut() {
    // Inside its wall time a fetch is nobody's job: cut, it ends, and its
    // result says why.
    let mut world = World::new(3, DEFAULT_DOMAINS);
    world.script.takes(Duration::from_secs(120));
    let fetching = tokio::spawn({
        let pipeline = world.pipeline.clone();
        async move {
            pipeline
                .fetch(page("https://shop.example/a", Priority::Sync))
                .await
        }
    });
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(world.pipeline.attempts_under_way(), 1);

    world.shut_down().await;
    let wound = world.pipeline.wind_down(Duration::from_secs(30)).await;
    assert_eq!(
        wound,
        WoundDown {
            finished: 0,
            cut: 1
        }
    );
    let report = fetching.await.unwrap().unwrap();
    assert_eq!(report.outcome, Outcome::ProviderError);
    assert_eq!(report.reason.as_deref(), Some(INTERRUPTED));
    assert_eq!(report.attempts.len(), 1);
    assert_eq!(world.script.seen().len(), 1);
    let stored = world.backend.records().pop().unwrap();
    assert_eq!(stored.request_id, report.request);
    assert_eq!(stored.reason.as_deref(), Some(INTERRUPTED));

    // A wait of nothing cuts at once; a second wind-down finds nothing.
    assert_eq!(
        world.pipeline.wind_down(Duration::ZERO).await,
        WoundDown {
            finished: 0,
            cut: 0
        }
    );
}

#[tokio::test(start_paused = true)]
async fn pages_that_end_inside_the_wait_and_pages_that_do_not_are_counted_apart() {
    let mut world = World::new(3, DEFAULT_DOMAINS);
    let start = |url: &'static str| {
        let pipeline = world.pipeline.clone();
        tokio::spawn(async move { pipeline.fetch(page(url, Priority::Sync)).await })
    };
    world.script.takes(Duration::from_secs(40));
    let quick = start("https://one.example/a");
    tokio::time::sleep(Duration::from_secs(5)).await;
    world.script.takes(Duration::from_secs(500));
    let slow = start("https://two.example/a");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(world.pipeline.attempts_under_way(), 2);

    world.shut_down().await;
    let wound = world.pipeline.wind_down(Duration::from_secs(60)).await;
    assert_eq!(
        wound,
        WoundDown {
            finished: 1,
            cut: 1
        }
    );
    assert_eq!(quick.await.unwrap().unwrap().outcome, Outcome::Ok);
    let cut = slow.await.unwrap().unwrap();
    assert_eq!(
        (cut.outcome, cut.reason.as_deref()),
        (Outcome::ProviderError, Some(INTERRUPTED))
    );
}
