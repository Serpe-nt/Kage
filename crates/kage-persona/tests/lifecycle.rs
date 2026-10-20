//! Generation and life of personas against a real Postgres, an in-process
//! provider and mock sidecars (spec §6.1, §6.3), and the host's own
//! personas under `egress.mode: direct` (§25).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::Duration as Span;
use common::{Harness, capital, geo, tally};
use kage_core::alert::AlertKind;
use kage_core::config::EgressMode;
use kage_core::domain::{
    ClaimedOs, CoolingReason, Engine, Persona, PersonaState, ProxyProviderKind, ProxyState,
    SessionId,
};
use kage_core::ids::PersonaId;
use kage_core::persona::mix::MixEntry;
use kage_core::persona::{self, place, screen};
use kage_persona::lifecycle::GeoChange;
use kage_persona::manager::{REASON_OPERATOR, REASON_PROXY_RETIRED, REASON_REGENERATED};
use kage_persona::{PersonaError, PersonaEvent, PersonaManager, PersonaParts, Sidecars};
use kage_proxy::Clock;
use kage_proxy::health::GeoLookup;
use kage_proxy::machine::{ExitObservation, ProbeReport};
use serde_json::json;

const COUNTRIES: [&str; 5] = ["AT", "DE", "FR", "GB", "IT"];

fn zone_of(country: &str) -> &'static str {
    match country {
        "AT" => "Europe/Vienna",
        "DE" => "Europe/Berlin",
        "FR" => "Europe/Paris",
        "GB" => "Europe/London",
        _ => "Europe/Rome",
    }
}

#[tokio::test]
async fn under_direct_the_host_has_personas_of_its_own_and_the_pools_wait() {
    let harness = Harness::start_with(|config| {
        config.egress.mode = EgressMode::Direct;
        config.egress.direct.personas = 2;
        config.egress.direct.country = Some("AT".parse().unwrap());
        config.egress.direct.locale = Some("de-AT".to_owned());
        config.egress.direct.engines = Some(vec![Engine::Firefox]);
    })
    .await;
    // A proxy of the pool gets no persona: nothing leaves through it.
    let pooled = harness.healthy_proxy("DE").await;
    assert_eq!(harness.manager.ensure(pooled.id).await.unwrap(), None);

    // The host's rows do, as declared: Austria, German, Firefox only.
    let rows = harness
        .pool
        .keep_host_rows(2, Some("AT".parse().unwrap()))
        .await
        .unwrap();
    let report = harness.manager.reconcile().await.unwrap();
    assert_eq!(report.generated.len(), 2, "{report:?}");
    let mut hosts = Vec::new();
    for row in &rows {
        let persona = harness
            .store()
            .personas()
            .get_by_proxy(row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persona.engine, Engine::Firefox);
        assert_eq!(
            persona
                .country
                .map(|country| country.to_string())
                .as_deref(),
            Some("AT")
        );
        assert_eq!(persona.locale, "de-AT");
        assert_eq!(persona.languages, ["de", "en-US", "en"]);
        assert_eq!(persona.timezone, "Europe/Vienna");
        let (proxy, _) = harness.manager.route(&persona).await.unwrap();
        assert_eq!(proxy.provider, ProxyProviderKind::Host);
        hosts.push(persona);
    }

    // A lookup that disagrees counts against the declared place and
    // changes nobody's identity.
    let at = harness.clock.now();
    let berlin = GeoLookup {
        country: "DE".parse().unwrap(),
        city: Some("Berlin".to_owned()),
        region: Some("Berlin".to_owned()),
        asn: Some("AS3320".to_owned()),
        org: None,
        timezone: Some("Europe/Berlin".to_owned()),
        loc: Some("52.5200,13.4050".to_owned()),
    };
    let ip = "203.0.113.77".parse().unwrap();
    harness
        .pool
        .record_host_place(rows[0].id, ip, berlin, at)
        .await
        .unwrap()
        .unwrap();
    let followed = harness.manager.ensure(rows[0].id).await.unwrap().unwrap();
    assert_eq!(
        followed
            .country
            .map(|country| country.to_string())
            .as_deref(),
        Some("AT")
    );
    assert_eq!(
        (followed.locale.as_str(), followed.timezone.as_str()),
        ("de-AT", "Europe/Vienna")
    );
    assert_eq!(followed.state, hosts[0].state);

    // A leak of the pool stops nothing that leaves from the host.
    harness
        .pool
        .record_probe(
            pooled.id,
            &ProbeReport {
                at,
                exit: Ok(ExitObservation {
                    ip,
                    latency_ms: 90,
                    connect_ms: None,
                    tls_ms: None,
                }),
                leak: Some(true),
                geo: None,
            },
        )
        .await
        .unwrap();
    assert!(harness.pool.egress_unsafe().await.unwrap());
    harness.manager.route(&hosts[1]).await.unwrap();
}

#[tokio::test]
async fn under_the_pool_the_hosts_own_rows_get_no_persona() {
    let harness = Harness::start().await;
    let (_, pooled) = harness.persona("DE").await;
    let rows = harness.pool.keep_host_rows(1, None).await.unwrap();

    assert_eq!(harness.manager.ensure(rows[0].id).await.unwrap(), None);
    assert_eq!(harness.manager.reconcile().await.unwrap().generated, []);
    assert!(harness.manager.route(&pooled).await.is_ok());
}

