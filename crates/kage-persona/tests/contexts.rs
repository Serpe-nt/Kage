//! The one owner of persona contexts and the gate above the manager's own
//! browser work (follow-up F27), against a real Postgres and mock
//! sidecars.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Duration as Span;
use common::Harness;
use kage_browser::{BrowserError, Navigation};
use kage_core::config::{
    DelayRange, GapConfig, SelftestPageConfig, SelftestPageKind, VisitRange, WarmupSite,
};
use kage_core::domain::{ClaimedOs, Engine, Persona, ProxyEndpoint};
use kage_core::duration::HumanDuration;
use kage_core::ids::PersonaId;
use kage_core::outcome::Outcome;
use kage_core::persona::mix::MixEntry;
use kage_persona::selftest::{Verdict, pages};
use kage_persona::{BrowserGate, Held, Judged, Permit, PersonaError, Purpose, Via};
use kage_proxy::Clock;
use serde_json::json;

fn chromium_only(config: &mut kage_core::config::Config) {
    config.personas.mix = vec![MixEntry {
        engine: Engine::Chromium,
        os: ClaimedOs::Linux,
        weight: 1,
    }];
}

async fn endpoint(harness: &Harness, persona: &Persona) -> ProxyEndpoint {
    harness
        .manager
        .egress(persona, Via::Browser)
        .await
        .unwrap()
        .1
}

fn closed(harness: &Harness) -> Vec<String> {
    harness.chromium.with(|sidecar| sidecar.closed.clone())
}

#[tokio::test]
async fn a_personas_context_is_held_by_one_piece_of_work_at_a_time() {
    let harness = Harness::start_with(chromium_only).await;
    let (_, persona) = harness.persona("DE").await;
    let contexts = harness.manager.contexts();
    let endpoint = endpoint(&harness, &persona).await;

    let lease = contexts.acquire(&persona, &endpoint).await.unwrap();

    assert!(!lease.reused());
    assert_eq!(lease.engine(), Engine::Chromium);
    assert_eq!(contexts.open(), [persona.id]);
    assert_eq!(contexts.open_on(Engine::Chromium), 1);
    assert_eq!(contexts.open_on(Engine::Firefox), 0);
    // Nobody else gets it meanwhile, and nobody closes it under its
    // holder.
    let second = tokio::time::timeout(
        Duration::from_millis(200),
        contexts.acquire(&persona, &endpoint),
    )
    .await;
    assert!(second.is_err());
    assert!(!contexts.close_of(persona.id).await);
    assert_eq!(contexts.close_all().await, 0);

    // Given back, it stays open: the next piece of work finds the
    // persona's session as it was.
    let context = lease.context();
    lease.keep(true);
    let again = contexts.acquire(&persona, &endpoint).await.unwrap();
    assert!(again.reused());
    assert_eq!(again.context(), context);
    assert_eq!(closed(&harness).len(), 0);

    // Closed by its holder, it is gone.
    again.close().await;
    assert_eq!(closed(&harness), [context.to_string()]);
    assert_eq!(contexts.open(), []);

    // A lease that is only dropped leaves the window open and free.
    let dropped = contexts.acquire(&persona, &endpoint).await.unwrap();
    let reopened = dropped.context();
    assert_ne!(reopened, context);
    drop(dropped);
    let next = contexts.acquire(&persona, &endpoint).await.unwrap();
    assert!(next.reused());
    assert_eq!(next.context(), reopened);
    next.keep(false);

    // A persona that leaves has its window closed, once.
    assert!(contexts.close_of(persona.id).await);
    assert!(!contexts.close_of(persona.id).await);
    assert_eq!(closed(&harness).len(), 2);
}

