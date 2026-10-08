//! Every repository against a real Postgres: what is written reads back
//! equal, unique keys conflict, listings filter and page.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use chrono::{Duration, NaiveDate};
use common::{TestDb, now};
use kage_core::alert::{Alert, AlertKind, AlertSeverity};
use kage_core::domain::{
    ArtifactKind, ArtifactRef, Attempt, AuditActor, AuditEvent, AuditOutcome, BreakerState,
    ClaimedOs, DomainBreaker, Engine, FetchRecord, FrontierEntry, FrontierState, Header,
    HealthEvent, Job, JobItem, JobItemState, JobKind, JobState, PersonaDomainState, PersonaState,
    PersonaSummary, ProxyHealth, ProxyState, Render, RobotsVerdict, Scope, SelftestRun, Session,
    SessionId, SessionLimits, SubjectKind, Timing,
};
use kage_core::ids::{
    ApiKeyId, ArtifactId, AttemptId, AuditEventId, FrontierId, HealthEventId, JobId, JobItemId,
    PersonaId, ProxyId, RequestId, ResultId, SelftestRunId,
};
use kage_core::outcome::{Block, BlockType, Outcome, Vendor};
use kage_core::secret::SealedSecret;
use kage_store::StoreError;
use kage_store::repo::{ResponseCacheEntry, RobotsCacheEntry};

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

    // The provider's reference is unique per provider.
    let mut duplicate = common::proxy("ref-1");
    duplicate.host = "198.51.100.99".to_owned();
    let conflict = proxies.insert(&duplicate).await.unwrap_err();
    assert!(conflict.is_conflict(), "{conflict}");

    // A health check moves the proxy through the pool states.
    let mut updated = first.clone();
    updated.state = ProxyState::Backoff;
    updated.consecutive_failures = 3;
    updated.backoff_until = Some(now() + Duration::minutes(4));
    updated.password = SealedSecret::new(vec![1, 2, 3]);
    updated.last_health = Some(ProxyHealth {
        checked_at: now(),
        reachable: false,
        latency_ms: None,
        exit_ip: Some("203.0.113.5".parse().unwrap()),
        exit_country: Some("DE".parse().unwrap()),
        country_match: Some(true),
        leak_detected: Some(false),
        reputation_score: Some(12),
    });
    assert!(proxies.update(&updated).await.unwrap());
    assert_eq!(proxies.get(first.id).await.unwrap(), Some(updated.clone()));
    assert_eq!(
        proxies.list(Some(ProxyState::Backoff)).await.unwrap(),
        [updated]
    );
    assert_eq!(proxies.list(Some(ProxyState::New)).await.unwrap(), [second]);

    // Updating a proxy that does not exist reports it.
    assert!(!proxies.update(&common::proxy("ghost")).await.unwrap());
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

    // A proxy has exactly one persona.
    let conflict = personas
        .insert(&common::persona(proxy.id))
        .await
        .unwrap_err();
    assert!(conflict.is_conflict());

    // A persona without geolocation or a wreq profile is browser-only.
    let other_proxy = common::proxy("ref-2");
    db.store.proxies().insert(&other_proxy).await.unwrap();
    let mut browser_only = common::persona(other_proxy.id);
    browser_only.engine = Engine::Firefox;
    browser_only.claimed_os = ClaimedOs::Linux;
    browser_only.wreq_profile = None;
    browser_only.geolocation = None;
    personas.insert(&browser_only).await.unwrap();
    assert_eq!(
        personas.get(browser_only.id).await.unwrap(),
        Some(browser_only.clone())
    );

    // Engine upgrade: major and profile change together.
    assert!(
        personas
            .set_browser(persona.id, 142, Some("chrome_142"))
            .await
            .unwrap()
    );
    let upgraded = personas.get(persona.id).await.unwrap().unwrap();
    assert_eq!(upgraded.browser_major, 142);
    assert_eq!(upgraded.wreq_profile.as_deref(), Some("chrome_142"));
    assert!(personas.set_browser(persona.id, 143, None).await.unwrap());
    assert_eq!(
        personas
            .get(persona.id)
            .await
            .unwrap()
            .unwrap()
            .wreq_profile,
        None
    );

    // Cooling, then retired with a timestamp.
    assert!(
        personas
            .set_state(persona.id, PersonaState::Cooling, None)
            .await
            .unwrap()
    );
    let retired_at = now();
    assert!(
        personas
            .set_state(persona.id, PersonaState::Retired, Some(retired_at))
            .await
            .unwrap()
    );
    let retired = personas.get(persona.id).await.unwrap().unwrap();
    assert_eq!(retired.state, PersonaState::Retired);
    assert_eq!(retired.retired_at, Some(retired_at));
    assert_eq!(
        personas.list(Some(PersonaState::Active)).await.unwrap(),
        [browser_only]
    );

    assert!(
        !personas
            .set_state(PersonaId::new(), PersonaState::Cooling, None)
            .await
            .unwrap()
    );
    assert!(
        !personas
            .set_browser(PersonaId::new(), 1, None)
            .await
            .unwrap()
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
    let mut expected = vec![state, other_state];
    expected.sort_by_key(|state| state.persona_id);
    assert_eq!(for_domain, expected);
    assert_eq!(
        states.list_for_domain("example.net").await.unwrap(),
        [elsewhere]
    );
    assert_eq!(states.list_for_domain("unknown.test").await.unwrap(), []);
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
        created_at: now(),
    };
    store.results().insert(&record).await.unwrap();
    assert_eq!(
        store.results().get(record.id).await.unwrap(),
        Some(record.clone())
    );
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
        created_at: now(),
    };
    store.results().insert(&blocked_record).await.unwrap();
    assert_eq!(
        store.results().get(blocked_record.id).await.unwrap(),
        Some(blocked_record)
    );
}