#[tokio::test]
async fn a_usable_proxy_gets_a_persona_derived_from_its_observed_geo() {
    let harness = Harness::start().await;
    let mut events = harness.manager.subscribe();

    // A proxy that was never probed has no persona yet.
    let proxy = harness.add_proxy("AT").await;
    assert_eq!(proxy.state, ProxyState::New);
    assert_eq!(harness.manager.ensure(proxy.id).await.unwrap(), None);
    assert!(matches!(
        harness.manager.generate(&proxy).await,
        Err(PersonaError::ProxyNotUsable {
            state: ProxyState::New,
            ..
        })
    ));

    // The provider says Austria; the lookup places the address in
    // Marseille. The observed record decides everything.
    let now = harness.clock.now();
    let observed = geo("FR", "Marseille", "Europe/Paris", "43.2965,5.3698", now);
    let proxy = harness.probe_ok(proxy.id, Some(observed)).await;
    assert_eq!(proxy.state, ProxyState::Healthy);
    let persona = harness.manager.ensure(proxy.id).await.unwrap().unwrap();

    assert_eq!(persona.proxy_id, proxy.id);
    assert_eq!(persona.country.unwrap().as_str(), "FR");
    assert_eq!(persona.timezone, "Europe/Paris");
    assert_eq!(persona.locale, "fr-FR");
    let stock: &[&str] = match persona.engine {
        Engine::Chromium => &["fr-FR", "fr", "en-US", "en"],
        Engine::Firefox => &["fr", "fr-FR", "en-US", "en"],
    };
    assert_eq!(persona.languages, stock);
    let position = persona.geolocation.unwrap();
    let off = place::distance_km((43.2965, 5.3698), (position.latitude, position.longitude));
    assert!((0.5..=4.0).contains(&off), "{off} km from the centre");

    // Everything hangs on the seed, which hangs on the id.
    assert_eq!(persona.seed, persona::seed_for(persona.id));
    assert_eq!(persona.tables_version, persona::TABLES_VERSION);
    assert_eq!(
        persona.profile_dir,
        persona::profile_dir(persona.engine, persona.id)
    );
    assert_eq!(persona.state, PersonaState::Active);
    assert_eq!(persona.created_at, now);
    // Warm-up is off in tests: ready at once.
    assert_eq!(persona.warmed_at, Some(now));
    assert!(persona.is_ready());
    match persona.engine {
        Engine::Chromium => {
            assert_eq!(persona.claimed_os, ClaimedOs::Linux);
            assert_eq!(persona.browser_major, 155);
        }
        Engine::Firefox => {
            assert!(matches!(
                persona.claimed_os,
                ClaimedOs::Windows | ClaimedOs::Macos
            ));
            assert_eq!(persona.browser_major, 156);
        }
    }

    // The sidecar was asked for exactly this persona: its seed, its
    // system, its languages, the engine's major, and one screen class of
    // the tables by exact bounds.
    let class = screen::draw(persona.engine, persona.claimed_os, &persona.seed).unwrap();
    assert_eq!(
        (persona.screen.width, persona.screen.height),
        (class.width, class.height)
    );
    let asked = harness
        .sidecar(persona.engine)
        .with(|state| state.generated.clone());
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["seed"], json!(persona.seed.to_string()));
    assert_eq!(asked[0]["claimed_os"], json!(persona.claimed_os.as_str()));
    assert_eq!(asked[0]["languages"], json!(persona.languages));
    assert_eq!(asked[0]["browser_major"], json!(persona.browser_major));
    assert_eq!(
        asked[0]["screen"],
        json!({
            "min_width": class.width, "max_width": class.width,
            "min_height": class.height, "max_height": class.height,
        })
    );
    // What the sidecar generated is stored whole.
    assert_eq!(persona.fingerprint["seed"], json!(persona.seed.to_string()));
    assert_eq!(
        persona.fingerprint["native"]["hardware_concurrency"],
        json!(persona.hardware_concurrency)
    );
    assert!(
        persona
            .user_agent
            .contains(&persona.browser_major.to_string())
    );
    assert_eq!(harness.reload(&persona).await, persona);

    // Neither engine of this release has an emulation profile: the
    // persona is browser-only and the alert says so, once.
    assert_eq!(persona.wreq_profile, None);
    assert_eq!(harness.alerts.kinds(), [AlertKind::HttpProfileMissing]);
    assert!(harness.manager.directory().is_empty());

    // The record and the events.
    let stored = harness.events_of(&persona).await;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].kind, "persona_generated");
    assert_eq!(stored[0].details["timezone_source"], json!("observed"));
    assert_eq!(stored[0].details["country"], json!("FR"));
    assert!(!stored[0].details.to_string().contains("secret-pass"));
    assert_eq!(
        events.try_recv().unwrap(),
        PersonaEvent::Generated {
            persona: persona.id,
            proxy: proxy.id
        }
    );
    assert_eq!(
        events.try_recv().unwrap(),
        PersonaEvent::Warmed {
            persona: persona.id
        }
    );

    // One persona per proxy: asking again answers with the same one.
    assert_eq!(
        harness.manager.ensure(proxy.id).await.unwrap().unwrap(),
        persona
    );
    assert_eq!(harness.manager.generate(&proxy).await.unwrap(), persona);
    assert_eq!(
        harness
            .sidecar(persona.engine)
            .with(|state| state.generated.len()),
        1
    );

    // No lookup has succeeded yet: the provider's country is the
    // fallback, its primary zone the time zone, and there is no position.
    let unplaced = harness.add_proxy("IT").await;
    let unplaced = harness.probe_ok(unplaced.id, None).await;
    let fallback = harness.manager.ensure(unplaced.id).await.unwrap().unwrap();
    assert_eq!(fallback.country.unwrap().as_str(), "IT");
    assert_eq!(fallback.timezone, "Europe/Rome");
    assert_eq!(fallback.locale, "it-IT");
    assert_eq!(fallback.geolocation, None);
    let generated = &harness.events_of(&fallback).await[0];
    assert_eq!(generated.details["timezone_source"], json!("country"));
    assert_eq!(generated.details["position"], json!(false));
    // Still one alert: the missing profile is said once per engine and
    // major, and both personas may well be of the same engine.
    assert!(harness.alerts.kinds().len() <= 2);
}

