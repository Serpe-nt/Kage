//! A fetch made for a job (spec §16.6), with a scripted executor on a
//! paused clock: what a job waits out instead of failing, the batch that
//! stays with its visitor, a stop that takes waiting work out of the queue
//! and never cuts an attempt, and a request that is taken up again after
//! the process that began it stopped.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::time::Duration;

use chrono::Duration as Span;
use common::{DEFAULT_DOMAINS, World, blocked, timed_out};
use kage_core::domain::{BreakerState, RenderMode};
use kage_core::ids::RequestId;
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::{Refusal, Visitors};
use kage_core::schedule::queue::Priority;
use kage_scheduler::{Deferred, FetchOrder, FetchReport, JobFetch, Stop};
use tokio::sync::watch;
use url::Url;

fn page(url: &str) -> FetchOrder {
    FetchOrder {
        render: RenderMode::Browser,
        ..FetchOrder::get(Url::parse(url).unwrap(), Priority::Job)
    }
}

/// A stop signal that never says stop, and its sender.
fn never() -> (watch::Sender<bool>, Stop) {
    watch::channel(false)
}

fn finished(fetch: JobFetch) -> FetchReport {
    match fetch {
        JobFetch::Finished(report) => *report,
        other => panic!("the fetch did not finish: {other:?}"),
    }
}

