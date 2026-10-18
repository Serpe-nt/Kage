//! Sessions, per-domain state, the cookie handoff, warm-up and self-tests
//! of personas, against a real Postgres and mock sidecars.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::collections::BTreeSet;

use chrono::{Duration as Span, Utc};
use common::{Harness, selftest_answer};
use kage_core::alert::AlertKind;
use kage_core::config::{
    DelayRange, GapConfig, SelftestPageConfig, SelftestPageKind, VisitRange, WarmupSite,
};
use kage_core::domain::{
    ClaimedOs, CookieSameSite, CoolingReason, Engine, HttpCookie, PersonaState, Render, SessionId,
};
use kage_core::duration::HumanDuration;
use kage_core::outcome::Outcome;
use kage_core::persona::mix::MixEntry;
use kage_persona::identity::{HttpPathOff, http_spec};
use kage_persona::selftest::{Verdict, pages};
use kage_persona::{Eligibility, PersonaError, PersonaEvent, SessionError};
use kage_proxy::Clock;
use serde_json::json;
use uuid::Uuid;

fn id(text: &str) -> SessionId {
    SessionId::new(text).unwrap()
}

fn only(engine: Engine, os: ClaimedOs) -> Vec<MixEntry> {
    vec![MixEntry {
        engine,
        os,
        weight: 1,
    }]
}

#[tokio::test]
async fn sessions_pin_a_callers_requests_to_one_persona_until_they_end() {
    let harness = Harness::start().await;
    let (_, first) = harness.persona("DE").await;
    let (_, second) = harness.persona("FR").await;
    let sessions = harness.manager.sessions();
    let (alice, bob) = (harness.api_key("alice").await, harness.api_key("bob").await);
    let everyone = |_: &kage_core::domain::Persona| true;

    // A caller's id opens a session; the same id is the same session on
    // the same persona.
    let opened = sessions
        .open(alice, Some(id("checkout")), everyone)
        .await
        .unwrap();
    assert!(opened.created);
    assert_eq!(opened.session.id, id("checkout"));
    assert_eq!(opened.session.created_at, harness.clock.now());
    assert_eq!(
        opened.session.expires_at,
        harness.clock.now() + Span::minutes(10)
    );
    harness.clock.advance(Span::minutes(4));
    let again = sessions
        .open(alice, Some(id("checkout")), everyone)
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.persona.id, opened.persona.id);
    // Its idle time started again.
    assert_eq!(
        again.session.expires_at,
        harness.clock.now() + Span::minutes(10)
    );

    // Ids are unique per key: another key's `checkout` is another
    // session, and with one session on one persona it goes to the other.
    let other_key = sessions
        .open(bob, Some(id("checkout")), everyone)
        .await
        .unwrap();
    assert!(other_key.created);
    assert_ne!(other_key.persona.id, opened.persona.id);
    assert!(
        sessions
            .resolve(bob, &id("nothing"))
            .await
            .unwrap()
            .is_none()
    );

    // Without an id the server mints one.
    let minted = sessions.open(alice, None, everyone).await.unwrap();
    assert!(minted.created);
    assert!(minted.session.id.as_str().starts_with("s-"));
    // The caller may narrow the choice.
    let narrowed = sessions
        .open(alice, None, |persona| persona.id == second.id)
        .await
        .unwrap();
    assert_eq!(narrowed.persona.id, second.id);
    assert!(matches!(
        sessions.open(alice, None, |_| false).await,
        Err(SessionError::NoPersona)
    ));

    // Ten minutes without use end a session; the id then names a new one.
    harness.clock.advance(Span::minutes(10));
    assert_eq!(
        sessions.resolve(alice, &id("checkout")).await.unwrap(),
        None
    );
    let reopened = sessions
        .open(alice, Some(id("checkout")), everyone)
        .await
        .unwrap();
    assert!(reopened.created);
    assert_eq!(reopened.session.created_at, harness.clock.now());

    // A busy session still ends six hours after it was opened.
    let born = harness.clock.now();
    for _ in 0..71 {
        harness.clock.advance(Span::minutes(5));
        let used = sessions
            .resolve(alice, &id("checkout"))
            .await
            .unwrap()
            .expect("used every five minutes");
        assert!(used.session.expires_at <= born + Span::hours(6));
    }
    assert_eq!(harness.clock.now(), born + Span::minutes(355));
    harness.clock.advance(Span::minutes(5));
    assert_eq!(
        sessions.resolve(alice, &id("checkout")).await.unwrap(),
        None
    );

    // The sweep removes what has ended.
    let kept = sessions
        .open(alice, Some(id("kept")), everyone)
        .await
        .unwrap();
    sessions
        .open(alice, Some(id("stale")), everyone)
        .await
        .unwrap();
    harness.clock.advance(Span::minutes(6));
    sessions.resolve(alice, &id("kept")).await.unwrap().unwrap();
    harness.clock.advance(Span::minutes(6));
    assert!(harness.manager.sweep().await.sessions_ended >= 1);
    assert!(
        sessions
            .resolve(alice, &id("kept"))
            .await
            .unwrap()
            .is_some()
    );

    // Ending a session by hand.
    assert!(sessions.end(alice, &id("kept")).await.unwrap());
    assert!(!sessions.end(alice, &id("kept")).await.unwrap());

    // A persona that cools gets no new session; one that is retired
    // ends the sessions it had.
    let pinned = sessions
        .open(alice, Some(id("pinned")), |persona| {
            persona.id == kept.persona.id
        })
        .await
        .unwrap();
    harness
        .store()
        .personas()
        .modify(pinned.persona.id, |persona| {
            persona.state = PersonaState::Cooling;
            persona.cooling_reason = Some(CoolingReason::SelftestFailed);
        })
        .await
        .unwrap();
    let other = if pinned.persona.id == first.id {
        second.id
    } else {
        first.id
    };
    for n in 0..6 {
        let new = sessions
            .open(alice, Some(id(&format!("new-{n}"))), everyone)
            .await
            .unwrap();
        assert_eq!(new.persona.id, other);
    }
    assert!(
        sessions
            .resolve(alice, &id("pinned"))
            .await
            .unwrap()
            .is_some()
    );
    harness
        .manager
        .retire(pinned.persona.id, "operator")
        .await
        .unwrap();
    assert_eq!(sessions.resolve(alice, &id("pinned")).await.unwrap(), None);
    assert_eq!(sessions.limits(), harness.config.sessions.limits());
}

