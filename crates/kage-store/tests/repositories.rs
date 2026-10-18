//! Every repository against a real Postgres: what is written reads back
//! equal, unique keys conflict, listings filter and page.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use chrono::{Duration, NaiveDate};
use common::{TestDb, now};
use kage_core::alert::{Alert, AlertKind, AlertSeverity};
use kage_core::domain::{
    ArtifactKind, ArtifactRef, Attempt, AuditActor, AuditEvent, AuditOutcome, BehaviorDone,
    BreakerState, ClaimedOs, ConsoleEntry, CookieSameSite, CoolingReason, DomainBreaker, Engine,
    FetchRecord, FrontierEntry, FrontierSource, FrontierState, GeoRecord, Header, HealthEvent,
    HealthFailure, HttpCookie, Job, JobItem, JobItemState, JobKind, JobOrigin, JobState,
    PacingApplied, Persona, PersonaDomainState, PersonaState, PersonaSummary, ProxyHealth,
    ProxyProviderKind, ProxyReplacement, ProxyState, Render, ReplaceReason, RobotsVerdict, Scope,
    SelftestRun, Session, SessionId, SessionLimits, SubjectKind, Timing, Visit,
};
use kage_core::ids::{
    ApiKeyId, ArtifactId, AttemptId, AuditEventId, FrontierId, HealthEventId, JobId, JobItemId,
    PersonaId, ProxyId, RequestId, ResultId, SelftestRunId,
};
use kage_core::outcome::{Block, BlockType, Outcome, Vendor};
use kage_core::secret::SealedSecret;
use kage_core::seed::Seed;
use kage_store::StoreError;
use kage_store::repo::{BrowserUpgrade, ResponseCacheEntry, RobotsCacheEntry, Upgraded};

// One proxy followed through its whole life: each step needs the one before.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn proxies_round_trip_filter_and_update() {
    let db = TestDb::migrated().await;
    let proxies = db.store.proxies();
    let first = common::proxy("ref-1");
    let mut second = common::proxy("ref-2");
    second.country = None;
    second.city = None;
    second.asn = None;

    proxies.insert(&first).await.unwrap();
    proxies.insert(&second).await.unwrap();

    assert_eq!(proxies.get(first.id).await.unwrap(), Some(first.clone()));
    assert_eq!(proxies.get(second.id).await.unwrap(), Some(second.clone()));
    assert_eq!(proxies.get(ProxyId::new()).await.unwrap(), None);
    assert_eq!(
        proxies.list(None).await.unwrap(),
        [first.clone(), second.clone()]
    );
    assert_eq!(proxies.list(Some(ProxyState::Healthy)).await.unwrap(), []);

    // The provider's reference is unique among the provider's live proxies.
    let mut duplicate = common::proxy("ref-1");
    duplicate.host = "198.51.100.99".to_owned();
    let conflict = proxies.insert(&duplicate).await.unwrap_err();
    assert!(conflict.is_conflict(), "{conflict}");
    assert!(
        conflict
            .to_string()
            .contains("proxies_provider_ref_live_idx"),
        "{conflict}"
    );

    // A health check moves the proxy through the pool states.
    let mut updated = first.clone();
    updated.state = ProxyState::Backoff;
    updated.consecutive_failures = 3;
    updated.backoff_count = 2;
    updated.backoff_until = Some(now() + Duration::minutes(4));
    updated.failing_since = Some(now() - Duration::minutes(20));
    updated.username = SealedSecret::new(vec![9, 8]);
    updated.password = SealedSecret::new(vec![1, 2, 3]);
    updated.asn = Some("AS3320".to_owned());
    updated.last_health = Some(ProxyHealth {
        failure: Some(HealthFailure::Unreachable),
        exit_ip: Some("203.0.113.5".parse().unwrap()),
        exit_country: Some("DE".parse().unwrap()),
        country_match: Some(true),
        leak_detected: Some(false),
        geo: Some(GeoRecord {
            country: "DE".parse().unwrap(),
            city: Some("Berlin".to_owned()),
            region: Some("Berlin".to_owned()),
            asn: Some("AS3320".to_owned()),
            org: Some("Deutsche Telekom AG".to_owned()),
            timezone: Some("Europe/Berlin".to_owned()),
            loc: Some("52.52,13.405".to_owned()),
            ip: "203.0.113.5".parse().unwrap(),
            checked_at: now(),
        }),
        reputation_score: Some(12),
        usage_type: Some("Fixed Line ISP".to_owned()),
        reputation_checked_at: Some(now()),
        ..ProxyHealth::new(now(), false)
    });
    assert!(proxies.update(&updated).await.unwrap());
    assert_eq!(proxies.get(first.id).await.unwrap(), Some(updated.clone()));
    assert_eq!(
        proxies.list(Some(ProxyState::Backoff)).await.unwrap(),
        [updated.clone()]
    );
    assert_eq!(
        proxies.list(Some(ProxyState::New)).await.unwrap(),
        [second.clone()]
    );

    // A requested replacement is stored with its reason and its ticket.
    let mut replacing = updated.clone();
    replacing.state = ProxyState::Replacing;
    replacing.backoff_until = None;
    replacing.replacement = Some(ProxyReplacement {
        requested_at: now(),
        reason: ReplaceReason::Unreachable,
        ticket: None,
    });
    assert!(proxies.update(&replacing).await.unwrap());
    assert_eq!(
        proxies.get(first.id).await.unwrap(),
        Some(replacing.clone())
    );
    replacing.replacement.as_mut().unwrap().ticket = Some("98315".to_owned());
    assert!(proxies.update(&replacing).await.unwrap());
    assert_eq!(
        proxies.get(first.id).await.unwrap(),
        Some(replacing.clone())
    );

    // Retired proxies leave the live list and free their reference.
    let mut retired = replacing.clone();
    retired.state = ProxyState::Retired;
    retired.retired_at = Some(now());
    assert!(proxies.update(&retired).await.unwrap());
    assert_eq!(
        proxies.list_live(None).await.unwrap(),
        std::slice::from_ref(&second)
    );
    assert_eq!(
        proxies
            .list_live(Some(ProxyProviderKind::Webshare))
            .await
            .unwrap(),
        std::slice::from_ref(&second)
    );
    assert_eq!(
        proxies
            .list_live(Some(ProxyProviderKind::Static))
            .await
            .unwrap(),
        []
    );
    let successor = common::proxy("ref-1");
    proxies.insert(&successor).await.unwrap();
    assert_eq!(proxies.list(None).await.unwrap().len(), 3);
    assert_eq!(proxies.list_live(None).await.unwrap().len(), 2);

    // Updating a proxy that does not exist reports it.
    assert!(!proxies.update(&common::proxy("ghost")).await.unwrap());
}

#[tokio::test]
async fn a_proxy_is_modified_under_its_row_lock() {
    let db = TestDb::migrated().await;
    let proxies = db.store.proxies();
    let proxy = common::proxy("ref-1");
    proxies.insert(&proxy).await.unwrap();

    // The closure sees what is stored, and what it returns comes back.
    let before = proxies
        .modify(proxy.id, |stored| {
            let before = stored.state;
            stored.state = ProxyState::Healthy;
            stored.consecutive_failures = 0;
            before
        })
        .await
        .unwrap();
    assert_eq!(before, Some(ProxyState::New));
    assert_eq!(
        proxies.get(proxy.id).await.unwrap().unwrap().state,
        ProxyState::Healthy
    );

    // A proxy that does not exist: the closure is not called.
    let missing = proxies
        .modify(ProxyId::new(), |_| panic!("called for a missing proxy"))
        .await
        .unwrap();
    assert_eq!(missing, None::<()>);

    // Fifty increments from concurrent tasks all land: none reads a value
    // that another is about to overwrite.
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..50 {
        let store = db.store.clone();
        let id = proxy.id;
        tasks.spawn(async move {
            store
                .proxies()
                .modify(id, |stored| stored.consecutive_failures += 1)
                .await
                .unwrap();
        });
    }
    while let Some(done) = tasks.join_next().await {
        done.unwrap();
    }
    assert_eq!(
        proxies
            .get(proxy.id)
            .await
            .unwrap()
            .unwrap()
            .consecutive_failures,
        50
    );

    // A value that cannot be stored leaves the row as it was.
    let error = proxies
        .modify(proxy.id, |stored| {
            stored.state = ProxyState::Backoff;
            stored.consecutive_failures = u32::MAX;
        })
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::OutOfRange { .. }));
    let stored = proxies.get(proxy.id).await.unwrap().unwrap();
    assert_eq!(stored.state, ProxyState::Healthy);
    assert_eq!(stored.consecutive_failures, 50);
}

#[tokio::test]
async fn the_replacement_budget_is_counted_and_spent_atomically() {
    let db = TestDb::migrated().await;
    let proxies = db.store.proxies();
    let month_start = now() - Duration::days(3);
    let budget = 5;
    let mut ids = Vec::new();
    for index in 0..12 {
        let proxy = common::proxy(&format!("ref-{index}"));
        proxies.insert(&proxy).await.unwrap();
        ids.push(proxy.id);
    }
    // One replacement from last month does not count against this one.
    proxies
        .modify(ids[11], |stored| {
            stored.state = ProxyState::Retired;
            stored.retired_at = Some(month_start - Duration::days(1));
            stored.replacement = Some(ProxyReplacement {
                requested_at: month_start - Duration::days(2),
                reason: ReplaceReason::Operator,
                ticket: Some("1".to_owned()),
            });
        })
        .await
        .unwrap();
    assert_eq!(proxies.replacements_since(month_start).await.unwrap(), 0);
    assert_eq!(
        proxies
            .replacements_since(month_start - Duration::days(5))
            .await
            .unwrap(),
        1
    );

    // Eleven tasks race for five slots.
    let mut tasks = tokio::task::JoinSet::new();
    for id in ids.iter().copied().take(11) {
        let store = db.store.clone();
        tasks.spawn(async move {
            store
                .proxies()
                .modify_counting_replacements(id, month_start, |stored, used| {
                    if used >= budget {
                        return false;
                    }
                    stored.state = ProxyState::Replacing;
                    stored.replacement = Some(ProxyReplacement {
                        requested_at: now(),
                        reason: ReplaceReason::Unreachable,
                        ticket: None,
                    });
                    true
                })
                .await
                .unwrap()
                .unwrap()
        });
    }
    let mut granted = 0;
    while let Some(done) = tasks.join_next().await {
        if done.unwrap() {
            granted += 1;
        }
    }

    assert_eq!(granted, budget);
    assert_eq!(
        proxies.replacements_since(month_start).await.unwrap(),
        budget
    );
    assert_eq!(
        proxies
            .list(Some(ProxyState::Replacing))
            .await
            .unwrap()
            .len(),
        5
    );

    // A request the provider failed is cleared and frees its slot; a
    // completed one (a retired row) keeps counting.
    let replacing = proxies.list(Some(ProxyState::Replacing)).await.unwrap();
    proxies
        .modify(replacing[0].id, |stored| {
            stored.state = ProxyState::Backoff;
            stored.replacement = None;
        })
        .await
        .unwrap();
    proxies
        .modify(replacing[1].id, |stored| {
            stored.state = ProxyState::Retired;
            stored.retired_at = Some(now());
        })
        .await
        .unwrap();
    assert_eq!(proxies.replacements_since(month_start).await.unwrap(), 4);

    // A missing proxy: nothing is counted for it and nothing is called.
    let missing = proxies
        .modify_counting_replacements(ProxyId::new(), month_start, |_, _| {
            panic!("called for a missing proxy")
        })
        .await
        .unwrap();
    assert_eq!(missing, None::<()>);
}

