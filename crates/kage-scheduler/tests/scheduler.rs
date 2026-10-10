//! The scheduler's grants and the gate above the persona manager's own
//! browser work (spec §11, follow-up F27), on a paused clock.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Duration as Span;
use common::{DEFAULT_DOMAINS, World, id, network, persona};
use kage_browser::contract::{EXAMPLES_JSON, PageResult};
use kage_browser::{BrowserError, Navigation};
use kage_core::domain::{BreakerState, DomainBreaker, IDLE_DOMAIN, Render};
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::{Completion, Refusal};
use kage_core::schedule::politeness::Courtesy;
use kage_core::schedule::queue::Priority;
use kage_persona::{BrowserGate, Held, Judged, Purpose};
use kage_scheduler::{Meter, SchedulerGate};

const ANSWERED: Completion = Completion {
    courtesy: Courtesy::Answered,
    blocked: false,
};

/// Lets the scheduler's loop run until it has nothing to do.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

fn gate(world: &World) -> SchedulerGate {
    SchedulerGate::new(
        Arc::clone(&world.scheduler),
        Arc::clone(&world.verdicts),
        None,
    )
}

/// A page a browser loaded with `status`.
fn page(status: i64) -> PageResult {
    let examples: serde_json::Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
    let mut page: PageResult = serde_json::from_value(examples["PageResult"][0].clone()).unwrap();
    page.error = None;
    page.status = Some(status);
    page.headers = Vec::new();
    "https://news.example/".clone_into(&mut page.final_url);
    page.challenge.interstitial = false;
    page.challenge.resolved = None;
    page.challenge.marker = None;
    page.challenge.script_hosts = Vec::new();
    page.dom_html = Some(format!(
        "<html><head><title>News</title></head><body>{}</body></html>",
        "<p>The council met on Tuesday and agreed on the budget for the coming year.</p>"
            .repeat(60)
    ));
    page
}