#[tokio::test]
async fn a_persona_counts_its_requests_per_domain_on_its_own_calendar() {
    let harness = Harness::start().await;
    let (_, persona) = harness.persona("DE").await;
    let domains = harness.manager.domains();
    let domain = "shop.example";

    assert_eq!(
        domains
            .eligibility(&persona, domain, Some(3))
            .await
            .unwrap(),
        Eligibility::Eligible
    );
    // Noon UTC is two in the afternoon in Berlin, in October.
    let today = harness.clock.now().date_naive();
    for expected in 1..=3 {
        let state = domains.record_request(&persona, domain).await.unwrap();
        assert_eq!(state.requests_today, expected);
        assert_eq!(state.counter_day, Some(today));
        assert_eq!(state.last_request_at, Some(harness.clock.now()));
    }
    // The cap is reached: out until midnight in Berlin, 22:00 UTC.
    let capped = domains
        .eligibility(&persona, domain, Some(3))
        .await
        .unwrap();
    assert_eq!(
        capped,
        Eligibility::CapReached {
            until: harness.clock.now() + Span::hours(10)
        }
    );
    // Another domain, and a domain without a cap, are not affected.
    assert!(
        domains
            .eligibility(&persona, "other.example", Some(3))
            .await
            .unwrap()
            .is_eligible()
    );
    assert!(
        domains
            .eligibility(&persona, domain, None)
            .await
            .unwrap()
            .is_eligible()
    );
    harness.clock.advance(Span::hours(10));
    assert!(
        domains
            .eligibility(&persona, domain, Some(3))
            .await
            .unwrap()
            .is_eligible()
    );
    let state = domains.record_request(&persona, domain).await.unwrap();
    assert_eq!(state.requests_today, 1);
    assert_eq!(state.counter_day, Some(today.succ_opt().unwrap()));

    // A block cools the domain for this persona and counts.
    let blocked_at = harness.clock.now();
    let state = domains
        .record_outcome(
            &persona,
            domain,
            Outcome::RateLimited,
            Some(std::time::Duration::from_hours(2)),
        )
        .await
        .unwrap();
    assert_eq!(state.last_outcome, Some(Outcome::RateLimited));
    assert_eq!(state.block_count_24h, 1);
    assert_eq!(
        domains
            .eligibility(&persona, domain, Some(3))
            .await
            .unwrap(),
        Eligibility::CoolingDown {
            until: blocked_at + Span::hours(2)
        }
    );
    // A success does not end it; a second block counts and never
    // shortens it.
    harness.clock.advance(Span::minutes(30));
    domains
        .record_outcome(&persona, domain, Outcome::Ok, None)
        .await
        .unwrap();
    let state = domains
        .record_outcome(
            &persona,
            domain,
            Outcome::Forbidden,
            Some(std::time::Duration::from_mins(10)),
        )
        .await
        .unwrap();
    assert_eq!(state.block_count_24h, 2);
    assert_eq!(state.cooldown_until, Some(blocked_at + Span::hours(2)));
    harness.clock.advance(Span::minutes(91));
    assert!(
        domains
            .eligibility(&persona, domain, Some(3))
            .await
            .unwrap()
            .is_eligible()
    );

    // Requests counted at the same moment are all counted.
    let before = domains.record_request(&persona, domain).await.unwrap();
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let domains = domains.clone();
        let persona = persona.clone();
        tasks.push(tokio::spawn(async move {
            domains.record_request(&persona, "shop.example").await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let after = harness
        .store()
        .persona_domain_state()
        .get(persona.id, domain)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.requests_today, before.requests_today + 12);
}

fn stored_cookie(domain: &str, name: &str) -> HttpCookie {
    HttpCookie {
        domain: domain.to_owned(),
        host_only: false,
        path: "/".to_owned(),
        name: name.to_owned(),
        value: "from-http".to_owned(),
        secure: true,
        http_only: false,
        same_site: Some(CookieSameSite::Lax),
        expires_at: Some(Utc::now() + Span::days(30)),
        created_at: Utc::now(),
        last_used_at: Utc::now(),
    }
}

#[tokio::test]
async fn cookies_move_between_the_browser_and_the_http_jar_only_on_the_same_major() {
    // Chrome 149 has an emulation profile of its own major.
    let harness = Harness::with(149, 151, |config| {
        config.personas.mix = only(Engine::Chromium, ClaimedOs::Linux);
        config.selftest.http.echo_url = Some("https://echo.kage.test/".to_owned());
    })
    .await;
    let (proxy, persona) = harness.persona("DE").await;
    assert_eq!(persona.wreq_profile.as_deref(), Some("chrome_149"));
    let context = Uuid::now_v7();
    let origin = "https://www.shop.example";
    let expires = (Utc::now() + Span::days(30)).to_rfc3339();

    // Browser to http: the cookies of the origin arrive in the persona's
    // own jar and in the store, under the persona.
    harness.chromium.with(|state| {
        state.export_cookies = json!([
            { "domain": "www.shop.example", "host_only": true, "path": "/", "name": "sid",
              "value": "from-browser", "secure": true, "http_only": true, "same_site": "lax",
              "expires_at": expires },
            { "domain": "shop.example", "host_only": false, "path": "/", "name": "consent",
              "value": "yes", "secure": false, "http_only": false, "same_site": null,
              "expires_at": expires },
            { "domain": "tracker.example", "host_only": false, "path": "/", "name": "third",
              "value": "x", "secure": true, "http_only": false, "same_site": "none",
              "expires_at": expires },
        ]);
    });
    let moved = harness
        .manager
        .browser_to_http(&persona, context, origin)
        .await
        .unwrap();
    assert_eq!((moved.moved, moved.rejected.len()), (2, 0));
    let exported = harness.chromium.with(|state| state.exported.clone());
    assert_eq!(
        exported,
        [json!({ "origin": origin, "include_storage": false })]
    );
    let stored = harness
        .store()
        .http_cookies()
        .list(proxy.id, Some(persona.id))
        .await
        .unwrap();
    let names: BTreeSet<&str> = stored.iter().map(|cookie| cookie.name.as_str()).collect();
    assert_eq!(names, BTreeSet::from(["consent", "sid"]));
    // The proxy's own jar of before M6 holds nothing of it.
    assert_eq!(
        harness
            .store()
            .http_cookies()
            .list(proxy.id, None)
            .await
            .unwrap(),
        []
    );
    let (_, endpoint) = harness.manager.route(&persona).await.unwrap();
    let client = harness
        .http
        .client(&http_spec(&persona).unwrap(), &endpoint)
        .await
        .unwrap();
    assert_eq!(client.persona(), Some(persona.id));
    assert_eq!(client.jar().len(), 2);

    // Http to browser: what the http path holds for the origin goes into
    // the context, and nothing of another site.
    client.jar().import(stored_cookie("shop.example", "cart"));
    client
        .jar()
        .import(stored_cookie("elsewhere.example", "other"));
    let back = harness
        .manager
        .http_to_browser(&persona, context, origin)
        .await
        .unwrap();
    assert_eq!(back.moved, 3);
    let imported = harness.chromium.with(|state| state.imported.clone());
    assert_eq!(imported.len(), 1);
    assert_eq!(imported[0]["origin"], json!(origin));
    let names: BTreeSet<&str> = imported[0]["cookies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|cookie| cookie["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, BTreeSet::from(["cart", "consent", "sid"]));
    // An origin the jar has nothing for asks the sidecar nothing.
    let nothing = harness
        .manager
        .http_to_browser(&persona, context, "https://empty.example")
        .await
        .unwrap();
    assert_eq!(nothing.moved, 0);
    assert_eq!(harness.chromium.with(|state| state.imported.len()), 1);
    assert!(matches!(
        harness
            .manager
            .browser_to_http(&persona, context, "https://www.shop.example/cart")
            .await,
        Err(PersonaError::Origin(_))
    ));

    // The http self-test runs as the persona. Nothing listens behind the
    // test proxy, so the echo cannot be read: no verdict, the run is on
    // record and the persona is what it was.
    let echo = "https://echo.kage.test/".parse().unwrap();
    let run = harness
        .manager
        .selftest_http(persona.id, &echo)
        .await
        .unwrap();
    assert_eq!(run.verdict, Verdict::Inconclusive);
    assert_eq!(run.report, None);
    assert_eq!(run.run.render, Render::Http);
    assert!(!run.run.passed);
    assert_eq!(run.run.findings["profile"], json!("chrome_149"));
    assert_eq!(run.persona.state, PersonaState::Active);
    assert_eq!(
        harness
            .store()
            .selftest_runs()
            .count(persona.id, Render::Http)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        harness
            .count_events(&persona, "persona_selftest_inconclusive")
            .await,
        1
    );
    // The schedule asks the operator's own echo, a few minutes after the
    // persona was generated and then not again for hours.
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 0);
    harness.clock.advance(Span::hours(3));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 1);
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 0);

    // The engine image moves to another major than the profile presents:
    // the cookies stay where they are, in both directions, and nothing is
    // asked of the sidecar.
    let upgraded = harness
        .store()
        .personas()
        .modify(persona.id, |persona| {
            persona.browser_major = 155;
            persona.clone()
        })
        .await
        .unwrap()
        .unwrap();
    for refused in [
        harness
            .manager
            .browser_to_http(&upgraded, context, origin)
            .await,
        harness
            .manager
            .http_to_browser(&upgraded, context, origin)
            .await,
    ] {
        assert!(
            matches!(
                &refused,
                Err(PersonaError::HttpPathOff {
                    persona: id,
                    reason: HttpPathOff::MajorMismatch { browser_major: 155, .. },
                }) if *id == persona.id
            ),
            "{refused:?}"
        );
    }
    assert!(matches!(
        harness.manager.selftest_http(persona.id, &echo).await,
        Err(PersonaError::HttpPathOff { .. })
    ));
    let (exports, imports) = harness
        .chromium
        .with(|state| (state.exported.len(), state.imported.len()));
    assert_eq!((exports, imports), (1, 1));

    // A browser-only persona has no jar to hand anything to.
    let browser_only = harness
        .store()
        .personas()
        .modify(persona.id, |persona| {
            persona.wreq_profile = None;
            persona.clone()
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        harness
            .manager
            .browser_to_http(&browser_only, context, origin)
            .await,
        Err(PersonaError::HttpPathOff {
            reason: HttpPathOff::NoProfile,
            ..
        })
    ));
}