#[tokio::test]
async fn a_recorded_leak_sets_the_egress_flag_until_it_is_cleared() {
    let db = TestDb::migrated().await;
    let proxies = db.store.proxies();
    let proxy = common::proxy("ref-1");
    let other = common::proxy("ref-2");
    proxies.insert(&proxy).await.unwrap();
    proxies.insert(&other).await.unwrap();
    assert!(!proxies.any_leak().await.unwrap());

    let leak = |flag: Option<bool>| ProxyHealth {
        leak_detected: flag,
        ..ProxyHealth::new(now(), true)
    };
    proxies
        .modify(other.id, |stored| {
            stored.last_health = Some(leak(Some(false)));
        })
        .await
        .unwrap();
    assert!(!proxies.any_leak().await.unwrap());

    proxies
        .modify(proxy.id, |stored| {
            stored.last_health = Some(leak(Some(true)));
        })
        .await
        .unwrap();
    assert!(proxies.any_leak().await.unwrap());

    // Retiring the proxy does not clear the flag: only a clean probe or an
    // operator does.
    proxies
        .modify(proxy.id, |stored| {
            stored.state = ProxyState::Retired;
            stored.retired_at = Some(now());
        })
        .await
        .unwrap();
    assert!(proxies.any_leak().await.unwrap());

    proxies
        .modify(proxy.id, |stored| stored.last_health = Some(leak(None)))
        .await
        .unwrap();
    assert!(!proxies.any_leak().await.unwrap());
}

#[tokio::test]
async fn the_schema_keeps_replacement_columns_consistent() {
    let db = TestDb::migrated().await;
    let proxy = common::proxy("ref-1");
    db.store.proxies().insert(&proxy).await.unwrap();

    for statement in [
        // A reason without a time, a time without a reason, a ticket alone.
        "UPDATE proxies SET replacement_reason = 'operator'",
        "UPDATE proxies SET replacement_requested_at = now()",
        "UPDATE proxies SET replacement_ticket = '7'",
        // A reason outside the domain model, a negative counter.
        "UPDATE proxies SET replacement_requested_at = now(), replacement_reason = 'bored'",
        "UPDATE proxies SET backoff_count = -1",
    ] {
        let error = sqlx::query(statement)
            .execute(db.store.pool())
            .await
            .unwrap_err();
        assert!(
            error
                .as_database_error()
                .is_some_and(sqlx::error::DatabaseError::is_check_violation),
            "{statement}: {error}"
        );
    }
    assert_eq!(db.store.proxies().get(proxy.id).await.unwrap(), Some(proxy));
}

#[tokio::test]
async fn a_stored_value_that_does_not_parse_is_reported_as_corrupt() {
    let db = TestDb::migrated().await;
    let proxy = common::proxy("ref-1");
    db.store.proxies().insert(&proxy).await.unwrap();

    // The schema checks enum names, but not the shape of a JSON document.
    sqlx::query("UPDATE proxies SET last_health = '{\"reachable\": \"maybe\"}'")
        .execute(db.store.pool())
        .await
        .unwrap();

    let error = db.store.proxies().get(proxy.id).await.unwrap_err();
    assert!(matches!(
        error,
        StoreError::Corrupt {
            table: "proxies",
            column: "last_health",
            ..
        }
    ));
    assert!(db.store.proxies().list(None).await.is_err());
}

// One persona followed through its whole life: each step needs the one
// before.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn personas_round_trip_and_follow_their_lifecycle() {
    let db = TestDb::migrated().await;
    let personas = db.store.personas();
    let proxy = common::proxy("ref-1");
    db.store.proxies().insert(&proxy).await.unwrap();
    let persona = common::persona(proxy.id);

    personas.insert(&persona).await.unwrap();

    assert_eq!(
        personas.get(persona.id).await.unwrap(),
        Some(persona.clone())
    );
    assert_eq!(
        personas.get_by_proxy(proxy.id).await.unwrap(),
        Some(persona.clone())
    );
    assert_eq!(personas.get(PersonaId::new()).await.unwrap(), None);
    assert_eq!(personas.get_by_proxy(ProxyId::new()).await.unwrap(), None);
    assert_eq!(
        personas.list(None).await.unwrap(),
        std::slice::from_ref(&persona)
    );
    assert_eq!(
        personas.list(Some(PersonaState::Active)).await.unwrap(),
        std::slice::from_ref(&persona)
    );
    assert_eq!(
        personas.list(Some(PersonaState::Retired)).await.unwrap(),
        []
    );
    assert_eq!(
        personas.list_live().await.unwrap(),
        std::slice::from_ref(&persona)
    );

    // A proxy has exactly one persona that is not retired.
    let conflict = personas
        .insert(&common::persona(proxy.id))
        .await
        .unwrap_err();
    assert!(conflict.is_conflict());
    assert!(conflict.to_string().contains("personas_proxy_live_idx"));

    // A persona without a position, a country or a wreq profile is
    // browser-only.
    let other_proxy = common::proxy("ref-2");
    db.store.proxies().insert(&other_proxy).await.unwrap();
    let mut browser_only = common::persona(other_proxy.id);
    browser_only.engine = Engine::Firefox;
    browser_only.claimed_os = ClaimedOs::Windows;
    browser_only.wreq_profile = None;
    browser_only.geolocation = None;
    browser_only.country = None;
    browser_only.screen.color_depth = 30;
    personas.insert(&browser_only).await.unwrap();
    assert_eq!(
        personas.get(browser_only.id).await.unwrap(),
        Some(browser_only.clone())
    );

    // What follows the address, the warm-up and an engine upgrade can
    // change; the change is returned and read back whole.
    let warmed = now() + Duration::hours(2);
    let changed = personas
        .modify(persona.id, |stored| {
            stored.country = Some("AT".parse().unwrap());
            stored.locale = "de-AT".to_owned();
            stored.languages = vec!["de-DE".to_owned(), "de".to_owned()];
            stored.timezone = "Europe/Vienna".to_owned();
            stored.geolocation = None;
            stored.browser_major = 150;
            stored.wreq_profile = None;
            stored.user_agent = stored.user_agent.replace("149", "150");
            stored.warmup_visits = 3;
            stored.warmed_at = Some(warmed);
            stored.clone()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        personas.get(persona.id).await.unwrap(),
        Some(changed.clone())
    );
    assert_eq!(changed.timezone, "Europe/Vienna");
    assert_eq!(changed.warmed_at, Some(warmed));
    assert!(changed.user_agent.contains("Chrome/150"));
    // What makes the persona is written once: a change to it is not kept.
    personas
        .modify(persona.id, |stored| {
            stored.engine = Engine::Firefox;
            stored.claimed_os = ClaimedOs::Macos;
            stored.fingerprint = serde_json::json!({});
            stored.screen.width = 800;
            stored.profile_dir = "elsewhere".to_owned();
            stored.tables_version = "v9".to_owned();
            stored.hardware_concurrency = 64;
        })
        .await
        .unwrap();
    assert_eq!(personas.get(persona.id).await.unwrap(), Some(changed));
    // A persona that does not exist is not changed, and the closure does
    // not run.
    assert_eq!(
        personas
            .modify(PersonaId::new(), |_| panic!("called for a missing persona"))
            .await
            .unwrap(),
        None::<()>
    );

    // Cooling needs its reason, and only cooling has one.
    personas
        .modify(persona.id, |stored| {
            stored.state = PersonaState::Cooling;
            stored.cooling_reason = Some(CoolingReason::SelftestFailed);
        })
        .await
        .unwrap();
    let cooling = personas.get(persona.id).await.unwrap().unwrap();
    assert_eq!(cooling.cooling_reason, Some(CoolingReason::SelftestFailed));
    assert_eq!(
        personas.list(Some(PersonaState::Cooling)).await.unwrap(),
        [cooling]
    );
    // Cooling without a reason, active with one, retired without a time.
    let broken: [fn(&mut Persona); 3] = [
        |stored| stored.cooling_reason = None,
        |stored| stored.state = PersonaState::Active,
        |stored| {
            stored.state = PersonaState::Retired;
            stored.cooling_reason = None;
        },
    ];
    for change in broken {
        assert!(personas.modify(persona.id, change).await.is_err());
    }

    // Retired with a time and a reason. The proxy may then get another
    // persona, and the retired one stays as history.
    let retired_at = now() + Duration::hours(3);
    personas
        .modify(persona.id, |stored| {
            stored.state = PersonaState::Retired;
            stored.cooling_reason = None;
            stored.retired_at = Some(retired_at);
            stored.retire_reason = Some("operator".to_owned());
        })
        .await
        .unwrap();
    let retired = personas.get(persona.id).await.unwrap().unwrap();
    assert_eq!(retired.state, PersonaState::Retired);
    assert_eq!(retired.retired_at, Some(retired_at));
    assert_eq!(retired.retire_reason.as_deref(), Some("operator"));
    assert_eq!(personas.get_by_proxy(proxy.id).await.unwrap(), None);
    let successor = common::persona(proxy.id);
    personas.insert(&successor).await.unwrap();
    assert_eq!(
        personas.get_by_proxy(proxy.id).await.unwrap(),
        Some(successor.clone())
    );
    assert_eq!(
        personas.list(Some(PersonaState::Retired)).await.unwrap(),
        std::slice::from_ref(&retired)
    );
    let live: Vec<PersonaId> = personas
        .list_live()
        .await
        .unwrap()
        .into_iter()
        .map(|persona| persona.id)
        .collect();
    assert_eq!(live.len(), 2);
    assert!(live.contains(&browser_only.id) && live.contains(&successor.id));

    // The profile of a retired persona is archived once it has been
    // retired for long enough, and the archive deleted after its
    // retention.
    assert_eq!(
        personas
            .list_archive_due(retired_at - Duration::seconds(1))
            .await
            .unwrap(),
        []
    );
    assert_eq!(
        personas.list_archive_due(retired_at).await.unwrap(),
        std::slice::from_ref(&retired)
    );
    assert_eq!(personas.list_delete_due(retired_at).await.unwrap(), []);
    let archived_at = retired_at + Duration::minutes(10);
    personas
        .modify(persona.id, |stored| {
            stored.profile_archived_at = Some(archived_at);
            stored.profile_archive = Some(format!(".archive/{}", stored.id));
        })
        .await
        .unwrap();
    assert_eq!(
        personas
            .list_archive_due(archived_at + Duration::days(1))
            .await
            .unwrap(),
        []
    );
    assert_eq!(
        personas
            .list_delete_due(archived_at - Duration::seconds(1))
            .await
            .unwrap(),
        []
    );
    let due = personas.list_delete_due(archived_at).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(
        due[0].profile_archive,
        Some(format!(".archive/{}", persona.id))
    );
    personas
        .modify(persona.id, |stored| {
            stored.profile_deleted_at = Some(archived_at + Duration::days(7));
        })
        .await
        .unwrap();
    assert_eq!(
        personas
            .list_delete_due(archived_at + Duration::days(30))
            .await
            .unwrap(),
        []
    );
    // An archive needs a retired persona and a date.
    assert!(
        personas
            .modify(successor.id, |stored| {
                stored.profile_archived_at = Some(archived_at);
            })
            .await
            .is_err()
    );
    assert!(
        personas
            .modify(successor.id, |stored| {
                stored.profile_archive = Some("x".to_owned());
            })
            .await
            .is_err()
    );

    // The schema refuses a seed that is none.
    let error = sqlx::query("UPDATE personas SET seed = 'not-a-seed' WHERE id = $1")
        .bind(successor.id.as_uuid())
        .execute(db.store.pool())
        .await
        .unwrap_err();
    assert!(
        error
            .as_database_error()
            .is_some_and(sqlx::error::DatabaseError::is_check_violation),
        "{error}"
    );
}