fn deferred(fetch: JobFetch) -> Deferred {
    match fetch {
        JobFetch::Deferred(deferred) => deferred,
        other => panic!("the fetch was not deferred: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_job_waits_out_a_paused_domain_instead_of_failing_its_page() {
    let rules = "defaults: { breaker_threshold: 2, breaker_pause: 30m, retries: 1 }\ndomains: {}\n";
    let mut world = World::new(8, rules);
    let (_keep, stop) = never();
    world
        .script
        .then(blocked(Outcome::Forbidden, 1.0))
        .then(blocked(Outcome::Forbidden, 1.0));
    let first = finished(
        world
            .pipeline
            .fetch_for_job(
                RequestId::new(),
                page("https://wall.example/a"),
                None,
                stop.clone(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(first.outcome, Outcome::Forbidden);
    let breaker = world.backend.breaker("wall.example").unwrap();
    assert_eq!(breaker.state, BreakerState::Open);

    // The next page of the job is not attempted and not failed: it is told
    // when the pause ends, its request is stored and nothing else is.
    let second = RequestId::new();
    let waiting = deferred(
        world
            .pipeline
            .fetch_for_job(second, page("https://wall.example/b"), None, stop.clone())
            .await
            .unwrap(),
    );
    assert_eq!(waiting.reason, Refusal::DomainPaused);
    assert_eq!(waiting.retry_at, breaker.paused_until);
    assert!(
        world
            .backend
            .requests()
            .iter()
            .any(|request| request.id == second)
    );
    assert!(
        world
            .backend
            .records()
            .iter()
            .all(|record| record.request_id != second)
    );
    assert_eq!(world.script.seen().len(), 2);

    // Asked again under the same request after the pause, it is the
    // breaker's trial, and it is served.
    tokio::time::sleep(Duration::from_mins(31)).await;
    let served = finished(
        world
            .pipeline
            .fetch_for_job(
                second,
                page("https://wall.example/b"),
                Some(Vec::new()),
                stop.clone(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(served.request, second);
    assert_eq!(served.outcome, Outcome::Ok);
    assert_eq!(served.attempts.len(), 1);
    // Stored once, although it was begun twice.
    assert_eq!(
        world
            .backend
            .requests()
            .iter()
            .filter(|request| request.id == second)
            .count(),
        1
    );
    assert_eq!(
        world.backend.breaker("wall.example").unwrap().state,
        BreakerState::Closed
    );

    // While egress is unsafe a job waits too, and is not told for how long.
    world.backend.set_egress_unsafe(true);
    let held = deferred(
        world
            .pipeline
            .fetch_for_job(
                RequestId::new(),
                page("https://open.example/a"),
                None,
                stop.clone(),
            )
            .await
            .unwrap(),
    );
    assert_eq!((held.reason, held.retry_at), (Refusal::EgressUnsafe, None));
    world.backend.set_egress_unsafe(false);
    world.scheduler.set_bandwidth_stop(true);
    let held = deferred(
        world
            .pipeline
            .fetch_for_job(
                RequestId::new(),
                page("https://open.example/a"),
                None,
                stop.clone(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(held.reason, Refusal::BandwidthExhausted);
    world.scheduler.set_bandwidth_stop(false);

    // What no waiting changes is an answer, as for any fetch: nobody here
    // has a firefox.
    let impossible = FetchOrder {
        engines: vec![kage_core::domain::Engine::Firefox],
        ..page("https://open.example/b")
    };
    let refused = finished(
        world
            .pipeline
            .fetch_for_job(RequestId::new(), impossible, None, stop)
            .await
            .unwrap(),
    );
    assert_eq!(refused.outcome, Outcome::ProviderError);
    assert_eq!(refused.reason.as_deref(), Some("no_persona"));
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_batch_stays_with_its_visitor_and_is_told_when_it_is_back() {
    let mut world = World::with(8, false, DEFAULT_DOMAINS, &[]);
    let (_keep, stop) = never();
    world.script.takes(Duration::from_secs(20));
    let mut visitors = Visitors {
        known: Vec::new(),
        max: 1,
    };
    let patience = Duration::from_mins(5);
    let mut pages = 0;
    let mut waits = Vec::new();

    // Forty pages of one site, one at a time, as the job worker asks.
    while pages < 40 {
        let order = FetchOrder {
            visitors: Some(visitors.clone()),
            patience: Some(patience),
            ..page(&format!("https://shop.example/{pages}"))
        };
        match world
            .pipeline
            .fetch_for_job(RequestId::new(), order, None, stop.clone())
            .await
            .unwrap()
        {
            JobFetch::Finished(report) => {
                assert_eq!(report.outcome, Outcome::Ok);
                visitors.known = vec![report.persona.as_ref().unwrap().id];
                pages += 1;
            }
            JobFetch::Deferred(wait) => {
                // Its visitor is on a long break: nobody is sent in its
                // place, and the job is told when the visitor is back.
                assert_eq!(wait.reason, Refusal::NoPersona);
                let back = wait.retry_at.unwrap();
                assert!(back > world.now() + Span::from_std(patience).unwrap());
                waits.push(back - world.now());
                tokio::time::sleep((back - world.now()).to_std().unwrap()).await;
            }
            JobFetch::Stopped => panic!("nothing said stop"),
        }
    }

    // One address read all forty, in more than one sitting.
    let readers: std::collections::BTreeSet<_> = world.script.personas().into_iter().collect();
    assert_eq!(readers.len(), 1);
    assert_ne!(waits, Vec::new());
    assert!(waits.iter().all(|wait| *wait < Span::hours(6)));

    // Blocked, it is no longer waited for: another persona retries the
    // page (D38) and is the visitor from then on.
    world.script.then(blocked(Outcome::Forbidden, 1.0));
    tokio::time::sleep(Duration::from_hours(3)).await;
    let first = visitors.known[0];
    let order = FetchOrder {
        visitors: Some(visitors.clone()),
        patience: Some(patience),
        ..page("https://shop.example/blocked")
    };
    let report = finished(
        world
            .pipeline
            .fetch_for_job(RequestId::new(), order, None, stop.clone())
            .await
            .unwrap(),
    );
    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(report.attempts.len(), 2);
    assert_eq!(report.attempts[0].persona_id, first);
    let second = report.persona.as_ref().unwrap().id;
    assert_ne!(second, first);
    // The next page names both; the one that cools does not count, so the
    // batch has its one visitor and stays with it.
    let order = FetchOrder {
        visitors: Some(Visitors {
            known: vec![first, second],
            max: 1,
        }),
        patience: Some(Duration::from_hours(2)),
        ..page("https://shop.example/next")
    };
    let report = finished(
        world
            .pipeline
            .fetch_for_job(RequestId::new(), order, None, stop)
            .await
            .unwrap(),
    );
    assert_eq!(report.persona.as_ref().unwrap().id, second);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_stop_takes_waiting_work_out_of_the_queue_and_never_cuts_an_attempt() {
    // One persona: the second page has to wait for the first.
    let mut world = World::with(1, false, DEFAULT_DOMAINS, &[]);
    world.script.takes(Duration::from_secs(30));
    let (stopper, stop) = never();
    let first = RequestId::new();
    let second = RequestId::new();
    let running = {
        let pipeline = world.pipeline.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            pipeline
                .fetch_for_job(first, page("https://shop.example/1"), None, stop)
                .await
        })
    };
    tokio::time::sleep(Duration::from_secs(5)).await;
    let waiting = {
        let pipeline = world.pipeline.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            pipeline
                .fetch_for_job(second, page("https://shop.example/2"), None, stop)
                .await
        })
    };
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(world.scheduler.depth(Priority::Job), 1);

    stopper.send(true).unwrap();

    // The page that waited is out of the queue at once, with nothing
    // concluded; the page in flight finishes and is stored.
    assert!(matches!(waiting.await.unwrap().unwrap(), JobFetch::Stopped));
    assert_eq!(world.scheduler.depth(Priority::Job), 0);
    let report = finished(running.await.unwrap().unwrap());
    assert_eq!(report.outcome, Outcome::Ok);
    assert_eq!(world.script.seen().len(), 1);
    let records = world.backend.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].request_id, first);

    // What was stopped is ended by its job: a result that says why.
    let ended = world
        .pipeline
        .abandon(
            second,
            page("https://shop.example/2"),
            Vec::new(),
            "cancelled",
        )
        .await
        .unwrap();
    assert_eq!(ended.request, second);
    assert_eq!(ended.outcome, Outcome::ProviderError);
    assert_eq!(ended.reason.as_deref(), Some("cancelled"));
    assert_eq!(ended.attempts, []);
    let stored = world.backend.records();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1].reason.as_deref(), Some("cancelled"));

    // A signal whose sender is gone never stops anything.
    let (gone, stop) = never();
    drop(gone);
    let report = finished(
        world
            .pipeline
            .fetch_for_job(RequestId::new(), page("https://shop.example/3"), None, stop)
            .await
            .unwrap(),
    );
    assert_eq!(report.outcome, Outcome::Ok);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_that_is_taken_up_again_keeps_its_attempts_and_its_budget() {
    let mut world = World::with(6, false, DEFAULT_DOMAINS, &[]);
    let (_keep, stop) = never();
    let request = RequestId::new();
    // Before the stop: the first persona was blocked, the retry's attempt
    // never came back (the process went away with it).
    world.script.then(blocked(Outcome::Forbidden, 1.0));
    let before = {
        let order = FetchOrder {
            max_retries: Some(0),
            ..page("https://shop.example/a")
        };
        finished(
            world
                .pipeline
                .fetch_for_job(request, order, None, stop.clone())
                .await
                .unwrap(),
        )
    };
    assert_eq!(before.attempts.len(), 1);
    let blocked_persona = before.attempts[0].persona_id;
    let earlier = world.backend.attempts();
    assert_eq!(earlier.len(), 1);

    // Taken up again with the policy's two retries: two attempts are left,
    // by other personas, numbered on, and the first one's block still
    // keeps its network away.
    world.script.then(timed_out());
    let after = finished(
        world
            .pipeline
            .fetch_for_job(
                request,
                page("https://shop.example/a"),
                Some(earlier.clone()),
                stop.clone(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(after.request, request);
    assert_eq!(after.outcome, Outcome::Ok);
    assert_eq!(
        after
            .attempts
            .iter()
            .map(|attempt| attempt.n)
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(after.attempts[0], earlier[0]);
    let later: Vec<_> = after.attempts[1..]
        .iter()
        .map(|attempt| attempt.persona_id)
        .collect();
    assert!(!later.contains(&blocked_persona));
    assert_ne!(later[0], later[1]);
    assert_eq!(world.backend.attempts().len(), 3);

    // A request whose budget was used up before the stop ends with what
    // its last attempt reached, without another.
    let spent = RequestId::new();
    let seen = world.script.seen().len();
    let mut last = earlier[0].clone();
    last.request_id = spent;
    let order = FetchOrder {
        max_retries: Some(0),
        ..page("https://shop.example/b")
    };
    let ended = finished(
        world
            .pipeline
            .fetch_for_job(spent, order, Some(vec![last.clone()]), stop)
            .await
            .unwrap(),
    );
    assert_eq!(ended.outcome, Outcome::Forbidden);
    assert_eq!(ended.attempts, [last.clone()]);
    assert_eq!(world.script.seen().len(), seen);
    let record = world
        .backend
        .records()
        .into_iter()
        .find(|record| record.request_id == spent)
        .unwrap();
    assert_eq!(record.status, last.status);
    assert_eq!(record.block, last.block);
    world.shut_down().await;
}