#[tokio::test]
async fn a_full_engine_makes_room_by_closing_the_least_recently_used_context_not_in_a_visit() {
    let harness = Harness::start_with(|config| {
        chromium_only(config);
        config.scheduler.max_contexts_per_engine = 2;
    })
    .await;
    let contexts = harness.manager.contexts();
    let mut personas = Vec::new();
    for _ in 0..3 {
        let (_, persona) = harness.persona("DE").await;
        let endpoint = endpoint(&harness, &persona).await;
        personas.push((persona, endpoint));
    }
    let lease = |index: usize| {
        let (persona, endpoint) = &personas[index];
        contexts.acquire(persona, endpoint)
    };

    // The first is done with its visit; the second is in the middle of
    // one.
    let first = lease(0).await.unwrap();
    let first_context = first.context().to_string();
    first.keep(false);
    harness.clock.advance(Span::minutes(1));
    let second = lease(1).await.unwrap();
    let second_context = second.context().to_string();
    second.keep(true);
    harness.clock.advance(Span::minutes(1));

    // The third needs a slot: the first one's window goes.
    let third = lease(2).await.unwrap();
    let third_context = third.context().to_string();
    assert_eq!(closed(&harness), std::slice::from_ref(&first_context));
    assert_eq!(contexts.open_on(Engine::Chromium), 2);
    third.keep(false);
    harness.clock.advance(Span::minutes(1));

    // The first comes back: the third goes although the second was used
    // longer ago, because the second is in the middle of a visit.
    let first = lease(0).await.unwrap();
    assert!(!first.reused());
    assert_eq!(closed(&harness), [first_context, third_context]);
    first.keep(true);
    harness.clock.advance(Span::minutes(1));

    // When everyone is visiting, the one used longest ago goes.
    let third = lease(2).await.unwrap();
    assert_eq!(closed(&harness).last(), Some(&second_context));
    assert_eq!(contexts.open(), {
        let mut open = vec![personas[0].0.id, personas[2].0.id];
        open.sort_unstable();
        open
    });
    third.keep(false);
}