#[tokio::test(start_paused = true)]
async fn a_grant_holds_a_persona_and_a_session_until_it_ends() {
    let mut world = World::new(1, DEFAULT_DOMAINS);

    let grant = world
        .scheduler
        .acquire(world.work("a.example", Render::Browser, Priority::Sync))
        .await
        .unwrap();

    let dispatch = grant.dispatch().clone();
    assert_eq!(dispatch.persona, id(0));
    assert_eq!(dispatch.domain, "a.example");
    assert_eq!(dispatch.render, Render::Browser);
    assert!(dispatch.starts_visit && !dispatch.trial);
    assert_eq!(world.scheduler.in_flight(Render::Browser), 1);
    assert_eq!(world.scheduler.in_flight(Render::Http), 0);
    // The page is counted the moment it leaves, before any other is
    // planned.
    let state = world.backend.domain_state(id(0), "a.example").unwrap();
    assert_eq!(state.requests_today, 1);
    assert_eq!(state.last_request_at, Some(world.now()));

    // The persona makes one attempt at a time: other work waits for it,
    // and a caller that stops waiting leaves the queue.
    let waited = tokio::time::timeout(
        Duration::from_secs(20),
        world
            .scheduler
            .acquire(world.work("b.example", Render::Http, Priority::Sync)),
    )
    .await;
    assert!(waited.is_err());
    assert_eq!(world.scheduler.depth(Priority::Sync), 0);

    grant.complete(ANSWERED);
    assert_eq!(world.scheduler.in_flight(Render::Browser), 0);
    let next = world
        .scheduler
        .acquire(world.work("b.example", Render::Http, Priority::Sync))
        .await
        .unwrap();
    assert_eq!(next.dispatch().persona, id(0));
    assert_eq!(world.scheduler.in_flight(Render::Http), 1);

    // A grant that is only dropped gives everything back as well.
    drop(next);
    assert_eq!(world.scheduler.in_flight(Render::Http), 0);
    let again = world
        .scheduler
        .acquire(world.work("c.example", Render::Http, Priority::Sync))
        .await
        .unwrap();
    assert_eq!(again.dispatch().persona, id(0));
    again.complete(ANSWERED);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn the_caps_hold_however_much_waits() {
    let mut world = World::with(
        8,
        true,
        DEFAULT_DOMAINS,
        &[
            ("KAGE__SCHEDULER__MAX_BROWSER_SESSIONS", "2"),
            ("KAGE__SCHEDULER__MAX_HTTP_INFLIGHT", "3"),
        ],
    );
    let held = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for number in 0..6 {
        for render in [Render::Browser, Render::Http] {
            let scheduler = Arc::clone(&world.scheduler);
            let work = world.work(
                &format!("{}-{number}.example", render.as_str()),
                render,
                Priority::Job,
            );
            let held = Arc::clone(&held);
            tasks.push(tokio::spawn(async move {
                let grant = scheduler.acquire(work).await.unwrap();
                held.lock().unwrap().push(grant);
            }));
        }
    }

    tokio::time::sleep(Duration::from_mins(2)).await;

    // Eight personas are free and twelve attempts want one: two browser
    // sessions and three http requests are all that is in flight.
    assert_eq!(world.scheduler.in_flight(Render::Browser), 2);
    assert_eq!(world.scheduler.in_flight(Render::Http), 3);
    assert_eq!(world.scheduler.depth(Priority::Job), 7);
    assert_eq!(held.lock().unwrap().len(), 5);

    // Every attempt that ends lets one that waits go.
    for _ in 0..12 {
        let granted: Vec<_> = held.lock().unwrap().drain(..).collect();
        for grant in granted {
            grant.complete(ANSWERED);
        }
        tokio::time::sleep(Duration::from_mins(2)).await;
        assert!(world.scheduler.in_flight(Render::Browser) <= 2);
        assert!(world.scheduler.in_flight(Render::Http) <= 3);
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(world.scheduler.depth(Priority::Job), 0);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_synchronous_request_leaves_before_a_job_that_waited_longer() {
    let mut world = World::new(1, DEFAULT_DOMAINS);
    let first = world
        .scheduler
        .acquire(world.work("a.example", Render::Browser, Priority::Sync))
        .await
        .unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for (name, priority) in [("job", Priority::Job), ("sync", Priority::Sync)] {
        let scheduler = Arc::clone(&world.scheduler);
        let work = world.work(&format!("{name}.example"), Render::Browser, priority);
        let order = Arc::clone(&order);
        tasks.push(tokio::spawn(async move {
            let grant = scheduler.acquire(work).await.unwrap();
            order.lock().unwrap().push(name);
            tokio::time::sleep(Duration::from_secs(1)).await;
            grant.complete(ANSWERED);
        }));
        // The job asks first.
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(world.scheduler.depth(Priority::Job), 1);
    assert_eq!(world.scheduler.depth(Priority::Sync), 1);

    first.complete(ANSWERED);
    for task in tasks {
        task.await.unwrap();
    }

    assert_eq!(*order.lock().unwrap(), ["sync", "job"]);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_is_refused_at_once_when_nobody_can_ever_serve_it() {
    let mut world = World::with(2, false, DEFAULT_DOMAINS, &[]);

    // No persona of this pool has an http path.
    let refused = world
        .scheduler
        .acquire(world.work("a.example", Render::Http, Priority::Sync))
        .await
        .unwrap_err();

    assert_eq!(refused.reason, Refusal::NoPersona);
    assert_eq!(refused.retry_at, None);
    assert_eq!(world.scheduler.depth(Priority::Sync), 0);
    world.shut_down().await;

    // And everything is refused once the scheduler stopped.
    let refused = world
        .scheduler
        .acquire(world.work("a.example", Render::Browser, Priority::Sync))
        .await
        .unwrap_err();
    assert_eq!(refused.reason, Refusal::ShuttingDown);
}

#[tokio::test(start_paused = true)]
async fn the_managers_own_work_takes_a_personas_browser_through_the_scheduler() {
    let mut world = World::with(
        3,
        true,
        DEFAULT_DOMAINS,
        &[("KAGE__SCHEDULER__MAX_BROWSER_SESSIONS", "2")],
    );
    let gate = gate(&world);
    let warmup = Purpose::Warmup {
        domain: "news.example",
    };
    settle().await;

    let permit = gate.reserve(&persona(0, true), warmup).await.unwrap();

    // The visit holds the persona and one of the two browser sessions.
    assert_eq!(world.scheduler.in_flight(Render::Browser), 1);
    assert_eq!(
        gate.reserve(&persona(0, true), warmup).await.unwrap_err(),
        Held::PersonaBusy
    );
    // Real work does not get the persona meanwhile: another one serves.
    let grant = world
        .scheduler
        .acquire(world.work("shop.example", Render::Browser, Priority::Sync))
        .await
        .unwrap();
    assert_ne!(grant.dispatch().persona, id(0));
    // Both sessions are taken now: a third persona's self-test waits.
    let third = if grant.dispatch().persona == id(1) {
        2
    } else {
        1
    };
    let selftest = Purpose::Selftest { operator: false };
    assert_eq!(
        gate.reserve(&persona(third, true), selftest)
            .await
            .unwrap_err(),
        Held::NoSession
    );
    // A persona that is making an attempt is not interrupted either.
    assert_eq!(
        gate.reserve(&persona(3 - third, true), selftest)
            .await
            .unwrap_err(),
        Held::PersonaBusy
    );

    // The visit ends: the session is free again.
    drop(permit);
    assert_eq!(world.scheduler.in_flight(Render::Browser), 1);
    let permit = gate.reserve(&persona(third, true), selftest).await.unwrap();
    assert_eq!(world.scheduler.in_flight(Render::Browser), 2);
    drop(permit);
    grant.complete(ANSWERED);
    assert_eq!(world.scheduler.in_flight(Render::Browser), 0);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_persona_the_scheduler_has_not_seen_yet_waits_one_pass() {
    let mut world = World::new(1, DEFAULT_DOMAINS);
    let gate = gate(&world);
    settle().await;
    // Generated a moment ago.
    world
        .backend
        .put_persona(persona(5, true), true, network(5));
    let selftest = Purpose::Selftest { operator: true };

    assert_eq!(
        gate.reserve(&persona(5, true), selftest).await.unwrap_err(),
        Held::NoSession
    );

    settle().await;
    let permit = gate.reserve(&persona(5, true), selftest).await;
    assert!(permit.is_ok(), "{permit:?}");
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn the_stops_hold_for_the_managers_work_but_not_for_an_operators_self_test() {
    let mut world = World::new(2, DEFAULT_DOMAINS);
    let gate = gate(&world);
    let warmup = Purpose::Warmup {
        domain: "news.example",
    };
    let scheduled = Purpose::Selftest { operator: false };
    let asked = Purpose::Selftest { operator: true };
    settle().await;

    // A leak is on record: nothing of the manager's own leaves.
    world.backend.set_egress_unsafe(true);
    for purpose in [warmup, scheduled] {
        assert_eq!(
            gate.reserve(&persona(0, true), purpose).await.unwrap_err(),
            Held::Stopped("egress_unsafe")
        );
    }
    // The operator's own self-test is how the leak is looked into.
    drop(gate.reserve(&persona(0, true), asked).await.unwrap());
    world.backend.set_egress_unsafe(false);

    // The same at the hard stop of the bandwidth budget.
    world.scheduler.set_bandwidth_stop(true);
    for purpose in [warmup, scheduled] {
        assert_eq!(
            gate.reserve(&persona(0, true), purpose).await.unwrap_err(),
            Held::Stopped("bandwidth_exhausted")
        );
    }
    drop(gate.reserve(&persona(0, true), asked).await.unwrap());
    world.scheduler.set_bandwidth_stop(false);

    drop(gate.reserve(&persona(0, true), warmup).await.unwrap());
    assert_eq!(world.scheduler.in_flight(Render::Browser), 0);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_warm_up_visit_waits_while_its_site_is_paused_and_is_never_the_trial() {
    let mut world = World::new(2, DEFAULT_DOMAINS);
    let gate = gate(&world);
    let warmup = Purpose::Warmup {
        domain: "news.example",
    };
    settle().await;
    let until = world.now() + Span::minutes(30);
    world.backend.put_breaker(DomainBreaker {
        state: BreakerState::Open,
        paused_until: Some(until),
        trips: 1,
        ..DomainBreaker::closed("news.example", world.now())
    });

    assert_eq!(
        gate.reserve(&persona(0, true), warmup).await.unwrap_err(),
        Held::DomainPaused { until: Some(until) }
    );
    // Another site, and a self-test, are not held by it.
    let elsewhere = Purpose::Warmup {
        domain: "blog.example",
    };
    drop(gate.reserve(&persona(0, true), elsewhere).await.unwrap());
    drop(
        gate.reserve(&persona(0, true), Purpose::Selftest { operator: false })
            .await
            .unwrap(),
    );

    // The pause is over, but whether the site stays paused is for a real
    // request to find out.
    tokio::time::sleep(Duration::from_mins(31)).await;
    assert_eq!(
        gate.reserve(&persona(0, true), warmup).await.unwrap_err(),
        Held::DomainPaused { until: None }
    );
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn what_a_warm_up_visit_found_is_settled_like_any_attempt() {
    let mut world = World::new(2, DEFAULT_DOMAINS);
    let gate = gate(&world);
    let visitor = persona(0, true);

    // A page that was served.
    let served = Navigation::Page(Box::new(page(200)));
    let judged = gate
        .judge(&visitor, "news.example", Ok(&served))
        .await
        .unwrap();
    assert_eq!(
        judged,
        Judged {
            outcome: Outcome::Ok,
            status: Some(200)
        }
    );
    let state = world.backend.domain_state(id(0), "news.example").unwrap();
    assert_eq!(state.last_outcome, Some(Outcome::Ok));
    assert_eq!(state.cooldown_until, None);
    assert_eq!(world.backend.breaker("news.example"), None);

    // A block: the persona cools for the site and its breaker counts one.
    let limited = Navigation::Page(Box::new(page(429)));
    let judged = gate
        .judge(&visitor, "news.example", Ok(&limited))
        .await
        .unwrap();
    assert_eq!(judged.outcome, Outcome::RateLimited);
    assert_eq!(judged.status, Some(429));
    let state = world.backend.domain_state(id(0), "news.example").unwrap();
    assert!(
        state
            .cooldown_until
            .is_some_and(|until| until > world.now())
    );
    assert_eq!(state.block_count_24h, 1);
    assert_eq!(
        world
            .backend
            .breaker("news.example")
            .unwrap()
            .block_count_1h,
        1
    );

    // A sidecar that did not answer says nothing about the site.
    let failed = BrowserError::Timeout {
        operation: "navigate",
    };
    let judged = gate
        .judge(&visitor, "blog.example", Err(&failed))
        .await
        .unwrap();
    assert_eq!(
        judged,
        Judged {
            outcome: Outcome::ProviderError,
            status: None
        }
    );
    let state = world.backend.domain_state(id(0), "blog.example").unwrap();
    assert_eq!(state.cooldown_until, None);
    assert_eq!(world.backend.breaker("blog.example"), None);
    world.shut_down().await;
}

#[tokio::test(start_paused = true)]
async fn a_warm_up_visits_bytes_count_toward_its_site() {
    let mut world = World::new(1, DEFAULT_DOMAINS);
    let meter = Arc::new(Meter::default());
    let gate = SchedulerGate::new(
        Arc::clone(&world.scheduler),
        Arc::clone(&world.verdicts),
        Some(Arc::clone(&meter)),
    );
    settle().await;

    let permit = gate
        .reserve(
            &persona(0, true),
            Purpose::Warmup {
                domain: "news.example",
            },
        )
        .await
        .unwrap();
    meter.add(id(0), 100, 2000);
    drop(permit);
    // A self-test visits pages of the operator's choosing: nothing to
    // count them toward.
    let permit = gate
        .reserve(&persona(0, true), Purpose::Selftest { operator: true })
        .await
        .unwrap();
    meter.add(id(0), 10, 20);
    drop(permit);

    let counted = meter.drain();
    let by_domain: Vec<(&str, u64, u64)> = counted
        .iter()
        .map(|counted| {
            (
                counted.domain.as_str(),
                counted.bytes_up,
                counted.bytes_down,
            )
        })
        .collect();
    assert_eq!(
        by_domain,
        [(IDLE_DOMAIN, 10, 20), ("news.example", 100, 2000)]
    );
    world.shut_down().await;
}