/// One pool of twenty proxies, four in each of five countries.
async fn pool_of_twenty(harness: &Harness) -> Vec<Persona> {
    let mut refs = Vec::new();
    for country in COUNTRIES {
        for _ in 0..4 {
            refs.push((harness.provider.add(country), country));
        }
    }
    harness.pool.reconcile().await.unwrap();
    let live = harness.pool.live().await.unwrap();
    let now = harness.clock.now();
    let mut personas = Vec::new();
    for (provider_ref, country) in refs {
        let proxy = live
            .iter()
            .find(|proxy| proxy.provider_ref == provider_ref)
            .unwrap();
        harness
            .probe_ok(proxy.id, Some(capital(country, now)))
            .await;
        personas.push(harness.manager.ensure(proxy.id).await.unwrap().unwrap());
    }
    personas
}

#[tokio::test]
async fn twenty_proxies_get_twenty_personas_in_the_configured_mix_with_varied_hardware() {
    const RUNS: usize = 8;
    let harness = Harness::start().await;
    let mut within_one = 0;
    let mut varied_screens = 0;
    let mut varied_processors = 0;
    let mut all = Vec::new();

    for run in 0..RUNS {
        let personas = pool_of_twenty(&harness).await;
        assert_eq!(personas.len(), 20);
        assert_eq!(
            personas
                .iter()
                .map(|persona| persona.proxy_id)
                .collect::<BTreeSet<_>>()
                .len(),
            20
        );

        // The default mix is 40 % chromium on Linux, 45 % firefox on
        // Windows, 15 % firefox on macOS: 8, 9 and 3 of twenty.
        let counts = tally(
            personas
                .iter()
                .map(|persona| (persona.engine.as_str(), persona.claimed_os.as_str())),
        );
        let got = [
            counts.get(&("chromium", "linux")).copied().unwrap_or(0),
            counts.get(&("firefox", "windows")).copied().unwrap_or(0),
            counts.get(&("firefox", "macos")).copied().unwrap_or(0),
        ];
        assert_eq!(got.iter().sum::<usize>(), 20, "run {run}: {counts:?}");
        let off = got
            .iter()
            .zip([8_usize, 9, 3])
            .map(|(got, want)| got.abs_diff(want))
            .max()
            .unwrap();
        assert!(off <= 2, "run {run}: {got:?}");
        if off <= 1 {
            within_one += 1;
        }

        for country in COUNTRIES {
            let here: Vec<&Persona> = personas
                .iter()
                .filter(|persona| persona.country.unwrap().as_str() == country)
                .collect();
            assert_eq!(here.len(), 4);
            // No country is all one browser.
            let engines: BTreeSet<&str> =
                here.iter().map(|persona| persona.engine.as_str()).collect();
            assert_eq!(engines.len(), 2, "run {run}, {country}");
            // Place and language are the country's.
            for persona in here {
                assert_eq!(persona.timezone, zone_of(country));
                assert!(
                    persona.locale.ends_with(&format!("-{country}")),
                    "{} in {country}",
                    persona.locale
                );
                assert!(persona.geolocation.is_some());
            }
        }

        // Screens come from the tables, each asked for by exact bounds;
        // processors are the generator's own.
        for persona in &personas {
            assert!(
                screen::classes(persona.engine, persona.claimed_os)
                    .iter()
                    .any(|class| (class.width, class.height)
                        == (persona.screen.width, persona.screen.height)),
                "{:?}",
                persona.screen
            );
        }
        let screens: BTreeSet<(u32, u32)> = personas
            .iter()
            .map(|persona| (persona.screen.width, persona.screen.height))
            .collect();
        if screens.len() >= 3 {
            varied_screens += 1;
        }
        let processors: BTreeSet<u32> = personas
            .iter()
            .map(|persona| persona.hardware_concurrency)
            .collect();
        if processors.len() >= 3 {
            varied_processors += 1;
        }

        // The next run starts from an empty pool.
        for persona in &personas {
            harness
                .manager
                .retire(persona.id, REASON_OPERATOR)
                .await
                .unwrap();
        }
        all.extend(personas);
    }

    // Over repeated runs: nearly always within one persona of the mix,
    // and the shares of all personas together are the configured ones.
    assert!(within_one >= RUNS - 1, "{within_one} of {RUNS} within one");
    assert!(varied_screens >= RUNS - 1, "{varied_screens} of {RUNS}");
    assert!(
        varied_processors >= RUNS - 1,
        "{varied_processors} of {RUNS}"
    );
    let share = |engine: Engine, os: ClaimedOs| {
        let count = all
            .iter()
            .filter(|persona| persona.engine == engine && persona.claimed_os == os)
            .count();
        #[allow(clippy::cast_precision_loss)]
        let share = count as f64 / all.len() as f64;
        share
    };
    assert!((share(Engine::Chromium, ClaimedOs::Linux) - 0.40).abs() < 0.03);
    assert!((share(Engine::Firefox, ClaimedOs::Windows) - 0.45).abs() < 0.03);
    assert!((share(Engine::Firefox, ClaimedOs::Macos) - 0.15).abs() < 0.03);
    // No two personas ever share a seed, a profile directory or a place.
    let seeds: BTreeSet<String> = all.iter().map(|persona| persona.seed.to_string()).collect();
    assert_eq!(seeds.len(), all.len());
    let positions: BTreeSet<String> = all
        .iter()
        .map(|persona| format!("{:?}", persona.geolocation))
        .collect();
    assert_eq!(positions.len(), all.len());
}