fn job(api_key_id: ApiKeyId, idempotency_key: Option<&str>) -> Job {
    Job {
        id: JobId::new(),
        api_key_id,
        kind: JobKind::Batch,
        state: JobState::Queued,
        spec: serde_json::json!({ "urls": ["https://example.org/a", "https://example.org/b"] }),
        idempotency_key: idempotency_key.map(str::to_owned),
        webhook_url: None,
        webhook_secret: None,
        items_total: 2,
        items_succeeded: 0,
        items_failed: 0,
        cancel_requested: false,
        error: None,
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
        request_id: None,
        error: None,
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

// One crawl followed from its seed to a fetched page.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn the_crawl_frontier_holds_one_entry_per_url_key() {
    let db = TestDb::migrated().await;
    let frontier = db.store.crawl_frontier();
    let key = common::seed_key(&db.store).await;
    let mut crawl = job(key.id, None);
    crawl.kind = JobKind::Crawl;
    db.store.jobs().insert(&crawl).await.unwrap();
    let mut other_crawl = job(key.id, None);
    other_crawl.kind = JobKind::Crawl;
    db.store.jobs().insert(&other_crawl).await.unwrap();
    let start = now();
    let entry =
        |job_id: JobId, url: &str, url_key: &str, depth: u32, offset_ms: i64| FrontierEntry {
            id: FrontierId::new(),
            job_id,
            url: url.to_owned(),
            url_key: url_key.to_owned(),
            depth,
            state: FrontierState::Queued,
            discovered_from: Some("https://example.org/".to_owned()),
            content_hash: None,
            simhash: None,
            near_duplicate: false,
            created_at: start + Duration::milliseconds(offset_ms),
            fetched_at: None,
        };

    let seed = entry(
        crawl.id,
        "https://example.org/",
        "https://example.org/",
        0,
        0,
    );
    let page_b = entry(
        crawl.id,
        "https://example.org/b?utm_source=x",
        "https://example.org/b",
        1,
        20,
    );
    let page_a = entry(
        crawl.id,
        "https://example.org/a",
        "https://example.org/a",
        1,
        10,
    );
    for new in [&seed, &page_b, &page_a] {
        assert!(frontier.insert_if_new(new).await.unwrap());
    }
    // The same page under another spelling has the same key: a duplicate.
    let respelled = entry(
        crawl.id,
        "https://example.org/b?utm_source=y#top",
        "https://example.org/b",
        2,
        30,
    );
    assert!(!frontier.insert_if_new(&respelled).await.unwrap());
    // Another crawl has its own frontier.
    assert!(
        frontier
            .insert_if_new(&entry(
                other_crawl.id,
                "https://example.org/b",
                "https://example.org/b",
                0,
                0
            ))
            .await
            .unwrap()
    );

    // Shallowest first, then oldest; the discovered spelling is kept.
    let queued = frontier
        .list(crawl.id, FrontierState::Queued, 10)
        .await
        .unwrap();
    assert_eq!(queued, [seed.clone(), page_a.clone(), page_b.clone()]);
    assert_eq!(queued[2].url, "https://example.org/b?utm_source=x");
    assert_eq!(
        frontier
            .list(crawl.id, FrontierState::Queued, 2)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        frontier
            .count(crawl.id, FrontierState::Queued)
            .await
            .unwrap(),
        3
    );
    assert_eq!(
        frontier.count(crawl.id, FrontierState::Done).await.unwrap(),
        0
    );

    let mut fetched = seed.clone();
    fetched.state = FrontierState::Done;
    fetched.content_hash = Some("ab".repeat(32));
    // A SimHash uses all 64 bits, the sign bit included.
    fetched.simhash = Some(0xF000_0000_0000_000F);
    fetched.near_duplicate = true;
    fetched.fetched_at = Some(now());
    assert!(frontier.update(&fetched).await.unwrap());
    assert_eq!(
        frontier
            .list(crawl.id, FrontierState::Done, 10)
            .await
            .unwrap(),
        [fetched]
    );
    assert_eq!(
        frontier
            .count(crawl.id, FrontierState::Queued)
            .await
            .unwrap(),
        2
    );
    assert!(
        !frontier
            .update(&entry(crawl.id, "x", "x", 0, 0))
            .await
            .unwrap()
    );
}

// Both caches follow one timeline, which is what makes the expiry checks readable.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn caches_serve_entries_until_they_expire() {
    let db = TestDb::migrated().await;
    let robots = db.store.robots_cache();
    let responses = db.store.response_cache();
    let fetched_at = now();
    let later = fetched_at + Duration::hours(25);

    let mut entry = RobotsCacheEntry {
        origin: "https://example.org".to_owned(),
        fetched_at,
        expires_at: fetched_at + Duration::hours(24),
        status: Some(200),
        body: Some("User-agent: *\nDisallow: /private\n".to_owned()),
    };
    robots.put(&entry).await.unwrap();
    assert_eq!(
        robots.get("https://example.org", fetched_at).await.unwrap(),
        Some(entry.clone())
    );
    assert_eq!(
        robots.get("https://example.net", fetched_at).await.unwrap(),
        None
    );
    // Expired entries are not served.
    assert_eq!(
        robots.get("https://example.org", later).await.unwrap(),
        None
    );
    // A refetch replaces the entry; a failed fetch has no status or body.
    entry.fetched_at = later;
    entry.expires_at = later + Duration::hours(24);
    entry.status = None;
    entry.body = None;
    robots.put(&entry).await.unwrap();
    assert_eq!(
        robots.get("https://example.org", later).await.unwrap(),
        Some(entry)
    );
    assert_eq!(robots.delete_expired(later).await.unwrap(), 0);
    assert_eq!(
        robots
            .delete_expired(later + Duration::hours(24))
            .await
            .unwrap(),
        1
    );

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
        created_at: fetched_at,
    };
    db.store.results().insert(&record).await.unwrap();
    let mut cached = ResponseCacheEntry {
        cache_key: "k1".to_owned(),
        url_key: "https://example.org/listing?page=2".to_owned(),
        result_id: record.id,
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
    cached.expires_at = fetched_at + Duration::minutes(30);
    responses.put(&cached).await.unwrap();
    assert_eq!(
        responses
            .get("k1", fetched_at + Duration::minutes(10))
            .await
            .unwrap(),
        Some(cached)
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