#[tokio::test]
async fn a_new_persona_warms_up_in_its_own_browser_before_it_is_ready() {
    let harness = Harness::start_with(|config| {
        let warmup = &mut config.personas.warmup;
        warmup.enabled = true;
        warmup.min_visits = 2;
        warmup.visits = VisitRange { min: 5, max: 5 };
        warmup.first_visit_after = DelayRange {
            min: HumanDuration::from_mins(3),
            max: HumanDuration::from_mins(20),
        };
        warmup.gap = GapConfig {
            median: HumanDuration::from_mins(10),
            p95: HumanDuration::from_mins(20),
        };
        warmup.sites = vec![
            WarmupSite {
                url: "https://{language}.encyclopedia.example/".to_owned(),
                countries: Vec::new(),
            },
            WarmupSite {
                url: "https://news.example/".to_owned(),
                countries: Vec::new(),
            },
        ];
    })
    .await;
    let mut events = harness.manager.subscribe();
    let (proxy, persona) = harness.persona("DE").await;
    let sidecar = harness.sidecar(persona.engine);

    // A new persona is not ready: the scheduler does not use it yet.
    assert_eq!(persona.warmed_at, None);
    assert!(!persona.is_ready());
    assert!(matches!(
        harness
            .manager
            .sessions()
            .open(harness.api_key("early").await, None, |_| true)
            .await,
        Err(SessionError::NoPersona)
    ));
    let plan = harness.manager.warmup_plan(&persona);
    assert_eq!(plan.len(), 5);
    assert_eq!(plan, harness.manager.warmup_plan(&persona));
    let first_after = plan[0].due_at.signed_duration_since(persona.created_at);
    assert!(
        (3..=20).contains(&first_after.num_minutes()),
        "{first_after}"
    );
    for visit in &plan {
        assert!(
            ["de.encyclopedia.example", "news.example"].contains(&visit.domain()),
            "{}",
            visit.url
        );
    }

    // Nothing is due yet.
    assert_eq!(harness.manager.warm_up_due().await.unwrap(), 0);
    assert_eq!(sidecar.with(|state| state.opened.len()), 0);

    // The first visit: the persona's own context through its own proxy,
    // a typed address read like a person reads, and the context closed.
    harness.clock.set(plan[0].due_at);
    assert_eq!(harness.manager.warm_up_due().await.unwrap(), 1);
    let (opened, navigated, closed) = sidecar.with(|state| {
        (
            state.opened.clone(),
            state.navigated.clone(),
            state.closed.len(),
        )
    });
    assert_eq!((opened.len(), navigated.len(), closed), (1, 1, 1));
    assert_eq!(opened[0]["persona_id"], json!(persona.id));
    assert_eq!(opened[0]["fingerprint"], persona.fingerprint);
    assert_eq!(opened[0]["profile_dir"], json!(persona.profile_dir));
    assert_eq!(opened[0]["locale"], json!(persona.locale));
    assert_eq!(opened[0]["languages"], json!(persona.languages));
    assert_eq!(opened[0]["timezone"], json!(persona.timezone));
    assert_eq!(opened[0]["proxy"]["server"], json!("http://127.0.0.1:9"));
    assert_eq!(navigated[0]["url"], json!(plan[0].url.as_str()));
    assert_eq!(navigated[0]["behavior"]["dwell"], json!("human"));
    assert_eq!(
        navigated[0]["behavior"]["entry"]["strategy"],
        json!("direct")
    );
    assert_eq!(navigated[0]["behavior"]["wander"], json!(true));
    assert_eq!(navigated[0]["capture"]["dom"], json!(false));
    // The same sweep does not make the next visit.
    assert_eq!(harness.manager.warm_up_due().await.unwrap(), 0);
    let after_one = harness.reload(&persona).await;
    assert_eq!(after_one.warmup_visits, 1);
    assert_eq!(after_one.warmed_at, None);
    // The visit counts toward the persona's state on that site, and the
    // proxy saw traffic.
    let state = harness
        .store()
        .persona_domain_state()
        .get(persona.id, plan[0].domain())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.requests_today, 1);
    assert_eq!(state.last_outcome, Some(Outcome::Ok));
    assert_eq!(
        harness.pool.last_traffic(proxy.id),
        Some(harness.clock.now())
    );

    // A persona that cools does not browse.
    harness.clock.set(plan[1].due_at);
    harness.fail_into_backoff(proxy.id).await;
    harness.manager.ensure(proxy.id).await.unwrap();
    assert_eq!(harness.manager.warm_up_due().await.unwrap(), 0);
    let now = harness.clock.now();
    harness
        .recover(proxy.id, Some(common::capital("DE", now)))
        .await;
    harness.manager.ensure(proxy.id).await.unwrap();

    // The second visit is blocked: it is on record, that site cools for
    // this persona, and nothing is tried again. Two visits make the
    // persona ready whatever they found.
    while events.try_recv().is_ok() {}
    sidecar.with(|state| state.page_status = 403);
    assert_eq!(harness.manager.warm_up_due().await.unwrap(), 1);
    sidecar.with(|state| state.page_status = 200);
    let blocked = plan[1].domain().to_owned();
    let state = harness
        .store()
        .persona_domain_state()
        .get(persona.id, &blocked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.last_outcome, Some(Outcome::Forbidden));
    assert_eq!(state.block_count_24h, 1);
    assert_eq!(
        state.cooldown_until,
        Some(harness.clock.now() + Span::hours(24))
    );
    let ready = harness.reload(&persona).await;
    assert_eq!(ready.warmup_visits, 2);
    assert_eq!(ready.warmed_at, Some(harness.clock.now()));
    assert!(ready.is_ready());
    assert_eq!(
        events.try_recv().unwrap(),
        PersonaEvent::Warmed {
            persona: persona.id
        }
    );
    assert_eq!(harness.count_events(&persona, "persona_warmed").await, 1);
    assert_eq!(sidecar.with(|state| state.navigated.len()), 2);

    // The rest of the plan is made afterwards; a visit to the site that
    // cools is skipped without a request.
    let mut made = 2;
    for visit in &plan[2..] {
        harness.clock.set(visit.due_at.max(harness.clock.now()));
        assert_eq!(harness.manager.warm_up_due().await.unwrap(), 1);
        if visit.domain() != blocked {
            made += 1;
        }
        assert_eq!(sidecar.with(|state| state.navigated.len()), made);
    }
    let done = harness.reload(&persona).await;
    assert_eq!(done.warmup_visits, 5);
    assert_eq!(done.warmed_at, ready.warmed_at);
    assert_eq!(
        sidecar.with(|state| (state.opened.len(), state.closed.len())),
        (made, made)
    );
    let visits = harness.events_of(&persona).await;
    let outcomes: Vec<&str> = visits
        .iter()
        .filter(|event| event.kind == "persona_warmup_visit")
        .map(|event| event.details["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes.len(), 5);
    assert_eq!(&outcomes[..2], ["ok", "forbidden"]);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == "skipped")
            .count(),
        5 - made
    );
    // The plan is over: nothing more is ever visited.
    harness.clock.advance(Span::days(2));
    assert_eq!(harness.manager.warm_up_due().await.unwrap(), 0);

    // A persona whose warm-up cannot run is ready after the longest wait.
    let (_, waiting) = harness.persona("DE").await;
    harness
        .store()
        .personas()
        .modify(waiting.id, |persona| {
            persona.state = PersonaState::Cooling;
            persona.cooling_reason = Some(CoolingReason::SelftestFailed);
        })
        .await
        .unwrap();
    harness.clock.advance(Span::hours(11));
    harness.manager.warm_up_due().await.unwrap();
    assert_eq!(harness.reload(&waiting).await.warmed_at, None);
    harness.clock.advance(Span::hours(1));
    harness.manager.warm_up_due().await.unwrap();
    let waited = harness.reload(&waiting).await;
    assert_eq!(waited.warmed_at, Some(harness.clock.now()));
    assert_eq!(waited.warmup_visits, 0);
}