#[tokio::test]
async fn a_persona_cools_with_its_proxy_and_follows_its_geo_record() {
    // Failing probes are minutes apart: the session of this test must
    // not end for being idle meanwhile.
    let harness = Harness::start_with(|config| {
        config.sessions.idle_ttl = kage_core::duration::HumanDuration::from_hours(3);
    })
    .await;
    let mut pool_events = harness.pool.subscribe();
    let proxy = harness.healthy_proxy("DE").await;
    harness.pump(&mut pool_events).await;
    let persona = harness
        .store()
        .personas()
        .get_by_proxy(proxy.id)
        .await
        .unwrap()
        .expect("the pool's events generated the persona");
    let mut events = harness.manager.subscribe();
    let key = harness.api_key("caller").await;
    let sessions = harness.manager.sessions();
    let open = sessions
        .open(key, Some(SessionId::new("open").unwrap()), |_| true)
        .await
        .unwrap();
    assert_eq!(open.persona.id, persona.id);

    // The proxy fails into backoff: the persona cools. It gets no new
    // session; the open one goes on.
    harness.fail_into_backoff(proxy.id).await;
    harness.pump(&mut pool_events).await;
    let cooling = harness.reload(&persona).await;
    assert_eq!(cooling.state, PersonaState::Cooling);
    assert_eq!(cooling.cooling_reason, Some(CoolingReason::ProxyBackoff));
    assert!(!cooling.is_ready());
    assert_eq!(
        events.try_recv().unwrap(),
        PersonaEvent::StateChanged {
            persona: persona.id,
            from: PersonaState::Active,
            to: PersonaState::Cooling
        }
    );
    assert!(sessions.open(key, None, |_| true).await.is_err());
    assert!(
        sessions
            .resolve(key, &open.session.id)
            .await
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        harness.manager.route(&cooling).await,
        Err(PersonaError::ProxyNotUsable {
            state: ProxyState::Backoff,
            ..
        })
    ));

    // The proxy answers again: the persona is active again.
    let now = harness.clock.now();
    harness.recover(proxy.id, Some(capital("DE", now))).await;
    harness.pump(&mut pool_events).await;
    let active = harness.reload(&persona).await;
    assert_eq!(active.state, PersonaState::Active);
    assert_eq!(active.cooling_reason, None);
    assert_eq!(
        harness
            .count_events(&persona, "persona_state_changed")
            .await,
        2
    );
    while events.try_recv().is_ok() {}

    // The lookup moves the address to Munich: same country, same zone;
    // the position follows and nothing else does.
    harness.clock.advance(Span::hours(25));
    let now = harness.clock.now();
    harness
        .probe_ok(
            proxy.id,
            Some(geo("DE", "Munich", "Europe/Berlin", "48.1374,11.5755", now)),
        )
        .await;
    harness.pump(&mut pool_events).await;
    let munich = harness.reload(&persona).await;
    assert_eq!(munich.state, PersonaState::Active);
    assert_eq!(munich.timezone, "Europe/Berlin");
    assert_eq!(munich.locale, persona.locale);
    assert_eq!(munich.languages, persona.languages);
    assert!((munich.geolocation.unwrap().latitude - 48.1374).abs() < 0.05);
    assert_eq!(
        events.try_recv().unwrap(),
        PersonaEvent::GeoUpdated {
            persona: persona.id,
            change: GeoChange::Place
        }
    );
    let moved = harness.events_of(&persona).await;
    let moved = moved.last().unwrap();
    assert_eq!(moved.kind, "persona_geo_updated");
    assert_eq!(moved.severity.as_str(), "info");
    assert_eq!(moved.details["change"], json!("place"));

    // The lookup moves it across a border: time zone, position, locale
    // and languages follow, the event is a warning, and the persona cools
    // until a self-test passes. What makes the persona never changes.
    harness.clock.advance(Span::hours(25));
    let now = harness.clock.now();
    harness
        .probe_ok(
            proxy.id,
            Some(geo("IT", "Milan", "Europe/Rome", "45.4643,9.1895", now)),
        )
        .await;
    harness.pump(&mut pool_events).await;
    let milan = harness.reload(&persona).await;
    assert_eq!(milan.country.unwrap().as_str(), "IT");
    assert_eq!(milan.timezone, "Europe/Rome");
    assert_eq!(milan.locale, "it-IT");
    assert_eq!(milan.languages, ["it-IT", "it", "en-US", "en"]);
    assert!((milan.geolocation.unwrap().longitude - 9.1895).abs() < 0.07);
    assert_eq!(milan.state, PersonaState::Cooling);
    assert_eq!(milan.cooling_reason, Some(CoolingReason::GeoChanged));
    assert_eq!(
        (
            milan.seed,
            &milan.fingerprint,
            milan.screen,
            &milan.profile_dir
        ),
        (
            persona.seed,
            &persona.fingerprint,
            persona.screen,
            &persona.profile_dir
        )
    );
    assert_eq!(
        events.try_recv().unwrap(),
        PersonaEvent::GeoUpdated {
            persona: persona.id,
            change: GeoChange::Identity
        }
    );
    let crossed = harness.events_of(&persona).await;
    let crossed = crossed
        .iter()
        .rev()
        .find(|event| event.kind == "persona_geo_updated")
        .unwrap();
    assert_eq!(crossed.severity.as_str(), "warning");
    assert_eq!(crossed.details["from"]["locale"], json!("de-DE"));
    assert_eq!(crossed.details["to"]["locale"], json!("it-IT"));
    assert_eq!(crossed.details["cooling"], json!("geo_changed"));

    // The proxy coming and going does not end that cooling.
    harness.fail_into_backoff(proxy.id).await;
    harness.pump(&mut pool_events).await;
    let now = harness.clock.now();
    harness
        .recover(
            proxy.id,
            Some(geo("IT", "Milan", "Europe/Rome", "45.4643,9.1895", now)),
        )
        .await;
    harness.pump(&mut pool_events).await;
    assert!(
        harness
            .pool
            .get(proxy.id)
            .await
            .unwrap()
            .unwrap()
            .state
            .is_usable()
    );
    let still = harness.reload(&persona).await;
    assert_eq!(still.state, PersonaState::Cooling);
    assert_eq!(still.cooling_reason, Some(CoolingReason::GeoChanged));
}