#[tokio::test]
async fn persona_domain_state_is_upserted_per_persona_and_domain() {
    let db = TestDb::migrated().await;
    let states = db.store.persona_domain_state();
    let persona = common::seed_persona(&db.store).await;
    let other = common::seed_persona(&db.store).await;

    assert_eq!(states.get(persona.id, "example.org").await.unwrap(), None);

    let mut state = PersonaDomainState::new(persona.id, "example.org");
    state.requests_today = 12;
    state.counter_day = NaiveDate::from_ymd_opt(2026, 10, 8);
    state.last_request_at = Some(now());
    state.last_outcome = Some(Outcome::Ok);
    states.upsert(&state).await.unwrap();
    assert_eq!(
        states.get(persona.id, "example.org").await.unwrap(),
        Some(state.clone())
    );

    // A block: the same row is replaced, not duplicated.
    state.cooldown_until = Some(now() + Duration::hours(24));
    state.requests_today = 13;
    state.last_outcome = Some(Outcome::Forbidden);
    state.block_count_24h = 1;
    state.block_window_started_at = Some(now());
    states.upsert(&state).await.unwrap();
    assert_eq!(
        states.get(persona.id, "example.org").await.unwrap(),
        Some(state.clone())
    );

    // Other domains and other personas are separate rows.
    let elsewhere = PersonaDomainState::new(persona.id, "example.net");
    states.upsert(&elsewhere).await.unwrap();
    let other_state = PersonaDomainState::new(other.id, "example.org");
    states.upsert(&other_state).await.unwrap();

    let mut for_domain = states.list_for_domain("example.org").await.unwrap();
    for_domain.sort_by_key(|state| state.persona_id);
    let mut expected = vec![state.clone(), other_state];
    expected.sort_by_key(|state| state.persona_id);
    assert_eq!(for_domain, expected);
    assert_eq!(
        states.list_for_domain("example.net").await.unwrap(),
        std::slice::from_ref(&elsewhere)
    );
    assert_eq!(states.list_for_domain("unknown.test").await.unwrap(), []);
    // One persona's domains, by name.
    assert_eq!(
        states.list_for_persona(persona.id).await.unwrap(),
        [elsewhere, state]
    );
    assert_eq!(states.list_for_persona(PersonaId::new()).await.unwrap(), []);
}