#[tokio::test]
async fn scheduled_self_tests_load_only_the_operators_pages_and_cool_a_failing_persona() {
    let harness = Harness::start_with(|config| {
        config.personas.mix = only(Engine::Firefox, ClaimedOs::Windows);
        config.selftest.browser.pages = vec![SelftestPageConfig {
            id: "probe".to_owned(),
            kind: SelftestPageKind::Probe,
            url: "https://fixture.kage.test/probe/".to_owned(),
        }];
    })
    .await;
    let (proxy, persona) = harness.persona("AT").await;
    let runs = || async {
        harness
            .store()
            .selftest_runs()
            .recent(persona.id, 50)
            .await
            .unwrap()
    };
    let failures = || {
        harness
            .alerts
            .alerts()
            .into_iter()
            .filter(|alert| alert.kind == AlertKind::BrowserSelftestFailed)
            .collect::<Vec<_>>()
    };

    // The first run comes a few minutes after the persona was generated.
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 0);
    harness.clock.advance(Span::minutes(31));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 1);
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 0);

    // It ran in the persona's own context, was judged against the
    // persona as it is stored, and loaded the operator's own page only:
    // no page run by others, ever, on a timer.
    let (opened, tested, closed) = harness.firefox.with(|state| {
        (
            state.opened.clone(),
            state.selftested.clone(),
            state.closed.len(),
        )
    });
    assert_eq!((opened.len(), tested.len(), closed), (1, 1, 1));
    assert_eq!(opened[0]["fingerprint"], persona.fingerprint);
    assert_eq!(opened[0]["profile_dir"], json!(persona.profile_dir));
    let urls: Vec<&str> = tested[0]["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| page["url"].as_str().unwrap())
        .collect();
    assert_eq!(urls, ["https://fixture.kage.test/probe/"]);
    assert_eq!(
        tested[0]["expected"],
        json!({
            "browser_major": 156,
            "claimed_os": "windows",
            "languages": ["de", "en-US", "en"],
            "timezone": "Europe/Vienna",
        })
    );
    assert_eq!(tested[0]["min_score"], json!(90.0));
    // Camoufox 156 has a recorded capture for an echo page.
    assert!(tested[0]["echo"]["ja4"].is_string());
    let stored = runs().await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].render, Render::Browser);
    assert_eq!(stored[0].engine, Engine::Firefox);
    assert!(stored[0].passed);
    assert_eq!(stored[0].score, Some(100.0));
    assert_eq!(stored[0].findings["verdict"], json!("passed"));
    assert_eq!(stored[0].finished_at, harness.clock.now());

    // A day later, give or take a quarter: the next one.
    harness.clock.advance(Span::hours(17));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 0);
    harness.clock.advance(Span::hours(14));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 1);

    // A test site that cannot be read is no verdict: nothing changes.
    let mut outage = selftest_answer(true);
    outage["complete"] = json!(false);
    outage["findings"][0]["status"] = json!("unknown");
    harness.firefox.with(|state| state.selftest = outage);
    harness.clock.advance(Span::hours(31));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 1);
    assert_eq!(harness.reload(&persona).await.state, PersonaState::Active);
    assert_eq!(runs().await[0].findings["verdict"], json!("inconclusive"));
    assert_eq!(failures().len(), 0);

    // The page shows an automation flag: the persona cools until a
    // self-test passes, and the alert is raised once.
    harness
        .firefox
        .with(|state| state.selftest = selftest_answer(false));
    harness.clock.advance(Span::hours(3));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 1);
    let cooling = harness.reload(&persona).await;
    assert_eq!(cooling.state, PersonaState::Cooling);
    assert_eq!(cooling.cooling_reason, Some(CoolingReason::SelftestFailed));
    let failed = &runs().await[0];
    assert!(!failed.passed);
    assert_eq!(failed.score, Some(40.0));
    assert_eq!(failed.findings["verdict"], json!("failed"));
    assert_eq!(failures().len(), 1);
    let alert = &failures()[0];
    assert_eq!(alert.subject, Some(persona.id.as_uuid()));
    assert_eq!(
        alert.subject_kind,
        Some(kage_core::domain::SubjectKind::Persona)
    );
    assert_eq!(
        alert.details["failed"],
        json!(["probe/navigator.webdriver"])
    );
    assert_eq!(
        harness
            .count_events(&persona, "persona_selftest_failed")
            .await,
        1
    );
    // Its proxy coming back does not end that cooling.
    harness.manager.ensure(proxy.id).await.unwrap();
    assert_eq!(harness.reload(&persona).await.state, PersonaState::Cooling);

    // It is tested again after two hours, not after a day. Still
    // failing: still cooling, and the alert is not raised again.
    harness.clock.advance(Span::minutes(80));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 0);
    harness.clock.advance(Span::minutes(80));
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 1);
    assert_eq!(failures().len(), 1);

    // An operator's run on demand may load the pages run by others, and
    // a pass ends the cooling.
    harness
        .firefox
        .with(|state| state.selftest = selftest_answer(true));
    let on_demand = harness
        .manager
        .selftest_browser(persona.id, pages(&harness.config.selftest.browser, true))
        .await
        .unwrap();
    assert_eq!(on_demand.verdict, Verdict::Passed);
    assert_eq!(on_demand.persona.state, PersonaState::Active);
    assert_eq!(on_demand.persona.cooling_reason, None);
    let last = harness
        .firefox
        .with(|state| state.selftested.last().cloned().unwrap());
    let ids: Vec<&str> = last["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| page["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["probe", "sannysoft", "creepjs"]);
    assert_eq!(
        harness
            .count_events(&persona, "persona_selftest_passed")
            .await,
        3
    );
    // A later failure is said again.
    harness
        .firefox
        .with(|state| state.selftest = selftest_answer(false));
    harness
        .manager
        .selftest_browser(persona.id, pages(&harness.config.selftest.browser, false))
        .await
        .unwrap();
    assert_eq!(failures().len(), 2);

    // What cannot be tested says why.
    assert!(matches!(
        harness
            .manager
            .selftest_browser(persona.id, Vec::new())
            .await,
        Err(PersonaError::NothingToTest(_))
    ));
    let echo = "https://echo.kage.test/".parse().unwrap();
    assert!(matches!(
        harness.manager.selftest_http(persona.id, &echo).await,
        Err(PersonaError::HttpPathOff {
            reason: HttpPathOff::NoProfile,
            ..
        })
    ));
    // While a leak is on record or the proxy is down, nothing is sent.
    harness.fail_into_backoff(proxy.id).await;
    assert!(matches!(
        harness
            .manager
            .selftest_browser(persona.id, pages(&harness.config.selftest.browser, false))
            .await,
        Err(PersonaError::ProxyNotUsable { .. })
    ));
    harness.clock.advance(Span::days(3));
    let before = harness.firefox.with(|state| state.selftested.len());
    assert_eq!(harness.manager.selftests_due().await.unwrap(), 0);
    assert_eq!(harness.firefox.with(|state| state.selftested.len()), before);
}