#[tokio::test]
async fn replacing_a_proxy_retires_its_persona_and_its_profile_is_archived_then_deleted() {
    let harness = Harness::with(149, 151, |_| {}).await;
    let mut pool_events = harness.pool.subscribe();
    let old_proxy = harness.healthy_proxy("DE").await;
    let keep_proxy = harness.healthy_proxy("FR").await;
    harness.pump(&mut pool_events).await;
    let personas = harness.store().personas();
    let old = personas.get_by_proxy(old_proxy.id).await.unwrap().unwrap();
    let kept = personas.get_by_proxy(keep_proxy.id).await.unwrap().unwrap();
    // These engines have emulation profiles: the personas are on the
    // http path and in the probe's directory.
    assert!(old.wreq_profile.is_some());
    assert_eq!(
        harness
            .manager
            .directory()
            .spec(old_proxy.id)
            .unwrap()
            .persona,
        Some(old.id)
    );

    // What the old persona leaves behind: a session, http cookies, state
    // on a domain, and a browser profile on the volume.
    let key = harness.api_key("caller").await;
    let sessions = harness.manager.sessions();
    let session = sessions
        .open(key, Some(SessionId::new("old").unwrap()), |persona| {
            persona.id == old.id
        })
        .await
        .unwrap();
    let (_, endpoint) = harness.manager.route(&old).await.unwrap();
    let spec = kage_persona::identity::http_spec(&old).unwrap();
    let client = harness.http.client(&spec, &endpoint).await.unwrap();
    client.jar().store(
        &"https://shop.example/".parse().unwrap(),
        "sid=old-persona; Path=/; Max-Age=86400",
        chrono::Utc::now(),
    );
    client.persist().await;
    let cookies = harness.store().http_cookies();
    assert_eq!(cookies.count(old_proxy.id).await.unwrap(), 1);
    harness
        .manager
        .domains()
        .record_request(&old, "shop.example")
        .await
        .unwrap();
    let profile = harness.in_volume(&old.profile_dir);
    std::fs::create_dir_all(profile.join("storage")).unwrap();
    std::fs::write(profile.join("storage").join("cookies.sqlite"), b"profile").unwrap();

    // The provider swaps the address: the old one is gone from its list
    // and a new one is there.
    harness.provider.remove(&old_proxy.provider_ref);
    let new_ref = harness.provider.add("DE");
    harness.pool.reconcile().await.unwrap();
    let seen = harness.pump(&mut pool_events).await;
    assert!(seen.iter().any(|event| matches!(
        event,
        kage_proxy::PoolEvent::StateChanged { proxy, to: ProxyState::Retired, .. }
            if *proxy == old_proxy.id
    )));

    // The persona is retired with it; its session has ended and its
    // cookies are deleted at once.
    let retired = harness.reload(&old).await;
    assert_eq!(retired.state, PersonaState::Retired);
    assert_eq!(retired.retired_at, Some(harness.clock.now()));
    assert_eq!(retired.retire_reason.as_deref(), Some(REASON_PROXY_RETIRED));
    assert_eq!(
        sessions.resolve(key, &session.session.id).await.unwrap(),
        None
    );
    assert_eq!(cookies.count(old_proxy.id).await.unwrap(), 0);
    assert_eq!(harness.manager.directory().spec(old_proxy.id), None);
    let event = harness.events_of(&old).await;
    let event = event.last().unwrap();
    assert_eq!(event.kind, "persona_retired");
    assert_eq!(event.details["sessions_ended"], json!(1));
    assert_eq!(event.details["http_cookies_deleted"], json!(1));
    assert!(matches!(
        harness.manager.route(&retired).await,
        Err(PersonaError::Retired(id)) if id == old.id
    ));
    // The other persona is untouched.
    assert_eq!(harness.reload(&kept).await, kept);

    // The replacement gets a new persona once it is usable, and it
    // inherits nothing.
    let new_proxy = harness
        .pool
        .live()
        .await
        .unwrap()
        .into_iter()
        .find(|proxy| proxy.provider_ref == new_ref)
        .unwrap();
    assert_eq!(personas.get_by_proxy(new_proxy.id).await.unwrap(), None);
    let now = harness.clock.now();
    harness
        .probe_ok(new_proxy.id, Some(capital("DE", now)))
        .await;
    harness.pump(&mut pool_events).await;
    let new = personas.get_by_proxy(new_proxy.id).await.unwrap().unwrap();
    assert_ne!(new.id, old.id);
    assert_ne!(new.seed, old.seed);
    assert_ne!(new.profile_dir, old.profile_dir);
    assert_ne!(new.fingerprint, old.fingerprint);
    assert_eq!(cookies.count(new_proxy.id).await.unwrap(), 0);
    assert_eq!(
        harness
            .store()
            .persona_domain_state()
            .list_for_persona(new.id)
            .await
            .unwrap(),
        []
    );
    assert_eq!(personas.list_live().await.unwrap().len(), 2);

    // The profile stays where it is while a browser may still have it
    // open, and is archived ten minutes after the retirement.
    assert_eq!(harness.manager.archive_due().await.unwrap(), (0, 0));
    harness.clock.advance(Span::minutes(9));
    assert_eq!(harness.manager.archive_due().await.unwrap(), (0, 0));
    assert!(profile.exists());
    harness.clock.advance(Span::minutes(2));
    assert_eq!(harness.manager.archive_due().await.unwrap(), (1, 0));
    let archive = harness.in_volume(&persona::archive_dir(old.id));
    assert!(!profile.exists());
    assert_eq!(
        std::fs::read(archive.join("storage").join("cookies.sqlite")).unwrap(),
        b"profile"
    );
    let description = std::fs::read_to_string(archive.join("persona.json")).unwrap();
    let described: serde_json::Value = serde_json::from_str(&description).unwrap();
    assert_eq!(described["persona_id"], json!(old.id));
    assert_eq!(described["proxy_id"], json!(old_proxy.id));
    assert_eq!(described["proxy"], json!("127.0.0.1:9"));
    assert_eq!(described["engine"], json!(old.engine.as_str()));
    assert_eq!(described["retire_reason"], json!(REASON_PROXY_RETIRED));
    // The address, never the login.
    assert!(!description.contains("secret-pass"), "{description}");
    assert!(!description.contains("user-"), "{description}");
    let archived = harness.reload(&old).await;
    assert_eq!(archived.profile_archived_at, Some(harness.clock.now()));
    assert_eq!(archived.profile_archive, Some(persona::archive_dir(old.id)));
    assert_eq!(archived.profile_deleted_at, None);
    // Archiving happens once.
    assert_eq!(harness.manager.archive_due().await.unwrap(), (0, 0));

    // The archive is kept for the retention and then removed whole.
    harness.clock.advance(Span::days(6));
    assert_eq!(harness.manager.archive_due().await.unwrap(), (0, 0));
    assert!(archive.exists());
    harness.clock.advance(Span::days(1) + Span::minutes(1));
    assert_eq!(harness.manager.archive_due().await.unwrap(), (0, 1));
    assert!(!archive.exists());
    let deleted = harness.reload(&old).await;
    assert_eq!(deleted.profile_deleted_at, Some(harness.clock.now()));
    assert_eq!(harness.manager.archive_due().await.unwrap(), (0, 0));
    assert_eq!(
        harness.count_events(&old, "persona_profile_archived").await,
        1
    );
    assert_eq!(
        harness.count_events(&old, "persona_profile_deleted").await,
        1
    );

    // The row, its fingerprint and what it did on a domain stay.
    assert_eq!(deleted.fingerprint, old.fingerprint);
    assert_eq!(
        harness
            .store()
            .persona_domain_state()
            .list_for_persona(old.id)
            .await
            .unwrap()
            .len(),
        1
    );
    // The live personas' profiles were never touched.
    assert_eq!(harness.reload(&new).await.profile_archived_at, None);
}