#[tokio::test]
async fn domain_state_changes_are_counted_one_after_the_other() {
    let db = TestDb::migrated().await;
    let persona = common::seed_persona(&db.store).await;
    let day = NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
    let at = now();

    // A first visit creates the row, and what the change returns comes
    // back.
    let counted = db
        .store
        .persona_domain_state()
        .modify(persona.id, "example.org", |state| {
            state.record_request(at, day);
            state.requests_today
        })
        .await
        .unwrap();
    assert_eq!(counted, 1);

    // Forty requests counted at once from separate tasks are forty.
    let mut tasks = Vec::new();
    for _ in 0..40 {
        let store = db.store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .persona_domain_state()
                .modify(persona.id, "example.org", |state| {
                    state.record_request(at, day);
                })
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let state = db
        .store
        .persona_domain_state()
        .get(persona.id, "example.org")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.requests_on(day), 41);
    assert_eq!(state.last_request_at, Some(at));

    // A block is recorded through the same door.
    db.store
        .persona_domain_state()
        .modify(persona.id, "example.org", |state| {
            state.record_outcome(Outcome::RateLimited, at, Some(Duration::hours(2)));
        })
        .await
        .unwrap();
    let blocked = db
        .store
        .persona_domain_state()
        .get(persona.id, "example.org")
        .await
        .unwrap()
        .unwrap();
    assert!(blocked.is_cooling(at + Duration::minutes(119)));
    assert_eq!(blocked.blocks_in_window(at), 1);
    assert_eq!(blocked.block_window_started_at, Some(at));

    // A state needs its persona.
    assert!(
        db.store
            .persona_domain_state()
            .modify(PersonaId::new(), "example.org", |_| ())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn api_keys_round_trip_disable_rotate_and_touch() {
    let db = TestDb::migrated().await;
    let keys = db.store.api_keys();
    let key = common::api_key("project-a", &[Scope::Fetch, Scope::Sessions, Scope::Admin]);
    let mut unlimited = common::api_key("project-b", &[Scope::Crawl]);
    unlimited.limits.daily_page_budget = None;

    keys.insert(&key).await.unwrap();
    keys.insert(&unlimited).await.unwrap();

    assert_eq!(keys.get(key.id).await.unwrap(), Some(key.clone()));
    assert_eq!(keys.get(ApiKeyId::new()).await.unwrap(), None);
    assert_eq!(keys.list().await.unwrap(), [key.clone(), unlimited]);

    let disabled_at = now();
    assert!(keys.set_disabled(key.id, Some(disabled_at)).await.unwrap());
    let disabled = keys.get(key.id).await.unwrap().unwrap();
    assert_eq!(disabled.disabled_at, Some(disabled_at));
    assert!(!disabled.allows(Scope::Fetch));
    assert!(keys.set_disabled(key.id, None).await.unwrap());
    assert!(
        keys.get(key.id)
            .await
            .unwrap()
            .unwrap()
            .allows(Scope::Fetch)
    );

    assert!(keys.set_secret_hash(key.id, &[9; 32]).await.unwrap());
    assert_eq!(
        keys.get(key.id).await.unwrap().unwrap().secret_hash,
        [9; 32]
    );

    let used_at = now();
    keys.touch(key.id, used_at).await.unwrap();
    assert_eq!(
        keys.get(key.id).await.unwrap().unwrap().last_used_at,
        Some(used_at)
    );

    assert!(!keys.set_disabled(ApiKeyId::new(), None).await.unwrap());
    assert!(
        !keys
            .set_secret_hash(ApiKeyId::new(), &[0; 32])
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn sessions_are_unique_per_key_and_expire() {
    let db = TestDb::migrated().await;
    let sessions = db.store.sessions();
    let persona = common::seed_persona(&db.store).await;
    let key = common::seed_key(&db.store).await;
    let other_key = common::api_key("other", &[Scope::Sessions]);
    db.store.api_keys().insert(&other_key).await.unwrap();
    let limits = SessionLimits::default();
    let start = now();
    let id: SessionId = "proj-a-42".parse().unwrap();

    let mut session = Session::open(id.clone(), key.id, persona.id, start, limits);
    sessions.insert(&session).await.unwrap();
    assert_eq!(
        sessions.get(key.id, &id).await.unwrap(),
        Some(session.clone())
    );
    assert_eq!(sessions.get(other_key.id, &id).await.unwrap(), None);

    // The same id under the same key conflicts; under another key it is fine.
    let conflict = sessions
        .insert(&Session::open(
            id.clone(),
            key.id,
            persona.id,
            start,
            limits,
        ))
        .await
        .unwrap_err();
    assert!(conflict.is_conflict());
    let other_session = Session::open(id.clone(), other_key.id, persona.id, start, limits);
    sessions.insert(&other_session).await.unwrap();

    // Use extends the session.
    session.touch(start + Duration::minutes(5), limits);
    assert!(sessions.touch(&session).await.unwrap());
    assert_eq!(
        sessions.get(key.id, &id).await.unwrap(),
        Some(session.clone())
    );

    // The cleanup job removes only what has expired.
    assert_eq!(
        sessions
            .delete_expired(start + Duration::minutes(11))
            .await
            .unwrap(),
        1
    );
    assert_eq!(sessions.get(other_key.id, &id).await.unwrap(), None);
    assert_eq!(
        sessions.get(key.id, &id).await.unwrap(),
        Some(session.clone())
    );

    assert!(sessions.delete(key.id, &id).await.unwrap());
    assert!(!sessions.delete(key.id, &id).await.unwrap());
    assert!(!sessions.touch(&session).await.unwrap());

    // A persona's sessions are counted while they run and all end with it.
    let other_persona = common::seed_persona(&db.store).await;
    for (name, pinned) in [
        ("a", persona.id),
        ("b", persona.id),
        ("c", other_persona.id),
    ] {
        sessions
            .insert(&Session::open(
                name.parse().unwrap(),
                key.id,
                pinned,
                start,
                limits,
            ))
            .await
            .unwrap();
    }
    assert_eq!(
        sessions.count_for_persona(persona.id, start).await.unwrap(),
        2
    );
    // Expired ones are not load.
    assert_eq!(
        sessions
            .count_for_persona(persona.id, start + Duration::minutes(10))
            .await
            .unwrap(),
        0
    );
    assert_eq!(sessions.delete_for_persona(persona.id).await.unwrap(), 2);
    assert_eq!(sessions.delete_for_persona(persona.id).await.unwrap(), 0);
    assert_eq!(
        sessions
            .count_for_persona(other_persona.id, start)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn audit_events_page_newest_first_and_outlive_their_key() {
    let db = TestDb::migrated().await;
    let audit = db.store.audit_events();
    let key = common::seed_key(&db.store).await;
    let event = |actor: AuditActor, action: &str, outcome: AuditOutcome| AuditEvent {
        id: AuditEventId::new(),
        at: now(),
        actor,
        action: action.to_owned(),
        target: Some("persona:1".to_owned()),
        outcome,
        details: serde_json::json!({ "reason": "test" }),
    };

    let by_key = event(
        AuditActor::ApiKey(key.id),
        "persona.retire",
        AuditOutcome::Success,
    );
    let by_cli = event(AuditActor::Cli, "key.create", AuditOutcome::Failure);
    let mut by_system = event(AuditActor::System, "proxy.replace", AuditOutcome::Denied);
    by_system.target = None;
    for entry in [&by_key, &by_cli, &by_system] {
        audit.insert(entry).await.unwrap();
        // Ids are time-ordered at millisecond resolution.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    let all = audit.list(None, 10).await.unwrap();
    assert_eq!(all, [by_system.clone(), by_cli.clone(), by_key.clone()]);
    let first_page = audit.list(None, 2).await.unwrap();
    assert_eq!(first_page, [by_system, by_cli.clone()]);
    let second_page = audit.list(Some(by_cli.id), 2).await.unwrap();
    assert_eq!(second_page, std::slice::from_ref(&by_key));
    assert_eq!(audit.list(Some(by_key.id), 2).await.unwrap(), []);

    // Deleting the key keeps the event; it is no longer attributed to a key.
    sqlx::query("DELETE FROM api_keys WHERE id = $1")
        .bind(key.id.as_uuid())
        .execute(db.store.pool())
        .await
        .unwrap();
    let orphaned = audit.list(Some(by_cli.id), 2).await.unwrap();
    assert_eq!(orphaned.len(), 1);
    assert_eq!(orphaned[0].id, by_key.id);
    assert_eq!(orphaned[0].actor, AuditActor::System);
    assert_eq!(orphaned[0].action, "persona.retire");
}

fn block() -> Block {
    Block {
        vendor: Vendor::new("cloudflare").unwrap(),
        block_type: BlockType::new("managed_challenge").unwrap(),
        confidence: 0.5,
        evidence: vec!["header:cf-mitigated=challenge".to_owned()],
    }
}

#[tokio::test]
async fn a_keys_requests_are_counted_since_a_moment() {
    let db = TestDb::migrated().await;
    let store = &db.store;
    let key = common::seed_key(store).await;
    let other = common::seed_key(store).await;
    let at = now();

    // Three requests of the key over two days, one of another key, and
    // one of Kage's own.
    for (owner, age) in [
        (Some(key.id), Duration::hours(30)),
        (Some(key.id), Duration::hours(2)),
        (Some(key.id), Duration::zero()),
        (Some(other.id), Duration::hours(1)),
        (None, Duration::hours(1)),
    ] {
        let mut request = common::request(owner);
        request.created_at = at - age;
        store.requests().insert(&request).await.unwrap();
    }

    let requests = store.requests();
    let since = |hours: i64| at - Duration::hours(hours);
    assert_eq!(
        requests
            .count_for_key_since(key.id, since(48))
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        requests
            .count_for_key_since(key.id, since(24))
            .await
            .unwrap(),
        2
    );
    // The moment itself is inside; the one after it is not.
    assert_eq!(requests.count_for_key_since(key.id, at).await.unwrap(), 1);
    assert_eq!(
        requests
            .count_for_key_since(key.id, at + Duration::seconds(1))
            .await
            .unwrap(),
        0
    );
    // A request that was served from the cache is not a page of the
    // budget; one that was fetched is, whatever came of it.
    for cached in [true, false] {
        let request = common::request(Some(key.id));
        store.requests().insert(&request).await.unwrap();
        let record = FetchRecord {
            id: ResultId::new(),
            request_id: request.id,
            outcome: Outcome::Ok,
            status: Some(200),
            headers: Vec::new(),
            final_url: None,
            redirects: Vec::new(),
            extract: None,
            xhr: None,
            artifacts: Vec::new(),
            persona: None,
            block: None,
            robots: None,
            cached,
            cached_at: cached.then_some(at),
            reason: None,
            visit: Visit::default(),
            reading: None,
            created_at: request.created_at,
        };
        store.results().insert(&record).await.unwrap();
    }
    assert_eq!(
        requests
            .count_for_key_since(key.id, since(48))
            .await
            .unwrap(),
        4
    );
    // Another key's requests, and Kage's own, are not this key's.
    assert_eq!(
        requests
            .count_for_key_since(other.id, since(48))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        requests
            .count_for_key_since(ApiKeyId::new(), since(48))
            .await
            .unwrap(),
        0
    );
}

// One request followed from acceptance through three attempts to its result.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn requests_attempts_and_results_round_trip() {
    let db = TestDb::migrated().await;
    let store = &db.store;
    let persona = common::seed_persona(store).await;
    let other_persona = common::seed_persona(store).await;
    let key = common::seed_key(store).await;

    let mut request = common::request(Some(key.id));
    request.session_id = Some("proj-a-42".parse().unwrap());
    store.requests().insert(&request).await.unwrap();
    assert_eq!(
        store.requests().get(request.id).await.unwrap(),
        Some(request.clone())
    );
    assert_eq!(store.requests().get(RequestId::new()).await.unwrap(), None);
    // Kage's own traffic has no key.
    let internal = common::request(None);
    store.requests().insert(&internal).await.unwrap();
    assert_eq!(
        store.requests().get(internal.id).await.unwrap(),
        Some(internal)
    );

    let started = now();
    let blocked = Attempt {
        id: AttemptId::new(),
        request_id: request.id,
        n: 1,
        persona_id: persona.id,
        engine: Engine::Chromium,
        render: Render::Http,
        started_at: started,
        finished_at: started + Duration::milliseconds(840),
        status: Some(403),
        final_url: Some("https://example.org/listing?page=2".to_owned()),
        outcome: Outcome::ChallengeFailed,
        block: Some(block()),
        timing: Timing {
            dns_ms: Some(12),
            connect_ms: Some(80),
            tls_ms: Some(95),
            ttfb_ms: Some(410),
            dom_content_loaded_ms: None,
            load_ms: None,
            total_ms: 840,
        },
        seed: None,
        reason: None,
        patience_ms: None,
    };
    let succeeded = Attempt {
        id: AttemptId::new(),
        n: 2,
        persona_id: other_persona.id,
        render: Render::Browser,
        started_at: started + Duration::seconds(5),
        finished_at: started + Duration::seconds(14),
        status: Some(200),
        outcome: Outcome::Ok,
        block: None,
        timing: Timing {
            total_ms: 9000,
            ..Timing::default()
        },
        // What the behavior of the attempt was drawn from, and the
        // patience it had with the page.
        seed: Some(Seed::from_u64(7)),
        patience_ms: Some(41_200),
        ..blocked.clone()
    };
    // Timed out before any response: no status, no final URL.
    let no_response = Attempt {
        id: AttemptId::new(),
        n: 3,
        status: None,
        final_url: None,
        outcome: Outcome::Timeout,
        block: None,
        reason: Some("navigation_failed".to_owned()),
        ..blocked.clone()
    };
    // Inserted out of order; listed in order.
    store.attempts().insert(&succeeded).await.unwrap();
    store.attempts().insert(&no_response).await.unwrap();
    store.attempts().insert(&blocked).await.unwrap();
    assert_eq!(
        store.attempts().list_for_request(request.id).await.unwrap(),
        [blocked.clone(), succeeded, no_response]
    );
    assert_eq!(
        store
            .attempts()
            .list_for_request(RequestId::new())
            .await
            .unwrap(),
        []
    );
    // The attempt number is unique within a request.
    let duplicate = Attempt {
        id: AttemptId::new(),
        ..blocked
    };
    assert!(
        store
            .attempts()
            .insert(&duplicate)
            .await
            .unwrap_err()
            .is_conflict()
    );

    let finished_at = now();
    assert!(
        store
            .requests()
            .finish(request.id, Outcome::Ok, finished_at)
            .await
            .unwrap()
    );
    let finished = store.requests().get(request.id).await.unwrap().unwrap();
    assert_eq!(finished.outcome, Some(Outcome::Ok));
    assert_eq!(finished.finished_at, Some(finished_at));
    assert!(
        !store
            .requests()
            .finish(RequestId::new(), Outcome::Ok, finished_at)
            .await
            .unwrap()
    );

    let record = FetchRecord {
        id: ResultId::new(),
        request_id: request.id,
        outcome: Outcome::Ok,
        status: Some(200),
        headers: vec![
            Header {
                name: "content-type".to_owned(),
                value: "text/html".to_owned(),
            },
            Header {
                name: "set-cookie".to_owned(),
                value: "a=1".to_owned(),
            },
            Header {
                name: "set-cookie".to_owned(),
                value: "b=2".to_owned(),
            },
        ],
        final_url: Some("https://example.org/listing?page=2".to_owned()),
        redirects: vec!["http://example.org/listing?page=2".to_owned()],
        extract: Some(serde_json::json!({ "title": "Listing", "thin_content": false })),
        xhr: Some(serde_json::json!([{ "url": "https://example.org/api/items", "status": 200 }])),
        artifacts: vec![ArtifactRef {
            kind: ArtifactKind::DomHtml,
            id: ArtifactId::new(),
        }],
        persona: Some(PersonaSummary {
            id: other_persona.id,
            engine: Engine::Chromium,
            claimed_os: ClaimedOs::Windows,
            country: Some("DE".parse().unwrap()),
        }),
        block: None,
        robots: Some(RobotsVerdict {
            fetched: true,
            allowed: true,
            matched_rule: None,
            crawl_delay: None,
        }),
        cached: false,
        cached_at: None,
        reason: None,
        // What is kept of the visit besides the page (follow-up F36).
        visit: Visit {
            pacing: Some(PacingApplied {
                applied: "human".to_owned(),
                request_ignored: true,
                waited_ms: 3900,
            }),
            behavior: Some(BehaviorDone {
                from_request: true,
                request_ignored: false,
                scroll_depth: Some(0.8),
                scroll_reached: Some(0.76),
                scroll_ended_by: Some("patience".to_owned()),
                patience_ms: Some(38_000),
                dwell_ms: Some(6100),
                referer_strategy: kage_core::policy::RefererStrategy::Direct,
                moves: Some(87),
                notches: Some(12),
            }),
            console: Some(vec![ConsoleEntry::bounded(
                "error",
                "network",
                "Failed to load resource",
                Some("https://example.org/missing.js"),
            )]),
            side_effects: vec!["resize".to_owned()],
        },
        // What a crawl took from the page, until it recorded it (F49).
        reading: Some(serde_json::json!({
            "final_url": "https://example.org/listing?page=2",
            "links": [{ "url": "https://example.org/listing?page=3", "hidden": false, "furniture": false }],
            "links_truncated": false,
            "text": "Listing, page two",
        })),
        created_at: now(),
    };
    store.results().insert(&record).await.unwrap();
    assert_eq!(
        store.results().get(record.id).await.unwrap(),
        Some(record.clone())
    );
    // The visit is one document of the row, with a member for each part.
    let visit: serde_json::Value = sqlx::query_scalar("SELECT visit FROM results WHERE id = $1")
        .bind(record.id.as_uuid())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(visit["pacing"]["waited_ms"], 3900);
    assert_eq!(visit["behavior"]["scroll_ended_by"], "patience");
    assert_eq!(visit["console"][0]["source"], "network");
    assert_eq!(visit["side_effects"], serde_json::json!(["resize"]));
    // Once the crawl has recorded the page, what it took is forgotten, and
    // nothing else of the result.
    assert!(store.results().forget_reading(request.id).await.unwrap());
    assert!(!store.results().forget_reading(request.id).await.unwrap());
    assert!(
        !store
            .results()
            .forget_reading(RequestId::new())
            .await
            .unwrap()
    );
    let forgotten = store.results().get(record.id).await.unwrap().unwrap();
    assert_eq!(
        forgotten,
        FetchRecord {
            reading: None,
            ..record.clone()
        }
    );
    let record = forgotten;
    assert_eq!(
        store.results().get_by_request(request.id).await.unwrap(),
        Some(record.clone())
    );
    assert_eq!(store.results().get(ResultId::new()).await.unwrap(), None);
    assert_eq!(
        store
            .results()
            .get_by_request(RequestId::new())
            .await
            .unwrap(),
        None
    );
    // One result per request.
    let second = FetchRecord {
        id: ResultId::new(),
        ..record
    };
    assert!(
        store
            .results()
            .insert(&second)
            .await
            .unwrap_err()
            .is_conflict()
    );

    // A blocked result with nothing optional set.
    let blocked_request = common::request(Some(key.id));
    store.requests().insert(&blocked_request).await.unwrap();
    let blocked_record = FetchRecord {
        id: ResultId::new(),
        request_id: blocked_request.id,
        outcome: Outcome::ChallengeFailed,
        status: None,
        headers: Vec::new(),
        final_url: None,
        redirects: Vec::new(),
        extract: None,
        xhr: None,
        artifacts: Vec::new(),
        persona: None,
        block: Some(block()),
        robots: None,
        cached: true,
        cached_at: Some(now()),
        reason: Some("domain_paused".to_owned()),
        visit: Visit::default(),
        reading: None,
        created_at: now(),
    };
    store.results().insert(&blocked_record).await.unwrap();
    assert_eq!(
        store.results().get(blocked_record.id).await.unwrap(),
        Some(blocked_record.clone())
    );
    // A result that keeps nothing of a visit stores no document for it,
    // and a row without one (every row older than the column) reads as a
    // visit of which nothing is known.
    let kept: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT visit FROM results WHERE id = $1")
            .bind(blocked_record.id.as_uuid())
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(kept, None);
    assert!(blocked_record.visit.is_empty());
    // A document that is no visit is a row that does not parse.
    sqlx::query("UPDATE results SET visit = '{\"side_effects\": 7}' WHERE id = $1")
        .bind(blocked_record.id.as_uuid())
        .execute(store.pool())
        .await
        .unwrap();
    assert!(store.results().get(blocked_record.id).await.is_err());
}

fn job(api_key_id: ApiKeyId, idempotency_key: Option<&str>) -> Job {
    Job {
        id: JobId::new(),
        api_key_id,
        kind: JobKind::Batch,
        state: JobState::Queued,
        spec: serde_json::json!({ "urls": ["https://example.org/a", "https://example.org/b"] }),
        idempotency_key: idempotency_key.map(str::to_owned),
        idempotency_hash: None,
        idempotency_until: None,
        origin: JobOrigin::Api,
        webhook_url: None,
        webhook_secret: None,
        items_total: 2,
        items_succeeded: 0,
        items_failed: 0,
        items_cancelled: 0,
        cancel_requested: false,
        error: None,
        deadline: None,
        paused_until: None,
        paused_reason: None,
        created_at: now(),
        started_at: None,
        finished_at: None,
    }
}

#[tokio::test]
async fn jobs_are_idempotent_per_key_and_track_progress() {
    let db = TestDb::migrated().await;
    let jobs = db.store.jobs();
    let key = common::seed_key(&db.store).await;
    let other_key = common::api_key("other", &[Scope::Jobs]);
    db.store.api_keys().insert(&other_key).await.unwrap();

    let mut first = job(key.id, Some("idem-1"));
    first.webhook_url = Some("https://hooks.example.net/kage".to_owned());
    first.webhook_secret = Some(SealedSecret::new(vec![5, 6, 7]));
    jobs.insert(&first).await.unwrap();
    assert_eq!(jobs.get(first.id).await.unwrap(), Some(first.clone()));
    assert_eq!(jobs.get(JobId::new()).await.unwrap(), None);

    // Re-submitting with the same key and idempotency key conflicts, and the
    // original job can be found to return instead.
    assert!(
        jobs.insert(&job(key.id, Some("idem-1")))
            .await
            .unwrap_err()
            .is_conflict()
    );
    assert_eq!(
        jobs.find_by_idempotency_key(key.id, "idem-1")
            .await
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        jobs.find_by_idempotency_key(key.id, "idem-2")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        jobs.find_by_idempotency_key(other_key.id, "idem-1")
            .await
            .unwrap(),
        None
    );
    // Another key may use the same idempotency key, and jobs without one
    // never collide.
    jobs.insert(&job(other_key.id, Some("idem-1")))
        .await
        .unwrap();
    jobs.insert(&job(key.id, None)).await.unwrap();
    jobs.insert(&job(key.id, None)).await.unwrap();

    let mut running = first.clone();
    running.state = JobState::Running;
    running.started_at = Some(now());
    running.items_succeeded = 1;
    running.cancel_requested = true;
    assert!(jobs.update(&running).await.unwrap());
    assert_eq!(jobs.get(first.id).await.unwrap(), Some(running.clone()));

    let mut done = running;
    done.state = JobState::Partial;
    done.items_failed = 1;
    done.error = Some("1 item failed".to_owned());
    done.finished_at = Some(now());
    assert!(jobs.update(&done).await.unwrap());
    let stored = jobs.get(first.id).await.unwrap().unwrap();
    assert_eq!(stored, done);
    assert_eq!(stored.items_pending(), 0);
    assert!(stored.state.is_terminal());

    assert!(!jobs.update(&job(key.id, None)).await.unwrap());
}

#[tokio::test]
async fn job_items_page_in_position_order() {
    let db = TestDb::migrated().await;
    let items = db.store.job_items();
    let key = common::seed_key(&db.store).await;
    let parent = job(key.id, None);
    db.store.jobs().insert(&parent).await.unwrap();
    let item = |seq: u64| JobItem {
        id: JobItemId::new(),
        job_id: parent.id,
        seq,
        state: JobItemState::Queued,
        input: serde_json::json!({ "url": format!("https://example.org/{seq}") }),
        site: Some("example.org".to_owned()),
        request_id: None,
        outcome: None,
        output: None,
        error: None,
        not_before: None,
        claims: 0,
        created_at: now(),
        started_at: None,
        finished_at: None,
    };
    let all: Vec<JobItem> = (0..5).map(item).collect();
    // Inserted in reverse; listed by position.
    for entry in all.iter().rev() {
        items.insert(entry).await.unwrap();
    }

    assert_eq!(items.list(parent.id, None, 100).await.unwrap(), all);
    assert_eq!(items.list(parent.id, None, 2).await.unwrap(), all[..2]);
    assert_eq!(items.list(parent.id, Some(1), 2).await.unwrap(), all[2..4]);
    assert_eq!(items.list(parent.id, Some(3), 2).await.unwrap(), all[4..]);
    assert_eq!(items.list(parent.id, Some(4), 2).await.unwrap(), []);
    assert_eq!(items.list(JobId::new(), None, 2).await.unwrap(), []);
    assert!(items.insert(&item(2)).await.unwrap_err().is_conflict());

    let request = common::request(Some(key.id));
    db.store.requests().insert(&request).await.unwrap();
    let mut finished = all[0].clone();
    finished.state = JobItemState::Failed;
    finished.request_id = Some(request.id);
    finished.error = Some("challenge_failed".to_owned());
    finished.outcome = Some(Outcome::ChallengeFailed);
    finished.output = Some(serde_json::json!({ "action": "goto", "duration_ms": 1200 }));
    finished.started_at = Some(now());
    finished.finished_at = Some(now());
    assert!(items.update(&finished).await.unwrap());
    assert_eq!(items.list(parent.id, None, 1).await.unwrap(), [finished]);
    assert!(!items.update(&item(9)).await.unwrap());

    // Items go with their job.
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(parent.id.as_uuid())
        .execute(db.store.pool())
        .await
        .unwrap();
    assert_eq!(items.list(parent.id, None, 100).await.unwrap(), []);
}

// One crawl followed from its seed to a page that was read, and what it
// keeps about itself.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn the_crawl_frontier_holds_one_entry_per_url_key_and_is_claimed_once() {
    let db = TestDb::migrated().await;
    let frontier = db.store.crawl_frontier();
    let key = common::seed_key(&db.store).await;
    let mut crawl = job(key.id, None);
    crawl.kind = JobKind::Crawl;
    crawl.items_total = 0;
    db.store.jobs().insert(&crawl).await.unwrap();
    let mut other_crawl = job(key.id, None);
    other_crawl.kind = JobKind::Crawl;
    db.store.jobs().insert(&other_crawl).await.unwrap();
    let start = now();
    let entry = |job_id: JobId, seq: u64, url: &str, url_key: &str, depth: u32| FrontierEntry {
        id: FrontierId::new(),
        job_id,
        seq,
        url: url.to_owned(),
        url_key: url_key.to_owned(),
        site: "example.org".to_owned(),
        depth,
        state: FrontierState::Queued,
        skip_reason: None,
        source: if seq == 0 {
            FrontierSource::Seed
        } else {
            FrontierSource::Link
        },
        found_on: (seq > 0).then_some(0),
        discovered_from: (seq > 0).then(|| "https://example.org/".to_owned()),
        furniture: false,
        links: Vec::new(),
        text_links: 0,
        spellings: Vec::new(),
        final_url: None,
        left_site: false,
        content_hash: None,
        simhash: None,
        duplicate_of: None,
        near_duplicate_of: None,
        lastmod: None,
        item_id: None,
        created_at: start + Duration::milliseconds(i64::try_from(seq).unwrap()),
        fetched_at: None,
    };

    let seed = entry(
        crawl.id,
        0,
        "https://example.org/",
        "https://example.org/",
        0,
    );
    let page_a = entry(
        crawl.id,
        1,
        "https://example.org/a",
        "https://example.org/a",
        1,
    );
    let page_b = entry(
        crawl.id,
        2,
        "https://example.org/b?utm_source=x",
        "https://example.org/b",
        1,
    );
    let mut hidden = entry(
        crawl.id,
        3,
        "https://example.org/trap",
        "https://example.org/trap",
        1,
    );
    hidden.state = FrontierState::Skipped;
    hidden.skip_reason = Some("hidden".to_owned());
    hidden.furniture = true;
    let mut from_sitemap = entry(
        crawl.id,
        4,
        "https://example.org/only",
        "https://example.org/only",
        0,
    );
    from_sitemap.source = FrontierSource::Sitemap;
    from_sitemap.found_on = None;
    from_sitemap.discovered_from = None;
    from_sitemap.lastmod = Some(start - Duration::days(3));
    // Nothing is written of a set that is empty; a set is written whole.
    frontier.save(&[]).await.unwrap();
    frontier
        .save(&[
            seed.clone(),
            page_a.clone(),
            page_b.clone(),
            hidden.clone(),
            from_sitemap.clone(),
        ])
        .await
        .unwrap();
    // Another crawl has a frontier of its own, with the same addresses.
    let elsewhere = entry(
        other_crawl.id,
        0,
        "https://example.org/b",
        "https://example.org/b",
        0,
    );
    frontier
        .save(std::slice::from_ref(&elsewhere))
        .await
        .unwrap();

    // In the order the crawl came to know them; the discovered spelling
    // is kept.
    let all = frontier.all(crawl.id).await.unwrap();
    assert_eq!(
        all,
        [
            seed.clone(),
            page_a.clone(),
            page_b.clone(),
            hidden.clone(),
            from_sitemap.clone()
        ]
    );
    assert_eq!(all[2].url, "https://example.org/b?utm_source=x");
    assert_eq!(frontier.all(other_crawl.id).await.unwrap(), [elsewhere]);
    assert_eq!(frontier.all(JobId::new()).await.unwrap(), []);
    assert_eq!(
        frontier.at(crawl.id, &[4, 1, 99]).await.unwrap(),
        [page_a.clone(), from_sitemap.clone()]
    );
    assert_eq!(frontier.at(crawl.id, &[]).await.unwrap(), []);

    // The same page under another spelling has the same key: the crawl
    // holds one entry per key, and a set that breaks that writes nothing.
    let respelled = entry(
        crawl.id,
        5,
        "https://example.org/b?utm_source=y#top",
        "https://example.org/b",
        2,
    );
    let fine = entry(
        crawl.id,
        6,
        "https://example.org/c",
        "https://example.org/c",
        2,
    );
    assert!(
        frontier
            .save(&[fine, respelled])
            .await
            .unwrap_err()
            .is_conflict()
    );
    assert_eq!(frontier.all(crawl.id).await.unwrap().len(), 5);
    // What is not read says why, and only what is not read does.
    let mut unexplained = entry(
        crawl.id,
        7,
        "https://example.org/d",
        "https://example.org/d",
        1,
    );
    unexplained.state = FrontierState::Skipped;
    assert!(matches!(
        frontier.save(&[unexplained]).await.unwrap_err(),
        StoreError::Database(_)
    ));

    // An entry is claimed for the item that reads its page, once.
    let item = JobItemId::new();
    assert!(frontier.claim(crawl.id, 0, item).await.unwrap());
    assert!(!frontier.claim(crawl.id, 0, JobItemId::new()).await.unwrap());
    // What is not waiting is nobody's, and neither is what is not there.
    assert!(!frontier.claim(crawl.id, 3, item).await.unwrap());
    assert!(!frontier.claim(crawl.id, 99, item).await.unwrap());
    assert!(!frontier.claim(other_crawl.id, 1, item).await.unwrap());
    let claimed = frontier.at(crawl.id, &[0]).await.unwrap().remove(0);
    assert_eq!(claimed.state, FrontierState::Fetching);
    assert_eq!(claimed.item_id, Some(item));
    // One that another worker holds at this moment is passed over, not
    // waited for.
    let mut held = db.store.pool().begin().await.unwrap();
    sqlx::query("SELECT id FROM crawl_frontier WHERE job_id = $1 AND seq = 1 FOR UPDATE")
        .bind(crawl.id.as_uuid())
        .execute(&mut *held)
        .await
        .unwrap();
    assert!(!frontier.claim(crawl.id, 1, JobItemId::new()).await.unwrap());
    held.rollback().await.unwrap();
    assert!(frontier.claim(crawl.id, 1, JobItemId::new()).await.unwrap());

    assert_eq!(
        frontier
            .count(crawl.id, FrontierState::Queued)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        frontier
            .count(crawl.id, FrontierState::Fetching)
            .await
            .unwrap(),
        2
    );

    // The page was read: what it says, what it links to, where it ended.
    // Its address, its key and its item stay as they are.
    let mut read = seed.clone();
    read.url = "https://example.org/changed".to_owned();
    read.state = FrontierState::Done;
    read.content_hash = Some("ab".repeat(32));
    // A SimHash uses all 64 bits, the sign bit included.
    read.simhash = Some(0xF000_0000_0000_000F);
    read.links = vec![1, 2, 3];
    read.text_links = 2;
    read.spellings = vec![(2, "https://example.org/b?utm_source=home".to_owned())];
    read.final_url = Some("https://example.org/home".to_owned());
    read.fetched_at = Some(now());
    let mut nearly = page_a.clone();
    nearly.state = FrontierState::Done;
    nearly.depth = 0;
    nearly.near_duplicate_of = Some((0, 5));
    nearly.fetched_at = Some(now());
    let mut same = page_b.clone();
    same.state = FrontierState::Done;
    same.duplicate_of = Some(0);
    same.left_site = true;
    frontier
        .save(&[read.clone(), nearly.clone(), same.clone()])
        .await
        .unwrap();
    read.url = seed.url.clone();
    read.item_id = Some(item);
    let stored = frontier.at(crawl.id, &[0, 1, 2]).await.unwrap();
    assert_eq!(stored[0], read);
    assert_eq!(stored[1].near_duplicate_of, Some((0, 5)));
    assert_eq!(stored[1].depth, 0);
    assert!(stored[1].item_id.is_some());
    assert_eq!(stored[2].duplicate_of, Some(0));
    assert!(stored[2].left_site);
    assert_eq!(
        frontier.count(crawl.id, FrontierState::Done).await.unwrap(),
        3
    );

    // What a crawl keeps about itself besides its frontier.
    let jobs = db.store.jobs();
    assert_eq!(jobs.crawl(crawl.id).await.unwrap(), serde_json::Value::Null);
    let kept = serde_json::json!({ "begun": 3, "tabs": { "example.org": [[0, 1]] } });
    jobs.set_crawl(crawl.id, &kept).await.unwrap();
    assert_eq!(jobs.crawl(crawl.id).await.unwrap(), kept);
    assert_eq!(
        jobs.crawl(other_crawl.id).await.unwrap(),
        serde_json::Value::Null
    );
    assert_eq!(
        jobs.crawl(JobId::new()).await.unwrap(),
        serde_json::Value::Null
    );

    // A crawl grows by an item for every page it asks for: the position
    // is given by the job, which counts it.
    let items = db.store.job_items();
    let page = |id: JobItemId| JobItem {
        id,
        job_id: crawl.id,
        // Whatever it says: the job knows its end.
        seq: 40,
        state: JobItemState::Running,
        input: serde_json::json!({ "url": "https://example.org/" }),
        site: Some("example.org".to_owned()),
        request_id: Some(RequestId::new()),
        outcome: None,
        output: None,
        error: None,
        not_before: None,
        claims: 1,
        created_at: start,
        started_at: Some(start),
        finished_at: None,
    };
    assert_eq!(items.append(&page(item)).await.unwrap(), Some(0));
    assert_eq!(
        items.append(&page(JobItemId::new())).await.unwrap(),
        Some(1)
    );
    let grown = jobs.get(crawl.id).await.unwrap().unwrap();
    assert_eq!(grown.items_total, 2);
    let listed = items.list(crawl.id, None, 10).await.unwrap();
    assert_eq!(
        listed.iter().map(|item| item.seq).collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(listed[0].id, item);
    assert_eq!(listed[0].state, JobItemState::Running);
    assert_eq!(listed[0].claims, 1);
    // A job that is gone grows by nothing.
    let mut orphan = page(JobItemId::new());
    orphan.job_id = JobId::new();
    assert_eq!(items.append(&orphan).await.unwrap(), None);

    // The frontier goes with its crawl.
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(crawl.id.as_uuid())
        .execute(db.store.pool())
        .await
        .unwrap();
    assert_eq!(frontier.all(crawl.id).await.unwrap(), []);
    assert_eq!(frontier.all(other_crawl.id).await.unwrap().len(), 1);
}

#[tokio::test]
async fn what_is_known_of_a_robots_txt_is_kept_until_it_is_asked_for_again() {
    let db = TestDb::migrated().await;
    let robots = db.store.robots_cache();
    let fetched_at = now();
    let later = fetched_at + Duration::hours(25);

    let mut entry = RobotsCacheEntry {
        origin: "https://www.example.org".to_owned(),
        site: "example.org".to_owned(),
        fetched_at,
        expires_at: fetched_at + Duration::hours(24),
        status: Some(200),
        body: Some("User-agent: *\nDisallow: /private\n".to_owned()),
        unreachable: false,
    };
    robots.put(&entry).await.unwrap();
    assert_eq!(
        robots.get("https://www.example.org").await.unwrap(),
        Some(entry.clone())
    );
    // An origin is a scheme, a host and a port: another of the same site
    // has a file of its own.
    assert_eq!(robots.get("http://www.example.org").await.unwrap(), None);
    assert_eq!(robots.get("https://example.org").await.unwrap(), None);
    // Asking again replaces the entry; a file that could not be had has
    // no status and no body.
    entry.fetched_at = later;
    entry.expires_at = later + Duration::hours(1);
    entry.status = None;
    entry.body = None;
    entry.unreachable = true;
    robots.put(&entry).await.unwrap();
    assert_eq!(
        robots.get("https://www.example.org").await.unwrap(),
        Some(entry)
    );
}

#[tokio::test]
async fn a_site_has_one_scout_and_everybody_else_who_asked_it_is_a_reader() {
    let db = TestDb::migrated().await;
    let scouts = db.store.site_scouts();
    let states = db.store.persona_domain_state();
    let scout = common::seed_persona(&db.store).await;
    let reader = common::seed_persona(&db.store).await;
    let deep_reader = common::seed_persona(&db.store).await;
    let stranger = common::seed_persona(&db.store).await;
    let at = now();
    // Readers are those that asked since a moment: before it, a persona
    // may scout the site again (`robots.scout_quarantine`).
    let lately = at - Duration::days(30);

    assert_eq!(scouts.get("example.org").await.unwrap(), None);
    assert_eq!(scouts.readers("example.org", lately).await.unwrap(), []);

    scouts.set("example.org", scout.id, at).await.unwrap();
    assert_eq!(scouts.get("example.org").await.unwrap(), Some(scout.id));
    assert_eq!(scouts.get("example.net").await.unwrap(), None);
    // Said again, nothing changes: the persona has been the scout since
    // it first asked.
    scouts
        .set("example.org", scout.id, at + Duration::hours(3))
        .await
        .unwrap();
    let since: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT since FROM site_scouts WHERE site = 'example.org'")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    assert_eq!(since, at);

    // Whoever asked a host of the site for a page is a reader of it; the
    // scout is not, though it asked too, and neither is a persona that
    // only has a row for it, or one that read another site whose name
    // ends alike.
    let asked = |persona: PersonaId, host: &str, did: bool| {
        let mut state = PersonaDomainState::new(persona, host);
        state.last_request_at = did.then_some(at);
        state
    };
    for state in [
        asked(scout.id, "example.org", true),
        asked(reader.id, "example.org", true),
        asked(deep_reader.id, "a.b.example.org", true),
        asked(deep_reader.id, "example.org", true),
        asked(stranger.id, "example.org", false),
        asked(stranger.id, "notexample.org", true),
        asked(stranger.id, "example.org.evil.test", true),
    ] {
        states.upsert(&state).await.unwrap();
    }
    let mut expected = vec![reader.id, deep_reader.id];
    expected.sort();
    assert_eq!(
        scouts.readers("example.org", lately).await.unwrap(),
        expected
    );
    assert_eq!(scouts.readers("example.net", lately).await.unwrap(), []);
    // A reader is one that asked after the moment: a second before what
    // they asked, and not at the moment itself.
    assert_eq!(
        scouts
            .readers("example.org", at - Duration::seconds(1))
            .await
            .unwrap(),
        expected
    );
    assert_eq!(scouts.readers("example.org", at).await.unwrap(), []);

    // Another persona takes the scout's place: the one before is a reader
    // like any other from then on, for it has asked.
    scouts
        .set("example.org", stranger.id, at + Duration::hours(4))
        .await
        .unwrap();
    assert_eq!(scouts.get("example.org").await.unwrap(), Some(stranger.id));
    let mut expected = vec![scout.id, reader.id, deep_reader.id];
    expected.sort();
    assert_eq!(
        scouts.readers("example.org", lately).await.unwrap(),
        expected
    );

    // A retired persona is nobody's scout, and the row goes with it.
    db.store
        .personas()
        .modify(stranger.id, |stored| {
            stored.state = PersonaState::Retired;
            stored.cooling_reason = None;
            stored.retired_at = Some(at);
        })
        .await
        .unwrap();
    assert_eq!(scouts.get("example.org").await.unwrap(), None);
    // A persona that does not exist cannot be one.
    assert!(
        scouts
            .set("example.net", PersonaId::new(), at)
            .await
            .is_err()
    );
}

// One entry followed from its first write to the result that takes it along.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn the_response_cache_serves_an_entry_until_it_expires() {
    let db = TestDb::migrated().await;
    let responses = db.store.response_cache();
    let fetched_at = now();

    let key = common::seed_key(&db.store).await;
    let request = common::request(Some(key.id));
    db.store.requests().insert(&request).await.unwrap();
    let record = FetchRecord {
        id: ResultId::new(),
        request_id: request.id,
        outcome: Outcome::Ok,
        status: Some(200),
        headers: Vec::new(),
        final_url: None,
        redirects: Vec::new(),
        extract: None,
        xhr: None,
        artifacts: Vec::new(),
        persona: None,
        block: None,
        robots: None,
        cached: false,
        cached_at: None,
        reason: None,
        visit: Visit::default(),
        reading: None,
        created_at: fetched_at,
    };
    db.store.results().insert(&record).await.unwrap();
    let mut cached = ResponseCacheEntry {
        cache_key: "k1".to_owned(),
        url_key: "https://example.org/listing?page=2".to_owned(),
        result_id: record.id,
        api_key_id: Some(key.id),
        cached_at: fetched_at,
        expires_at: fetched_at + Duration::minutes(10),
    };
    responses.put(&cached).await.unwrap();
    assert_eq!(
        responses.get("k1", fetched_at).await.unwrap(),
        Some(cached.clone())
    );
    assert_eq!(responses.get("k2", fetched_at).await.unwrap(), None);
    assert_eq!(
        responses
            .get("k1", fetched_at + Duration::minutes(10))
            .await
            .unwrap(),
        None
    );
    // A later visit replaces the entry; one that a policy shares between
    // keys is nobody's.
    cached.expires_at = fetched_at + Duration::minutes(30);
    cached.api_key_id = None;
    responses.put(&cached).await.unwrap();
    assert_eq!(
        responses
            .get("k1", fetched_at + Duration::minutes(10))
            .await
            .unwrap(),
        Some(cached.clone())
    );
    assert_eq!(
        responses
            .delete_expired(fetched_at + Duration::minutes(10))
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        responses
            .delete_expired(fetched_at + Duration::minutes(30))
            .await
            .unwrap(),
        1
    );

    // An entry goes with the result it serves, and so with the request.
    responses.put(&cached).await.unwrap();
    db.store
        .requests()
        .finish(request.id, Outcome::Ok, fetched_at)
        .await
        .unwrap();
    let (requests, _) = db
        .store
        .requests()
        .delete_before(fetched_at + Duration::hours(1), Some(key.id), true)
        .await
        .unwrap();
    assert_eq!(requests, 1);
    assert_eq!(responses.get("k1", fetched_at).await.unwrap(), None);

    // A result says when its page was fetched exactly when it was served
    // from the cache.
    let request = common::request(Some(key.id));
    db.store.requests().insert(&request).await.unwrap();
    let half = FetchRecord {
        id: ResultId::new(),
        request_id: request.id,
        cached: true,
        cached_at: None,
        ..record.clone()
    };
    assert!(db.store.results().insert(&half).await.is_err());
    let other_half = FetchRecord {
        cached: false,
        cached_at: Some(fetched_at),
        ..half.clone()
    };
    assert!(db.store.results().insert(&other_half).await.is_err());
    let whole = FetchRecord {
        cached_at: Some(fetched_at),
        ..half
    };
    db.store.results().insert(&whole).await.unwrap();
    assert_eq!(db.store.results().get(whole.id).await.unwrap(), Some(whole));
}

#[tokio::test]
async fn health_events_store_alerts_and_filter_by_subject() {
    let db = TestDb::migrated().await;
    let events = db.store.health_events();
    let persona = common::seed_persona(&db.store).await;

    let about_persona = HealthEvent {
        id: HealthEventId::new(),
        at: now(),
        subject_kind: SubjectKind::Persona,
        subject_id: Some(persona.id.as_uuid()),
        kind: "selftest_failed".to_owned(),
        severity: AlertSeverity::Warning,
        details: serde_json::json!({ "score": 0.4 }),
    };
    events.insert(&about_persona).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    // An operator alert becomes a health event about the policy files.
    let mut alert = Alert::new(
        AlertKind::PolicyReloadFailed,
        AlertSeverity::Critical,
        "a policy file is invalid; the last good policy stays in force",
        serde_json::json!({ "file": "policies/domains.yaml", "error": "unknown variant" }),
    );
    alert.at = now();
    let stored = events
        .record_alert(SubjectKind::Policy, &alert)
        .await
        .unwrap();
    assert_eq!(stored.kind, "policy_reload_failed");
    assert_eq!(stored.severity, AlertSeverity::Critical);
    assert_eq!(stored.at, alert.at);
    assert_eq!(stored.details["message"], alert.message);
    assert_eq!(stored.details["details"]["file"], "policies/domains.yaml");
    assert_eq!(stored.subject_id, None);

    assert_eq!(
        events.recent(None, 10).await.unwrap(),
        [stored.clone(), about_persona.clone()]
    );
    assert_eq!(
        events.recent(None, 1).await.unwrap(),
        std::slice::from_ref(&stored)
    );
    assert_eq!(
        events.recent(Some(SubjectKind::Policy), 10).await.unwrap(),
        [stored]
    );
    assert_eq!(
        events.recent(Some(SubjectKind::Persona), 10).await.unwrap(),
        [about_persona]
    );
    assert_eq!(
        events.recent(Some(SubjectKind::Proxy), 10).await.unwrap(),
        []
    );

    // An alert that names its subject is stored about that subject.
    let about_proxy = Alert::new(
        AlertKind::ProxyLeak,
        AlertSeverity::Critical,
        "the exit address is the host address",
        serde_json::json!({}),
    )
    .about(persona.proxy_id)
    .at(now());
    let stored_about_proxy = events
        .record_alert(SubjectKind::Proxy, &about_proxy)
        .await
        .unwrap();
    assert_eq!(
        stored_about_proxy.subject_id,
        Some(persona.proxy_id.as_uuid())
    );
    assert_eq!(stored_about_proxy.kind, "proxy_leak");
    assert_eq!(
        events.recent(Some(SubjectKind::Proxy), 10).await.unwrap(),
        [stored_about_proxy]
    );
}

#[tokio::test]
async fn selftest_runs_report_the_latest_per_path() {
    let db = TestDb::migrated().await;
    let runs = db.store.selftest_runs();
    let persona = common::seed_persona(&db.store).await;
    let start = now();
    let run = |render: Render, minutes: i64, passed: bool, score: Option<f32>| SelftestRun {
        id: SelftestRunId::new(),
        persona_id: persona.id,
        engine: Engine::Chromium,
        render,
        started_at: start + Duration::minutes(minutes),
        finished_at: start + Duration::minutes(minutes + 1),
        passed,
        score,
        findings: serde_json::json!({ "automation_flags": [] }),
    };

    assert_eq!(
        runs.latest(persona.id, Render::Browser).await.unwrap(),
        None
    );

    let old_browser = run(Render::Browser, 0, true, Some(0.97));
    let new_browser = run(Render::Browser, 60, false, Some(0.41));
    let http = run(Render::Http, 30, true, None);
    // Inserted newest first: the answer follows the start time, not the order.
    for entry in [&new_browser, &old_browser, &http] {
        runs.insert(entry).await.unwrap();
    }

    assert_eq!(
        runs.latest(persona.id, Render::Browser).await.unwrap(),
        Some(new_browser)
    );
    assert_eq!(
        runs.latest(persona.id, Render::Http).await.unwrap(),
        Some(http)
    );
    assert_eq!(
        runs.latest(PersonaId::new(), Render::Http).await.unwrap(),
        None
    );

    // The history of a persona, newest first, over both paths.
    let recent = runs.recent(persona.id, 10).await.unwrap();
    let started: Vec<_> = recent.iter().map(|run| run.started_at).collect();
    assert_eq!(
        started,
        [
            start + Duration::minutes(60),
            start + Duration::minutes(30),
            start
        ]
    );
    assert_eq!(runs.recent(persona.id, 1).await.unwrap().len(), 1);
    assert_eq!(runs.recent(PersonaId::new(), 10).await.unwrap(), []);
    assert_eq!(runs.count(persona.id, Render::Browser).await.unwrap(), 2);
    assert_eq!(runs.count(persona.id, Render::Http).await.unwrap(), 1);
    assert_eq!(runs.count(PersonaId::new(), Render::Http).await.unwrap(), 0);
}

#[tokio::test]
async fn domain_breakers_are_upserted_and_listed_when_open() {
    let db = TestDb::migrated().await;
    let breakers = db.store.domain_breakers();
    let at = now();

    assert_eq!(breakers.get("example.org").await.unwrap(), None);

    let mut breaker = DomainBreaker::closed("example.org", at);
    breaker.block_count_1h = 3;
    breaker.window_started_at = Some(at);
    breakers.upsert(&breaker).await.unwrap();
    assert_eq!(
        breakers.get("example.org").await.unwrap(),
        Some(breaker.clone())
    );
    assert_eq!(breakers.list_open().await.unwrap(), []);

    // The fifth block opens it for thirty minutes.
    breaker.state = BreakerState::Open;
    breaker.block_count_1h = 5;
    breaker.paused_until = Some(at + Duration::minutes(30));
    breaker.updated_at = at + Duration::minutes(1);
    breakers.upsert(&breaker).await.unwrap();
    let stored = breakers.get("example.org").await.unwrap().unwrap();
    assert_eq!(stored, breaker);
    assert!(stored.is_open(at + Duration::minutes(29)));
    assert!(!stored.is_open(at + Duration::minutes(30)));

    breakers
        .upsert(&DomainBreaker::closed("example.net", at))
        .await
        .unwrap();
    assert_eq!(breakers.list_open().await.unwrap(), [breaker]);
}

#[tokio::test]
async fn values_too_large_for_their_column_are_refused_before_the_query() {
    let db = TestDb::migrated().await;
    let key = common::seed_key(&db.store).await;

    let mut huge = job(key.id, None);
    huge.items_total = u32::MAX;
    let error = db.store.jobs().insert(&huge).await.unwrap_err();
    assert!(matches!(
        error,
        StoreError::OutOfRange {
            table: "jobs",
            column: "items_total"
        }
    ));

    let mut oversized = common::proxy("ref-1");
    oversized.consecutive_failures = u32::MAX;
    assert!(matches!(
        db.store.proxies().insert(&oversized).await,
        Err(StoreError::OutOfRange { .. })
    ));
    // Nothing was written.
    assert_eq!(db.store.proxies().list(None).await.unwrap(), []);
}

fn http_cookie(domain: &str, name: &str) -> HttpCookie {
    HttpCookie {
        domain: domain.to_owned(),
        host_only: true,
        path: "/".to_owned(),
        name: name.to_owned(),
        value: "v".to_owned(),
        secure: true,
        http_only: false,
        same_site: None,
        expires_at: None,
        created_at: now(),
        last_used_at: now(),
    }
}

// One jar followed through its whole life: each step needs the one before.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn cookie_jars_are_kept_per_proxy_in_order_and_die_with_it() {
    let db = TestDb::migrated().await;
    let cookies = db.store.http_cookies();
    let first = common::proxy("ref-1");
    let second = common::proxy("ref-2");
    db.store.proxies().insert(&first).await.unwrap();
    db.store.proxies().insert(&second).await.unwrap();
    assert_eq!(cookies.list(first.id, None).await.unwrap(), []);
    assert_eq!(cookies.count(first.id).await.unwrap(), 0);

    let mut session = http_cookie("www.example.org", "sid");
    session.http_only = true;
    session.same_site = Some(CookieSameSite::Lax);
    let mut lasting = http_cookie("example.org", "theme");
    lasting.host_only = false;
    lasting.path = "/shop".to_owned();
    lasting.secure = false;
    lasting.same_site = Some(CookieSameSite::None);
    lasting.expires_at = Some(now() + Duration::days(30));
    lasting.created_at = now() - Duration::days(2);
    // The same name under another path and scope is another cookie.
    let mut scoped = http_cookie("example.org", "theme");
    scoped.same_site = Some(CookieSameSite::Strict);
    let jar = vec![session.clone(), lasting.clone(), scoped.clone()];

    cookies.replace(first.id, None, &jar).await.unwrap();

    // Read back equal, in the order given, whatever the timestamps say.
    assert_eq!(cookies.list(first.id, None).await.unwrap(), jar);
    assert_eq!(cookies.count(first.id).await.unwrap(), 3);
    assert_eq!(cookies.list(second.id, None).await.unwrap(), []);

    // Replacing swaps the whole jar.
    let other = http_cookie("api.example.net", "token");
    cookies
        .replace(first.id, None, &[scoped.clone(), other.clone()])
        .await
        .unwrap();
    assert_eq!(
        cookies.list(first.id, None).await.unwrap(),
        [scoped.clone(), other.clone()]
    );

    // The same cookie twice is refused, and the jar stays as it was.
    let conflict = cookies
        .replace(first.id, None, &[session.clone(), session.clone()])
        .await
        .unwrap_err();
    assert!(conflict.is_conflict(), "{conflict}");
    assert_eq!(cookies.count(first.id).await.unwrap(), 2);
    // A jar needs its proxy.
    assert!(
        cookies
            .replace(ProxyId::new(), None, &[session.clone()])
            .await
            .is_err()
    );

    // One site's cookies can be purged, subdomains included, and only it.
    cookies
        .replace(
            second.id,
            None,
            &[
                http_cookie("example.org", "a"),
                http_cookie("shop.example.org", "b"),
                http_cookie("notexample.org", "c"),
                http_cookie("example.net", "d"),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        cookies
            .delete_for_domain(second.id, ".Example.org")
            .await
            .unwrap(),
        2
    );
    let left: Vec<String> = cookies
        .list(second.id, None)
        .await
        .unwrap()
        .into_iter()
        .map(|cookie| cookie.domain)
        .collect();
    assert_eq!(left, ["notexample.org", "example.net"]);
    assert_eq!(
        cookies
            .delete_for_domain(second.id, "example.org")
            .await
            .unwrap(),
        0
    );

    // A jar belongs to the persona that wrote it: another persona behind
    // the same proxy, and nobody, read nothing of it.
    let owner = common::persona(first.id);
    db.store.personas().insert(&owner).await.unwrap();
    cookies
        .replace(first.id, Some(owner.id), &[session.clone(), other.clone()])
        .await
        .unwrap();
    assert_eq!(
        cookies.list(first.id, Some(owner.id)).await.unwrap(),
        [session.clone(), other.clone()]
    );
    assert_eq!(cookies.list(first.id, None).await.unwrap(), []);
    assert_eq!(
        cookies
            .list(first.id, Some(PersonaId::new()))
            .await
            .unwrap(),
        []
    );
    assert_eq!(cookies.count(first.id).await.unwrap(), 2);
    // A persona that does not exist owns nothing.
    assert!(
        cookies
            .replace(first.id, Some(PersonaId::new()), &[session.clone()])
            .await
            .is_err()
    );
    assert_eq!(cookies.count(first.id).await.unwrap(), 2);
    // Retiring the persona deletes its rows, and only its rows.
    assert_eq!(cookies.delete_for_persona(owner.id).await.unwrap(), 2);
    assert_eq!(cookies.delete_for_persona(owner.id).await.unwrap(), 0);
    assert_eq!(cookies.count(second.id).await.unwrap(), 2);
    cookies
        .replace(first.id, Some(owner.id), &[scoped.clone(), other.clone()])
        .await
        .unwrap();

    // A retired persona's jar is deleted outright.
    assert_eq!(cookies.delete_for_proxy(first.id).await.unwrap(), 2);
    assert_eq!(cookies.delete_for_proxy(first.id).await.unwrap(), 0);
    assert_eq!(cookies.list(first.id, Some(owner.id)).await.unwrap(), []);
    assert_eq!(cookies.count(second.id).await.unwrap(), 2);

    // The schema refuses what the jar never writes: a domain that is not
    // lowercase or is empty, a relative path, an unknown SameSite value, a
    // negative position.
    for statement in [
        "INSERT INTO http_cookies (proxy_id, domain, host_only, path, name, value, secure,
                                   http_only, created_at, last_used_at, ordinal)
         VALUES ($1, 'Example.ORG', true, '/', 'bad', 'v', false, false, now(), now(), 9)",
        "INSERT INTO http_cookies (proxy_id, domain, host_only, path, name, value, secure,
                                   http_only, created_at, last_used_at, ordinal)
         VALUES ($1, '', true, '/', 'bad', 'v', false, false, now(), now(), 9)",
        "INSERT INTO http_cookies (proxy_id, domain, host_only, path, name, value, secure,
                                   http_only, created_at, last_used_at, ordinal)
         VALUES ($1, 'example.org', true, 'relative', 'bad', 'v', false, false, now(), now(), 9)",
        "INSERT INTO http_cookies (proxy_id, domain, host_only, path, name, value, secure,
                                   http_only, same_site, created_at, last_used_at, ordinal)
         VALUES ($1, 'example.org', true, '/', 'bad', 'v', false, false, 'sometimes', now(),
                 now(), 9)",
        "INSERT INTO http_cookies (proxy_id, domain, host_only, path, name, value, secure,
                                   http_only, created_at, last_used_at, ordinal)
         VALUES ($1, 'example.org', true, '/', 'bad', 'v', false, false, now(), now(), -1)",
    ] {
        let error = sqlx::query(statement)
            .bind(second.id.as_uuid())
            .execute(db.store.pool())
            .await
            .unwrap_err();
        assert!(
            error
                .as_database_error()
                .is_some_and(sqlx::error::DatabaseError::is_check_violation),
            "{statement}: {error}"
        );
    }
    // The rows go when the persona row goes, and when the proxy row goes.
    cookies
        .replace(first.id, Some(owner.id), &[session.clone()])
        .await
        .unwrap();
    sqlx::query("DELETE FROM personas WHERE id = $1")
        .bind(owner.id.as_uuid())
        .execute(db.store.pool())
        .await
        .unwrap();
    assert_eq!(cookies.count(first.id).await.unwrap(), 0);
    cookies
        .replace(first.id, None, &[session.clone()])
        .await
        .unwrap();
    sqlx::query("DELETE FROM proxies WHERE id = $1")
        .bind(first.id.as_uuid())
        .execute(db.store.pool())
        .await
        .unwrap();
    assert_eq!(cookies.count(first.id).await.unwrap(), 0);
    // And a stored value that does not parse is reported, not guessed.
    sqlx::query("ALTER TABLE http_cookies DROP CONSTRAINT http_cookies_same_site_check")
        .execute(db.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE http_cookies SET same_site = 'sometimes' WHERE proxy_id = $1")
        .bind(second.id.as_uuid())
        .execute(db.store.pool())
        .await
        .unwrap();
    assert!(matches!(
        cookies.list(second.id, None).await,
        Err(StoreError::Corrupt {
            table: "http_cookies",
            column: "same_site",
            ..
        })
    ));
}

#[tokio::test]
async fn an_engine_upgrade_moves_the_browser_columns_once_and_nothing_else() {
    let db = TestDb::migrated().await;
    let persona = common::seed_persona(&db.store).await;
    let upgrade = BrowserUpgrade {
        fingerprint: serde_json::json!({
            "schema": 1,
            "browser_version": "155.0.8059.39",
            "native": { "hardware_concurrency": 8 },
        }),
        browser_major: 155,
        user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                     Chrome/155.0.0.0 Safari/537.36"
            .to_owned(),
        wreq_profile: None,
    };

    let Upgraded {
        persona: moved,
        moved: wrote,
    } = db
        .store
        .personas()
        .upgrade_browser(persona.id, 149, &upgrade)
        .await
        .unwrap()
        .unwrap();
    assert!(wrote);
    assert_eq!(moved.browser_major, 155);
    assert_eq!(moved.fingerprint, upgrade.fingerprint);
    assert_eq!(moved.user_agent, upgrade.user_agent);
    assert_eq!(moved.wreq_profile, None);
    // What is stored is what came back, and nothing of the identity moved.
    let stored = db.store.personas().get(persona.id).await.unwrap().unwrap();
    assert_eq!(stored, moved);
    let expected = Persona {
        browser_major: 155,
        fingerprint: upgrade.fingerprint.clone(),
        user_agent: upgrade.user_agent.clone(),
        wreq_profile: None,
        ..persona.clone()
    };
    assert_eq!(stored, expected);

    // A second move from the old major finds it moved and writes nothing.
    let again = BrowserUpgrade {
        browser_major: 156,
        ..upgrade.clone()
    };
    let Upgraded {
        persona: found,
        moved: wrote,
    } = db
        .store
        .personas()
        .upgrade_browser(persona.id, 149, &again)
        .await
        .unwrap()
        .unwrap();
    assert!(!wrote);
    assert_eq!(found, expected);
    assert_eq!(
        db.store.personas().get(persona.id).await.unwrap().unwrap(),
        expected
    );

    // A persona that does not exist is none.
    assert_eq!(
        db.store
            .personas()
            .upgrade_browser(PersonaId::new(), 149, &upgrade)
            .await
            .unwrap(),
        None
    );
}