fn probe_page() -> Vec<SelftestPageConfig> {
    vec![SelftestPageConfig {
        id: "probe".to_owned(),
        kind: SelftestPageKind::Probe,
        url: "https://fixture.kage.test/probe/".to_owned(),
    }]
}

#[tokio::test]
async fn a_persona_moves_with_its_engine_to_a_newer_browser_and_keeps_what_it_has() {
    // The configuration names no major: the engine's own is the one.
    let harness = Harness::with(155, 152, |config| {
        config.personas.mix = only(Engine::Firefox, ClaimedOs::Windows);
        config.engines.firefox.browser_major = None;
        config.selftest.browser.pages = probe_page();
    })
    .await;
    let (proxy, persona) = harness.persona("DE").await;
    assert_eq!(persona.browser_major, 152);
    assert_eq!(persona.wreq_profile, None);
    // What the persona has: a session, cookies in its jar, a record on a
    // site.
    let key = harness.api_key("mover").await;
    let session = harness
        .manager
        .sessions()
        .open(key, Some(id("before-the-move")), |_| true)
        .await
        .unwrap();
    assert_eq!(session.persona.id, persona.id);
    harness
        .store()
        .http_cookies()
        .replace(
            proxy.id,
            Some(persona.id),
            &[stored_cookie("shop.example", "cart")],
        )
        .await
        .unwrap();
    harness
        .manager
        .domains()
        .record_request(&persona, "shop.example")
        .await
        .unwrap();

    // The image moves to Firefox 156. The persona's next open is an
    // operator's self-test: the engine refuses the old document, the
    // sidecar moves it, Kage stores it and opens the context again.
    harness.firefox.with(|state| state.major = 156);
    let run = harness
        .manager
        .selftest_browser(persona.id, pages(&harness.config.selftest.browser, false))
        .await
        .unwrap();
    assert_eq!(run.verdict, Verdict::Passed);
    let (opened, upgraded, tested) = harness.firefox.with(|state| {
        (
            state.opened.clone(),
            state.upgraded.clone(),
            state.selftested.clone(),
        )
    });
    assert_eq!((opened.len(), upgraded.len(), tested.len()), (2, 1, 1));
    assert_eq!(upgraded[0]["fingerprint"], persona.fingerprint);
    assert_eq!(opened[0]["fingerprint"], persona.fingerprint);

    let moved = harness.reload(&persona).await;
    assert_eq!(moved.browser_major, 156);
    assert_eq!(
        moved.user_agent,
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:156.0) Gecko/20100101 Firefox/156.0"
    );
    assert_eq!(moved.fingerprint["browser_version"], json!("156.0.1"));
    assert_eq!(opened[1]["fingerprint"], moved.fingerprint);
    // The device, the place and the profile are the persona's as before.
    assert_eq!(moved.fingerprint["native"], persona.fingerprint["native"]);
    assert_eq!(moved.seed, persona.seed);
    assert_eq!(
        (moved.screen, moved.hardware_concurrency),
        (persona.screen, persona.hardware_concurrency)
    );
    assert_eq!(moved.profile_dir, persona.profile_dir);
    assert_eq!(opened[1]["profile_dir"], json!(persona.profile_dir));
    assert_eq!(moved.wreq_profile, None);
    assert_eq!(run.persona.browser_major, 156);
    // It was tested as what it is now.
    assert_eq!(tested[0]["expected"]["browser_major"], json!(156));
    let events: Vec<_> = harness
        .events_of(&persona)
        .await
        .into_iter()
        .filter(|event| event.kind == "persona_browser_upgraded")
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].details,
        json!({
            "engine": "firefox",
            "from_major": 152,
            "to_major": 156,
            "browser_version": "156.0.1",
            "wreq_profile": null,
            // What the persona was: the way back to the previous image.
            "previous": {
                "fingerprint": persona.fingerprint,
                "user_agent": persona.user_agent,
                "wreq_profile": null,
            },
        })
    );
    // Its session, its jar and its record on the site stay.
    let again = harness
        .manager
        .sessions()
        .open(key, Some(id("before-the-move")), |_| true)
        .await
        .unwrap();
    assert_eq!(again.persona.id, persona.id);
    assert_eq!(again.persona.browser_major, 156);
    let jar = harness
        .store()
        .http_cookies()
        .list(proxy.id, Some(persona.id))
        .await
        .unwrap();
    assert_eq!(jar.len(), 1);
    assert_eq!(jar[0].name, "cart");
    let site = harness
        .store()
        .persona_domain_state()
        .get(persona.id, "shop.example")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(site.requests_today, 1);

    // The next open takes the moved persona as it is: nothing moves twice.
    harness
        .manager
        .selftest_browser(persona.id, pages(&harness.config.selftest.browser, false))
        .await
        .unwrap();
    assert_eq!(harness.firefox.with(|state| state.upgraded.len()), 1);
    assert_eq!(
        harness
            .count_events(&persona, "persona_browser_upgraded")
            .await,
        1
    );

    // A browser never goes back: an image of an older major refuses the
    // moved persona and moves nothing.
    harness.firefox.with(|state| state.major = 152);
    let refused = harness
        .manager
        .selftest_browser(persona.id, pages(&harness.config.selftest.browser, false))
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, PersonaError::Browser(error) if error.is_browser_major_mismatch()),
        "{refused}"
    );
    assert_eq!(harness.reload(&persona).await.browser_major, 156);

    // The operator's way back (docs/runbooks/upgrading-an-engine.md, "Going
    // back", step 3) restores the persona as it was before the move, from
    // the event alone, and the previous image opens it again.
    let runbook = include_str!("../../../docs/runbooks/upgrading-an-engine.md");
    let start = runbook.find("UPDATE personas p").unwrap();
    let end = start + runbook[start..].find("p.browser_major = 156").unwrap();
    let statement = &runbook[start..end + "p.browser_major = 156".len()];
    let restored = sqlx::query(statement)
        .execute(harness.store().pool())
        .await
        .unwrap();
    assert_eq!(restored.rows_affected(), 1);
    let back = harness.reload(&persona).await;
    assert_eq!(
        (
            &back.fingerprint,
            &back.user_agent,
            &back.wreq_profile,
            back.browser_major
        ),
        (
            &persona.fingerprint,
            &persona.user_agent,
            &persona.wreq_profile,
            152
        )
    );
    harness
        .manager
        .selftest_browser(persona.id, pages(&harness.config.selftest.browser, false))
        .await
        .unwrap();
    assert_eq!(
        harness
            .firefox
            .with(|state| state.opened.last().cloned().unwrap())["fingerprint"],
        persona.fingerprint
    );
}