#[tokio::test]
async fn an_operator_retires_and_regenerates_and_the_sweep_catches_what_events_missed() {
    let harness = Harness::start().await;
    let (proxy, first) = harness.persona("GB").await;

    // Regenerate: the old persona is retired, the proxy gets a new one.
    let second = harness.manager.regenerate(first.id).await.unwrap();
    assert_ne!(second.id, first.id);
    assert_eq!(second.proxy_id, proxy.id);
    assert_eq!(second.state, PersonaState::Active);
    let retired = harness.reload(&first).await;
    assert_eq!(retired.state, PersonaState::Retired);
    assert_eq!(retired.retire_reason.as_deref(), Some(REASON_REGENERATED));
    // A retired persona is not regenerated again, an unknown one neither.
    assert!(matches!(
        harness.manager.regenerate(first.id).await,
        Err(PersonaError::Retired(id)) if id == first.id
    ));
    let nobody = PersonaId::new();
    assert!(matches!(
        harness.manager.regenerate(nobody).await,
        Err(PersonaError::NotFound(id)) if id == nobody
    ));
    assert!(matches!(
        harness.manager.retire(nobody, REASON_OPERATOR).await,
        Err(PersonaError::NotFound(_))
    ));

    // While the proxy cannot carry traffic nothing is regenerated, and
    // nothing is retired for it.
    harness.fail_into_backoff(proxy.id).await;
    assert!(matches!(
        harness.manager.regenerate(second.id).await,
        Err(PersonaError::ProxyNotUsable { .. })
    ));
    assert!(harness.reload(&second).await.is_live());

    // Retire by hand: final, and saying it twice changes nothing.
    let gone = harness
        .manager
        .retire(second.id, REASON_OPERATOR)
        .await
        .unwrap();
    assert_eq!(gone.state, PersonaState::Retired);
    assert_eq!(gone.retire_reason.as_deref(), Some(REASON_OPERATOR));
    let again = harness
        .manager
        .retire(second.id, "something else")
        .await
        .unwrap();
    assert_eq!(again, gone);
    assert_eq!(harness.count_events(&second, "persona_retired").await, 1);

    // Nothing announced any of the following; the sweep finds it in the
    // store. The proxy without a persona gets one once it is usable.
    let report = harness.manager.sweep().await;
    assert_eq!(report.reconcile.generated, []);
    let now = harness.clock.now();
    harness.recover(proxy.id, Some(capital("GB", now))).await;
    let other = harness.healthy_proxy("FR").await;
    let report = harness.manager.sweep().await;
    assert_eq!(report.reconcile.generated.len(), 2);
    assert_eq!(report.reconcile.failed, []);
    let third = harness
        .store()
        .personas()
        .get_by_proxy(proxy.id)
        .await
        .unwrap()
        .unwrap();
    let fourth = harness
        .store()
        .personas()
        .get_by_proxy(other.id)
        .await
        .unwrap()
        .unwrap();

    // A proxy that went into backoff unseen cools its persona, and one
    // that left the pool unseen retires its persona.
    harness.fail_into_backoff(proxy.id).await;
    harness.provider.remove(&other.provider_ref);
    harness.provider.add("FR");
    harness.pool.reconcile().await.unwrap();
    let report = harness.manager.sweep().await;
    assert_eq!(report.reconcile.retired, [fourth.id]);
    assert_eq!(harness.reload(&third).await.state, PersonaState::Cooling);
    assert_eq!(harness.reload(&fourth).await.state, PersonaState::Retired);

    // Where the volume is out of reach nothing is archived, and the row
    // says so instead of waiting for ever.
    // (The first persona never had a profile on the volume: the same.)
    let never_opened = harness.reload(&first).await;
    assert!(never_opened.profile_archived_at.is_some());
    assert_eq!(never_opened.profile_archive, None);
    std::fs::remove_dir_all(harness.volume.path()).unwrap();
    harness.clock.advance(Span::minutes(11));
    let report = harness.manager.sweep().await;
    assert_eq!((report.archived, report.deleted), (0, 0));
    let unarchived = harness.reload(&fourth).await;
    assert_eq!(unarchived.profile_archived_at, Some(harness.clock.now()));
    assert_eq!(unarchived.profile_archive, None);
    assert_eq!(unarchived.profile_deleted_at, None);
    harness.clock.advance(Span::days(8));
    let report = harness.manager.sweep().await;
    assert_eq!((report.archived, report.deleted), (0, 0));
    assert_eq!(
        harness.reload(&fourth).await.profile_deleted_at,
        Some(harness.clock.now())
    );
}