#[tokio::test]
async fn when_every_context_of_the_engine_is_held_a_lease_waits_for_one() {
    let harness = Harness::start_with(|config| {
        chromium_only(config);
        config.scheduler.max_contexts_per_engine = 1;
    })
    .await;
    let contexts = Arc::clone(harness.manager.contexts());
    let (_, first) = harness.persona("DE").await;
    let (_, second) = harness.persona("DE").await;
    let first_endpoint = endpoint(&harness, &first).await;
    let second_endpoint = endpoint(&harness, &second).await;
    let held = contexts.acquire(&first, &first_endpoint).await.unwrap();
    let held_context = held.context().to_string();

    let waiting = tokio::spawn({
        let contexts = Arc::clone(&contexts);
        let second = second.clone();
        async move {
            let lease = contexts.acquire(&second, &second_endpoint).await.unwrap();
            let context = lease.context();
            lease.keep(false);
            context
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!waiting.is_finished());
    assert_eq!(harness.chromium.with(|sidecar| sidecar.opened.len()), 1);

    held.keep(false);

    let context = waiting.await.unwrap();
    assert_eq!(closed(&harness), [held_context]);
    assert_eq!(contexts.open(), [second.id]);
    assert_ne!(context.to_string(), closed(&harness)[0]);
}

#[tokio::test]
async fn idle_contexts_are_closed_and_held_ones_never() {
    let harness = Harness::start_with(chromium_only).await;
    let contexts = harness.manager.contexts();
    let (_, idle) = harness.persona("DE").await;
    let (_, busy) = harness.persona("DE").await;
    let idle_endpoint = endpoint(&harness, &idle).await;
    let busy_endpoint = endpoint(&harness, &busy).await;
    let started = harness.clock.now();
    contexts
        .acquire(&idle, &idle_endpoint)
        .await
        .unwrap()
        .keep(true);
    let held = contexts.acquire(&busy, &busy_endpoint).await.unwrap();

    // Not idle for long enough yet.
    assert_eq!(contexts.close_idle(started - Span::seconds(1)).await, 0);
    harness.clock.advance(Span::minutes(6));

    // Five minutes without a page: the one nobody holds is closed,
    // visiting or not.
    let limit = harness.clock.now() - Span::minutes(5);
    assert_eq!(contexts.close_idle(limit).await, 1);
    assert_eq!(contexts.open(), [busy.id]);

    // Given back now, it is fresh; at shutdown everything goes.
    held.keep(false);
    assert_eq!(contexts.close_idle(limit).await, 0);
    assert_eq!(contexts.close_all().await, 1);
    assert_eq!(contexts.open(), []);
    assert_eq!(closed(&harness).len(), 2);
}

#[tokio::test]
async fn a_context_open_with_other_parameters_is_closed_and_opened_with_these() {
    let harness = Harness::start_with(chromium_only).await;
    let contexts = harness.manager.contexts();
    let (_, persona) = harness.persona("DE").await;
    let endpoint = endpoint(&harness, &persona).await;
    let lease = contexts.acquire(&persona, &endpoint).await.unwrap();
    let before = lease.context();
    lease.keep(false);

    // The persona's proxy moved since.
    let mut moved = endpoint.clone();
    moved.port = 10_001;
    let lease = contexts.acquire(&persona, &moved).await.unwrap();

    assert!(!lease.reused());
    assert_ne!(lease.context(), before);
    assert_eq!(closed(&harness), [before.to_string()]);
    let servers: Vec<String> = harness.chromium.with(|sidecar| {
        sidecar
            .opened
            .iter()
            .map(|request| request["proxy"]["server"].as_str().unwrap().to_owned())
            .collect()
    });
    assert_eq!(
        servers,
        [
            "http://127.0.0.1:9",
            "http://127.0.0.1:10001",
            "http://127.0.0.1:10001"
        ]
    );
    lease.close().await;

    // A container full of contexts Kage does not know refuses, and
    // nothing is believed to be open afterwards.
    harness.chromium.with(|sidecar| sidecar.capacity = 0);
    let refused = contexts.acquire(&persona, &endpoint).await.unwrap_err();
    assert!(
        matches!(&refused, PersonaError::Browser(error) if error.is_at_capacity()),
        "{refused}"
    );
    assert_eq!(contexts.open(), []);
    harness.chromium.with(|sidecar| sidecar.capacity = 3);
    contexts
        .acquire(&persona, &endpoint)
        .await
        .unwrap()
        .close()
        .await;
}

/// A gate that answers what a test set and records what it was asked.
#[derive(Debug, Default)]
struct FakeGate {
    held: Mutex<Option<Held>>,
    verdict: Mutex<Option<Outcome>>,
    asked: Mutex<Vec<String>>,
    judged: Mutex<Vec<(PersonaId, String, bool)>>,
    /// Permits that are out.
    live: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct Token(Arc<AtomicUsize>);

impl Permit for Token {}

impl Drop for Token {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl BrowserGate for FakeGate {
    async fn reserve(
        &self,
        _persona: &Persona,
        purpose: Purpose<'_>,
    ) -> Result<Box<dyn Permit>, Held> {
        self.asked.lock().unwrap().push(match purpose {
            Purpose::Warmup { domain } => format!("warmup:{domain}"),
            Purpose::Selftest { operator: true } => "selftest:operator".to_owned(),
            Purpose::Selftest { operator: false } => "selftest:scheduled".to_owned(),
        });
        if let Some(held) = self.held.lock().unwrap().clone() {
            return Err(held);
        }
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Token(Arc::clone(&self.live))))
    }

    async fn judge(
        &self,
        persona: &Persona,
        domain: &str,
        navigation: Result<&Navigation, &BrowserError>,
    ) -> Result<Judged, PersonaError> {
        // The permit is still out while the visit is judged.
        assert_eq!(self.live.load(Ordering::SeqCst), 1);
        self.judged
            .lock()
            .unwrap()
            .push((persona.id, domain.to_owned(), navigation.is_ok()));
        Ok(Judged {
            outcome: self.verdict.lock().unwrap().unwrap_or(Outcome::Ok),
            status: Some(200),
        })
    }
}

impl FakeGate {
    fn hold(&self, held: Option<Held>) {
        *self.held.lock().unwrap() = held;
    }

    fn asked(&self) -> Vec<String> {
        std::mem::take(&mut self.asked.lock().unwrap())
    }
}

#[tokio::test]
async fn warm_up_visits_ask_the_gate_first_and_are_judged_by_it() {
    let harness = Harness::start_with(|config| {
        chromium_only(config);
        let warmup = &mut config.personas.warmup;
        warmup.enabled = true;
        warmup.min_visits = 2;
        warmup.visits = VisitRange { min: 3, max: 3 };
        warmup.first_visit_after = DelayRange {
            min: HumanDuration::from_mins(3),
            max: HumanDuration::from_mins(20),
        };
        warmup.gap = GapConfig {
            median: HumanDuration::from_mins(10),
            p95: HumanDuration::from_mins(20),
        };
        warmup.sites = vec![WarmupSite {
            url: "https://news.example/".to_owned(),
            countries: Vec::new(),
        }];
    })
    .await;
    let gate = Arc::new(FakeGate::default());
    let as_gate: Arc<dyn BrowserGate> = gate.clone();
    assert!(harness.manager.set_gate(as_gate));
    // The first gate stays.
    let other: Arc<dyn BrowserGate> = Arc::new(FakeGate::default());
    assert!(!harness.manager.set_gate(other));
    assert!(harness.manager.gate().is_some());
    let (_, persona) = harness.persona("DE").await;
    let plan = harness.manager.warmup_plan(&persona);
    harness.clock.set(plan[0].due_at);

    // Whatever holds the persona, the visit waits and stays due.
    for held in [
        Held::PersonaBusy,
        Held::NoSession,
        Held::Stopped("egress_unsafe"),
        Held::DomainPaused { until: None },
    ] {
        gate.hold(Some(held));
        assert_eq!(harness.manager.warm_up_due().await.unwrap(), 0);
        assert_eq!(gate.asked(), ["warmup:news.example"]);
    }
    assert_eq!(harness.chromium.with(|sidecar| sidecar.opened.len()), 0);
    assert_eq!(harness.reload(&persona).await.warmup_visits, 0);

    // Free: the visit is made under the gate's permit, and what it found
    // is the gate's to judge and to settle.
    gate.hold(None);
    *gate.verdict.lock().unwrap() = Some(Outcome::Forbidden);
    assert_eq!(harness.manager.warm_up_due().await.unwrap(), 1);
    assert_eq!(gate.asked(), ["warmup:news.example"]);
    assert_eq!(
        *gate.judged.lock().unwrap(),
        [(persona.id, "news.example".to_owned(), true)]
    );
    assert_eq!(gate.live.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness.chromium.with(|sidecar| (
            sidecar.opened.len(),
            sidecar.navigated.len(),
            sidecar.closed.len()
        )),
        (1, 1, 1)
    );
    assert_eq!(harness.reload(&persona).await.warmup_visits, 1);
    let visits = harness.events_of(&persona).await;
    let visit = visits
        .iter()
        .find(|event| event.kind == "persona_warmup_visit")
        .unwrap();
    assert_eq!(visit.details["outcome"], json!("forbidden"));
    // The manager counted the page; the outcome and the cooldown are for
    // the gate to write, once.
    let state = harness
        .store()
        .persona_domain_state()
        .get(persona.id, "news.example")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.requests_today, 1);
    assert_eq!(state.last_outcome, None);
    assert_eq!(state.cooldown_until, None);

    // A visit while real work has the persona's window open leaves it
    // open.
    let endpoint = endpoint(&harness, &persona).await;
    let contexts = harness.manager.contexts();
    let lease = contexts.acquire(&persona, &endpoint).await.unwrap();
    let working = lease.context();
    lease.keep(true);
    *gate.verdict.lock().unwrap() = None;
    harness.clock.set(plan[1].due_at.max(harness.clock.now()));
    assert_eq!(harness.manager.warm_up_due().await.unwrap(), 1);
    assert_eq!(harness.chromium.with(|sidecar| sidecar.closed.len()), 1);
    assert_eq!(contexts.open(), [persona.id]);
    let visited_in = harness.chromium.with(|sidecar| sidecar.navigated.len());
    assert_eq!(visited_in, 2);
    let again = contexts.acquire(&persona, &endpoint).await.unwrap();
    assert_eq!(again.context(), working);
    again.close().await;
}

#[tokio::test]
async fn self_tests_ask_the_gate_and_a_scheduled_one_waits_where_it_holds() {
    let harness = Harness::start_with(|config| {
        chromium_only(config);
        config.selftest.browser.pages = vec![SelftestPageConfig {
            id: "probe".to_owned(),
            kind: SelftestPageKind::Probe,
            url: "https://fixture.kage.test/probe/".to_owned(),
        }];
    })
    .await;
    let gate = Arc::new(FakeGate::default());
    let as_gate: Arc<dyn BrowserGate> = gate.clone();
    assert!(harness.manager.set_gate(as_gate));
    let (_, persona) = harness.persona("DE").await;
    let own_pages = pages(&harness.config.selftest.browser, false);
    harness.clock.advance(Span::minutes(31));

    // Held: the scheduled run is not an error, it is tried at the next
    // sweep; an operator's run says why it cannot be made.
    gate.hold(Some(Held::NoSession));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 0);
    assert_eq!(gate.asked(), ["selftest:scheduled"]);
    let refused = harness
        .manager
        .selftest_browser(persona.id, own_pages.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(
            &refused,
            PersonaError::Held { persona: held, held: Held::NoSession } if *held == persona.id
        ),
        "{refused}"
    );
    assert_eq!(gate.asked(), ["selftest:operator"]);
    assert_eq!(harness.chromium.with(|sidecar| sidecar.opened.len()), 0);

    // Free: both run, each under a permit that is given back.
    gate.hold(None);
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 1);
    let run = harness
        .manager
        .selftest_browser(persona.id, own_pages)
        .await
        .unwrap();
    assert_eq!(run.verdict, Verdict::Passed);
    assert_eq!(gate.asked(), ["selftest:scheduled", "selftest:operator"]);
    assert_eq!(gate.live.load(Ordering::SeqCst), 0);
    assert_eq!(
        harness.chromium.with(|sidecar| (
            sidecar.selftested.len(),
            sidecar.opened.len(),
            sidecar.closed.len()
        )),
        (2, 2, 2)
    );
    // A self-test is not judged as a visit to a site.
    assert_eq!(gate.judged.lock().unwrap().len(), 0);
}