#[tokio::test]
async fn a_move_to_another_major_than_the_configuration_names_is_refused() {
    let harness = Harness::with(155, 152, |config| {
        config.personas.mix = only(Engine::Firefox, ClaimedOs::Macos);
        config.selftest.browser.pages = probe_page();
    })
    .await;
    let (_, persona) = harness.persona("FR").await;
    // The image runs 156 while the configuration still names 152.
    harness.firefox.with(|state| state.major = 156);
    let refused = harness
        .manager
        .selftest_browser(persona.id, pages(&harness.config.selftest.browser, false))
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, PersonaError::Upgrade { persona: moved, reason } if *moved == persona.id && reason.contains("configuration names 152")),
        "{refused}"
    );
    assert_eq!(harness.reload(&persona).await, persona);
    assert_eq!(
        harness
            .count_events(&persona, "persona_browser_upgraded")
            .await,
        0
    );
}

#[tokio::test]
async fn the_copies_kept_before_an_upgrade_go_after_the_backup_window() {
    let harness = Harness::start().await;
    let copies = harness.volume.path().join(".upgrade");
    let copy = |name: &str, note: Option<chrono::DateTime<Utc>>| {
        let dir = copies.join(name);
        std::fs::create_dir_all(dir.join("profile")).unwrap();
        std::fs::write(dir.join("profile").join("cookies.sqlite"), b"kept").unwrap();
        if let Some(at) = note {
            let note =
                json!({ "persona_id": name, "from_major": 152, "copied_at": at.to_rfc3339() });
            std::fs::write(dir.join("upgrade.json"), note.to_string()).unwrap();
        }
        dir
    };
    // No directory of copies: nothing to do.
    assert_eq!(harness.manager.sweep_upgrade_copies().await.unwrap(), 0);

    let now = harness.clock.now();
    let old = copy(
        "0199c1f2-0000-7000-8000-000000000001",
        Some(now - Span::days(8)),
    );
    let young = copy(
        "0199c1f2-0000-7000-8000-000000000002",
        Some(now - Span::days(6)),
    );
    // A copy without a note, and one that was never finished, are as old
    // as their last change: today, by the wall clock.
    let unnoted = copy("0199c1f2-0000-7000-8000-000000000003", None);
    let partial = copy("0199c1f2-0000-7000-8000-000000000004.partial", None);
    std::fs::write(copies.join("stray.txt"), b"not a copy").unwrap();

    assert_eq!(harness.manager.sweep_upgrade_copies().await.unwrap(), 1);
    assert!(!old.exists());
    assert!(young.exists() && unnoted.exists() && partial.exists());

    // A week and more after their last change, by the manager's clock.
    harness.clock.set(Utc::now() + Span::days(8));
    assert_eq!(harness.manager.sweep_upgrade_copies().await.unwrap(), 3);
    assert!(!young.exists() && !unnoted.exists() && !partial.exists());
    assert!(copies.join("stray.txt").exists());
    // The manager's sweep does it as well.
    copy(
        "0199c1f2-0000-7000-8000-000000000005",
        Some(harness.clock.now() - Span::days(30)),
    );
    assert_eq!(harness.manager.sweep().await.upgrade_copies_deleted, 1);
}