#[tokio::test]
async fn a_persona_that_cannot_be_generated_raises_an_alert_and_is_tried_again() {
    let harness = Harness::start().await;
    // Only firefox on Windows: every generation asks the firefox sidecar.
    let config = {
        let mut config = harness.config.clone();
        config.personas.mix = vec![MixEntry {
            engine: Engine::Firefox,
            os: ClaimedOs::Windows,
            weight: 1,
        }];
        config
    };
    let manager = PersonaManager::new(PersonaParts {
        store: harness.store().clone(),
        pool: harness.pool.clone(),
        sidecars: Sidecars::from_config(&config).unwrap(),
        http: harness.http.clone(),
        alerts: harness.alerts.clone(),
        config: config.clone(),
        echo_trust_anchor: None,
    })
    .unwrap();
    let failures = || {
        harness
            .alerts
            .alerts()
            .into_iter()
            .filter(|alert| alert.kind == AlertKind::PersonaGenerationFailed)
            .collect::<Vec<_>>()
    };
    let proxy = harness.healthy_proxy("DE").await;

    // The container is full: time may cure that, so two failures are
    // only logged and the third raises the alert, once.
    harness.firefox.with(|state| {
        state.refuse_generate = Some((
            503,
            json!({ "code": "context_limit", "message": "this container already runs 3 contexts" }),
        ));
    });
    for expected in [0, 0, 1, 1] {
        let error = manager.ensure(proxy.id).await.unwrap_err();
        assert!(error.is_transient(), "{error}");
        assert_eq!(failures().len(), expected);
    }
    let alert = &failures()[0];
    assert_eq!(alert.subject, Some(proxy.id.as_uuid()));
    assert!(alert.message.contains(&proxy.id.to_string()));
    let report = manager.reconcile().await.unwrap();
    assert_eq!(report.failed, [proxy.id]);
    assert_eq!(report.generated, []);
    assert_eq!(
        harness
            .store()
            .personas()
            .get_by_proxy(proxy.id)
            .await
            .unwrap(),
        None
    );

    // The sidecar answers again: the persona exists and a later failure
    // would be said again.
    harness.firefox.with(|state| state.refuse_generate = None);
    let persona = manager.ensure(proxy.id).await.unwrap().unwrap();
    assert_eq!(
        (persona.engine, persona.claimed_os),
        (Engine::Firefox, ClaimedOs::Windows)
    );

    // A sidecar that generates something else than it was asked for is
    // not a matter of time: the alert is raised at once and nothing is
    // stored.
    let second = harness.healthy_proxy("DE").await;
    harness.firefox.with(|state| state.wrong_seed = true);
    let error = manager.ensure(second.id).await.unwrap_err();
    assert!(
        matches!(&error, PersonaError::Generation { engine: Engine::Firefox, reason } if reason.contains("another seed")),
        "{error}"
    );
    assert!(!error.is_transient());
    assert_eq!(failures().len(), 2);
    assert_eq!(
        harness
            .store()
            .personas()
            .get_by_proxy(second.id)
            .await
            .unwrap(),
        None
    );

    // A mix no persona can be drawn from is refused before anything
    // runs: a chromium persona claims the system its Chrome runs on.
    let mut cross = config.clone();
    cross.personas.mix = vec![MixEntry {
        engine: Engine::Chromium,
        os: ClaimedOs::Windows,
        weight: 1,
    }];
    let refused = PersonaManager::new(PersonaParts {
        store: harness.store().clone(),
        pool: harness.pool.clone(),
        sidecars: Sidecars::from_config(&cross).unwrap(),
        http: harness.http.clone(),
        alerts: harness.alerts.clone(),
        config: cross,
        echo_trust_anchor: None,
    });
    assert!(
        matches!(&refused, Err(PersonaError::Mix(reason)) if reason.contains("chromium")),
        "{refused:?}"
    );
}

/// Waits for something the running manager does in its own time.
async fn eventually<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    for _ in 0..200 {
        if let Some(found) = check().await {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never happened: {what}");
}

#[tokio::test]
async fn the_manager_follows_the_pool_until_it_is_told_to_stop() {
    let harness = Harness::with(149, 151, |config| {
        config.personas.sweep_interval = kage_core::duration::HumanDuration::from_secs(3600);
    })
    .await;
    // A persona from before this start: the directory is filled from the
    // store before anything is probed.
    let (before_proxy, before) = harness.persona("AT").await;
    let fresh = PersonaManager::new(PersonaParts {
        store: harness.store().clone(),
        pool: harness.pool.clone(),
        sidecars: Sidecars::from_config(&harness.config).unwrap(),
        http: harness.http.clone(),
        alerts: harness.alerts.clone(),
        config: harness.config.clone(),
        echo_trust_anchor: None,
    })
    .unwrap();
    let manager = std::sync::Arc::new(fresh);
    assert!(manager.directory().is_empty());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(manager.clone().run(async move {
        let _ = stopped.await;
    }));

    eventually("the stored persona is in the directory", || async {
        manager
            .directory()
            .spec(before_proxy.id)
            .filter(|spec| spec.persona == Some(before.id))
    })
    .await;

    // A proxy becomes healthy while the manager runs.
    let proxy = harness.healthy_proxy("DE").await;
    let persona = eventually("a persona for the new proxy", || async {
        harness
            .store()
            .personas()
            .get_by_proxy(proxy.id)
            .await
            .unwrap()
    })
    .await;
    assert!(manager.directory().spec(proxy.id).is_some());

    // The provider swaps it.
    harness.provider.remove(&proxy.provider_ref);
    harness.provider.add("DE");
    harness.pool.reconcile().await.unwrap();
    eventually("the persona is retired with its proxy", || async {
        let stored = harness.reload(&persona).await;
        (stored.state == PersonaState::Retired).then_some(())
    })
    .await;
    eventually("no probe presents as the retired persona", || async {
        manager.directory().spec(proxy.id).is_none().then_some(())
    })
    .await;

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the manager stops when told to")
        .unwrap();
}
