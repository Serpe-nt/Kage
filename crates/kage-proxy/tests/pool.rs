//! The pool against a real Postgres and a mock of the Webshare API: sync
//! with the provider, credentials at rest, the replacement lifecycle and its
//! budget, the leak flag, and the alerts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use common::{
    MASTER_KEY, MASTER_KEY_BYTES, MockWebshare, PROXY_PASS, PROXY_USER, TestDb, WEBSHARE_TOKEN,
};
use kage_core::alert::{AlertKind, AlertSeverity, MemoryAlertSink};
use kage_core::config::{Config, StaticProxy, WebshareConfig};
use kage_core::domain::{
    GeoRecord, HealthEvent, HealthFailure, Proxy, ProxyHealth, ProxyProviderKind, ProxyState,
    ReplaceReason,
};
use kage_core::duration::HumanDuration;
use kage_core::ids::ProxyId;
use kage_core::secret::Secret;
use kage_core::seed::Seed;
use kage_proxy::machine::{ExitObservation, ProbeReport, ReputationReport};
use kage_proxy::{
    AutoReplace, BudgetWindow, CredentialCipher, CryptoError, ManualClock, Pool, PoolError,
    PoolEvent, PoolParts, PoolRules, ProviderError, ProxyProvider, ReplacementOutcome,
    StaticListProvider, WebshareProvider,
};
use serde_json::json;

/// Mid-month, so a test that moves a few days stays inside one calendar
/// month unless it means to leave it.
fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
}

struct Harness {
    db: TestDb,
    mock: MockWebshare,
    pool: Arc<Pool>,
    clock: Arc<ManualClock>,
    alerts: Arc<MemoryAlertSink>,
}

fn webshare(mock: &MockWebshare) -> Arc<dyn ProxyProvider> {
    Arc::new(
        WebshareProvider::new(&WebshareConfig {
            api_key: Some(Secret::new(WEBSHARE_TOKEN)),
            base_url: mock.base_url(),
            timeout: HumanDuration::from_secs(5),
            rate_limit_backoff: HumanDuration::new(Duration::from_millis(20)),
            // The mock is local: no need to pace the tests.
            requests_per_minute: 60_000,
            list_requests_per_minute: 60_000,
            max_retries: 0,
            ..WebshareConfig::default()
        })
        .unwrap(),
    )
}

async fn harness_with(rules: PoolRules, proxies: u32) -> Harness {
    let db = TestDb::migrated().await;
    let mock = MockWebshare::start().await;
    let clock = Arc::new(ManualClock::new(start()));
    mock.with(|state| {
        state.now = start();
        state.add_proxies(proxies, "DE");
    });
    let alerts = Arc::new(MemoryAlertSink::new());
    let pool = Arc::new(Pool::new(PoolParts {
        store: db.store.clone(),
        provider: Some(webshare(&mock)),
        cipher: CredentialCipher::new(&MASTER_KEY_BYTES),
        rules,
        missing_syncs: 3,
        alerts: alerts.clone(),
        clock: clock.clone(),
        seed: Seed::from_u64(7),
    }));
    Harness {
        db,
        mock,
        pool,
        clock,
        alerts,
    }
}

async fn harness(proxies: u32) -> Harness {
    harness_with(PoolRules::default(), proxies).await
}

impl Harness {
    fn now(&self) -> DateTime<Utc> {
        self.pool.clock().now()
    }

    /// Moves the pool's clock and the mock's clock together.
    fn advance(&self, by: chrono::Duration) {
        self.clock.advance(by);
        let now = self.now();
        self.mock.with(|state| state.now = now);
    }

    async fn proxy(&self, id: ProxyId) -> Proxy {
        self.pool.get(id).await.unwrap().unwrap()
    }

    /// The live proxy at this address.
    async fn at(&self, ip: &str) -> Proxy {
        self.pool
            .live()
            .await
            .unwrap()
            .into_iter()
            .find(|proxy| proxy.host == ip)
            .expect("no live proxy at this address")
    }

    fn exit(proxy: &Proxy) -> IpAddr {
        proxy.host.parse().unwrap()
    }

    async fn pass(&self, proxy: &Proxy) -> Proxy {
        let report = ProbeReport {
            at: self.now(),
            exit: Ok(ExitObservation {
                ip: Self::exit(proxy),
                latency_ms: 120,
                connect_ms: None,
                tls_ms: None,
            }),
            leak: Some(false),
            geo: None,
        };
        self.pool
            .record_probe(proxy.id, &report)
            .await
            .unwrap()
            .unwrap()
    }

    async fn fail(&self, proxy: &Proxy) -> Proxy {
        let report = ProbeReport {
            at: self.now(),
            exit: Err(HealthFailure::Unreachable),
            leak: None,
            geo: None,
        };
        self.pool
            .record_probe(proxy.id, &report)
            .await
            .unwrap()
            .unwrap()
    }

    async fn leak(&self, proxy: &Proxy) -> Proxy {
        let report = ProbeReport {
            at: self.now(),
            exit: Ok(ExitObservation {
                ip: "192.0.2.77".parse().unwrap(),
                latency_ms: 3,
                connect_ms: None,
                tls_ms: None,
            }),
            leak: Some(true),
            geo: None,
        };
        self.pool
            .record_probe(proxy.id, &report)
            .await
            .unwrap()
            .unwrap()
    }

    /// Syncs with the provider and makes every new proxy healthy.
    async fn healthy_pool(&self) -> Vec<Proxy> {
        self.pool.reconcile().await.unwrap();
        let mut proxies = Vec::new();
        for proxy in self.pool.live().await.unwrap() {
            proxies.push(self.pass(&proxy).await);
        }
        proxies
    }

    /// Puts a proxy where a sustained outage would have put it: in backoff
    /// after several cycles, failing since the given time.
    async fn make_due(&self, proxy: &Proxy, failing_since: DateTime<Utc>) {
        let now = self.now();
        self.db
            .store
            .proxies()
            .modify(proxy.id, |stored| {
                stored.state = ProxyState::Backoff;
                stored.consecutive_failures = 9;
                stored.backoff_count = 6;
                stored.backoff_until = Some(now + chrono::Duration::minutes(45));
                stored.failing_since = Some(failing_since);
                stored.last_health = Some(ProxyHealth {
                    failure: Some(HealthFailure::Unreachable),
                    exit_ip: Some(Self::exit(proxy)),
                    ..ProxyHealth::new(now, false)
                });
            })
            .await
            .unwrap();
    }

    /// Lets the provider finish the replacement in flight: the mock needs
    /// three status reads, and the pool asks at most every fifteen seconds.
    async fn finish_replacement(&self) {
        for _ in 0..3 {
            self.advance(chrono::Duration::seconds(20));
            self.pool.tick().await.unwrap();
        }
    }

    async fn events(&self, kind: &str) -> Vec<HealthEvent> {
        self.db
            .store
            .health_events()
            .recent(None, 1000)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == kind)
            .collect()
    }

    fn alerts_of(&self, kind: AlertKind) -> usize {
        self.alerts
            .kinds()
            .into_iter()
            .filter(|raised| *raised == kind)
            .count()
    }

    async fn used_this_month(&self) -> u32 {
        self.pool.summary().await.unwrap().replacements_this_month
    }
}

#[tokio::test]
async fn a_sync_adopts_the_list_and_keeps_every_credential_encrypted() {
    let harness = harness(3).await;
    assert_eq!(
        harness.pool.provider_kind(),
        Some(ProxyProviderKind::Webshare)
    );
    let mut events = harness.pool.subscribe();

    let report = harness.pool.reconcile().await.unwrap();

    assert_eq!(report.listed, 3);
    assert_eq!(report.added.len(), 3);
    assert!(report.updated.is_empty() && report.retired.is_empty() && !report.suspect);
    let proxies = harness.pool.live().await.unwrap();
    assert_eq!(proxies.len(), 3);
    for proxy in &proxies {
        assert_eq!(proxy.state, ProxyState::New);
        assert_eq!(proxy.provider, ProxyProviderKind::Webshare);
        assert_eq!(proxy.country.unwrap().as_str(), "DE");
        assert_eq!(proxy.city.as_deref(), Some("Testville"));
        assert_eq!(proxy.created_at, start());
        // What comes back from the pool opens to what the provider listed.
        let endpoint = harness.pool.endpoint(proxy).unwrap();
        assert_eq!(endpoint.username.expose(), PROXY_USER);
        assert_eq!(endpoint.password.expose(), PROXY_PASS);
        assert_eq!(
            endpoint.url(),
            format!("http://{}:{}", proxy.host, proxy.port)
        );
        assert_eq!(
            events.recv().await.unwrap(),
            PoolEvent::Added { proxy: proxy.id }
        );
    }
    assert_eq!(harness.events("proxy_added").await.len(), 3);

    // What is in the table is neither the user name nor the password.
    let stored: Vec<(Vec<u8>, Vec<u8>)> = sqlx::query_as("SELECT username, password FROM proxies")
        .fetch_all(harness.db.store.pool())
        .await
        .unwrap();
    assert_eq!(stored.len(), 3);
    let contains = |haystack: &[u8], needle: &str| {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    };
    for (username, password) in &stored {
        assert!(!contains(username, PROXY_USER) && !contains(password, PROXY_PASS));
        assert!(!contains(username, PROXY_PASS) && !contains(password, PROXY_USER));
        // Version byte, nonce, ciphertext, tag.
        assert_eq!(username.len(), 1 + 24 + PROXY_USER.len() + 16);
        assert_eq!(password.len(), 1 + 24 + PROXY_PASS.len() + 16);
    }
    // The same user name on three proxies is three different ciphertexts.
    assert_ne!(stored[0].0, stored[1].0);
    assert_ne!(stored[1].0, stored[2].0);
    // Nothing in the stored events names a credential either.
    let events: Vec<(serde_json::Value,)> = sqlx::query_as("SELECT details FROM health_events")
        .fetch_all(harness.db.store.pool())
        .await
        .unwrap();
    for (details,) in events {
        let text = details.to_string();
        assert!(
            !text.contains(PROXY_PASS) && !text.contains(PROXY_USER),
            "{text}"
        );
    }

    // A second sync finds nothing to do and seals nothing again.
    let again = harness.pool.reconcile().await.unwrap();
    assert_eq!(again.listed, 3);
    assert!(again.added.is_empty() && again.updated.is_empty() && again.retired.is_empty());
    let unchanged: Vec<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT username, password FROM proxies")
            .fetch_all(harness.db.store.pool())
            .await
            .unwrap();
    assert_eq!(unchanged, stored);
}

#[tokio::test]
async fn changed_credentials_and_locations_are_taken_over() {
    let harness = harness(2).await;
    let proxies = harness.healthy_pool().await;
    let target = proxies[0].clone();

    // The account's proxy password is changed, and one proxy moves.
    harness.mock.with(|state| {
        for proxy in &mut state.proxies {
            proxy.password = "rotated-91cc".to_owned();
        }
        state.proxies[0].country = "AT".to_owned();
        state.proxies[0].city = "Vienna".to_owned();
    });
    let report = harness.pool.reconcile().await.unwrap();

    assert_eq!(report.updated.len(), 2);
    assert!(report.added.is_empty() && report.retired.is_empty());
    let moved = harness.proxy(target.id).await;
    assert_eq!(moved.country.unwrap().as_str(), "AT");
    assert_eq!(moved.city.as_deref(), Some("Vienna"));
    // Health and state are untouched by a change of credentials.
    assert_eq!(moved.state, ProxyState::Healthy);
    assert_eq!(moved.last_health, target.last_health);
    for proxy in harness.pool.live().await.unwrap() {
        let endpoint = harness.pool.endpoint(&proxy).unwrap();
        assert_eq!(endpoint.username.expose(), PROXY_USER);
        assert_eq!(endpoint.password.expose(), "rotated-91cc");
    }
    // The event says what changed, never the values.
    let updated = harness.events("proxy_updated").await;
    assert_eq!(updated.len(), 2);
    assert!(
        updated
            .iter()
            .any(|event| event.details["changed"] == json!(["credentials", "country", "city"]))
    );
    assert!(
        updated
            .iter()
            .all(|event| !event.details.to_string().contains("rotated-91cc"))
    );
}

#[tokio::test]
async fn credentials_sealed_under_another_key_are_sealed_again_at_the_next_sync() {
    let harness = harness(2).await;
    harness.pool.reconcile().await.unwrap();

    // The same database, opened with another master key.
    let other_key = Arc::new(Pool::new(PoolParts {
        store: harness.db.store.clone(),
        provider: Some(webshare(&harness.mock)),
        cipher: CredentialCipher::new(&[0x55; 32]),
        rules: PoolRules::default(),
        missing_syncs: 3,
        alerts: harness.alerts.clone(),
        clock: harness.clock.clone(),
        seed: Seed::from_u64(8),
    }));
    let proxies = other_key.live().await.unwrap();
    for proxy in &proxies {
        assert_eq!(
            other_key.endpoint(proxy).unwrap_err(),
            CryptoError::Rejected
        );
    }

    let report = other_key.reconcile().await.unwrap();

    assert_eq!(report.updated.len(), 2);
    for proxy in other_key.live().await.unwrap() {
        assert_eq!(
            other_key.endpoint(&proxy).unwrap().password.expose(),
            PROXY_PASS
        );
        // And the first key no longer opens them.
        assert!(harness.pool.endpoint(&proxy).is_err());
    }
}

#[tokio::test]
async fn a_proxy_missing_from_the_list_is_retired_only_after_three_syncs() {
    let harness = harness(4).await;
    let proxies = harness.healthy_pool().await;
    let gone = proxies[1].clone();
    let removed = harness.mock.with(|state| state.proxies.remove(1));

    for sync in 1..=2 {
        let report = harness.pool.reconcile().await.unwrap();
        assert_eq!(report.missing, [gone.id], "sync {sync}");
        assert_eq!(report.retired, []);
        assert_eq!(harness.proxy(gone.id).await.state, ProxyState::Healthy);
    }

    // It is listed again: the count starts over.
    harness.mock.with(|state| state.proxies.insert(1, removed));
    assert_eq!(harness.pool.reconcile().await.unwrap().missing, []);
    harness.mock.with(|state| {
        state.proxies.remove(1);
    });
    for _ in 1..=2 {
        assert_eq!(harness.pool.reconcile().await.unwrap().retired, []);
    }

    harness.advance(chrono::Duration::minutes(30));
    let report = harness.pool.reconcile().await.unwrap();

    assert_eq!(
        report.retired,
        [(gone.id, "missing_from_provider".to_owned())]
    );
    let retired = harness.proxy(gone.id).await;
    assert_eq!(retired.state, ProxyState::Retired);
    assert_eq!(retired.retired_at, Some(harness.now()));
    assert_eq!(harness.pool.live().await.unwrap().len(), 3);
    let events = harness.events("proxy_retired").await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].details["reason"], "missing_from_provider");
    assert_eq!(events[0].details["from"], "healthy");
    assert_eq!(events[0].subject_id, Some(gone.id.as_uuid()));
    // Nobody replaced it, so nobody is told the provider did.
    assert_eq!(harness.alerts_of(AlertKind::ProviderReplacedProxy), 0);
    // A later sync does not bring it back or retire it twice.
    assert_eq!(harness.pool.reconcile().await.unwrap().retired, []);
}

#[tokio::test]
async fn a_proxy_the_provider_replaced_by_itself_is_retired_at_once_with_an_alert() {
    let harness = harness(4).await;
    let proxies = harness.healthy_pool().await;
    let replaced = proxies[2].clone();
    harness.advance(chrono::Duration::hours(3));
    let now = harness.now();
    // A setting the operator did not accept did it.
    let new_ip = harness
        .mock
        .with(|state| state.auto_replace(&replaced.host, "auto_out_of_rotation", now));

    let report = harness.pool.reconcile().await.unwrap();

    // One sync is enough: the provider's own log confirms it.
    assert_eq!(
        report.retired,
        [(replaced.id, "auto_out_of_rotation".to_owned())]
    );
    assert_eq!(report.added.len(), 1);
    assert_eq!(harness.proxy(replaced.id).await.state, ProxyState::Retired);
    let adopted = harness.at(&new_ip).await;
    assert_eq!(adopted.state, ProxyState::New);
    assert_eq!(adopted.id, report.added[0]);
    let alerts = harness.alerts.alerts();
    let alert = alerts
        .iter()
        .find(|alert| alert.kind == AlertKind::ProviderReplacedProxy)
        .unwrap();
    assert_eq!(alert.subject, Some(replaced.id.as_uuid()));
    assert_eq!(alert.details["reason"], "auto_out_of_rotation");
    assert_eq!(alert.details["replaced_with"], new_ip);
    assert_eq!(harness.events("provider_replaced_proxy").await, []);
    // That was not a replacement Kage asked for: the budget is untouched.
    assert_eq!(harness.used_this_month().await, 0);
}

/// Decision D31 and follow-up F16: Webshare may replace a proxy its own
/// checks found dead. The swap is noted instead of alerted, and takes the
/// same path as any other: the old proxy retired, the new one adopted, both
/// published, so the persona behind the old address is retired and a new one
/// generated.
#[tokio::test]
async fn a_swap_by_an_accepted_provider_setting_is_noted_and_takes_the_same_path() {
    let harness = harness(4).await;
    let proxies = harness.healthy_pool().await;
    let replaced = proxies[1].clone();
    harness.advance(chrono::Duration::hours(3));
    let now = harness.now();
    let mut events = harness.pool.subscribe();
    let new_ip = harness
        .mock
        .with(|state| state.auto_replace(&replaced.host, "auto_invalidated", now));

    let report = harness.pool.reconcile().await.unwrap();

    assert_eq!(
        report.retired,
        [(replaced.id, "auto_invalidated".to_owned())]
    );
    assert_eq!(harness.proxy(replaced.id).await.state, ProxyState::Retired);
    let adopted = harness.at(&new_ip).await;
    assert_eq!(adopted.state, ProxyState::New);
    assert_eq!(report.added, [adopted.id]);
    // What subscribers see is what they see for a replacement Kage asked
    // for: one proxy added (the list is read first), one retired.
    let mut published = Vec::new();
    while let Ok(event) = events.try_recv() {
        published.push(event);
    }
    assert_eq!(
        published,
        [
            PoolEvent::Added { proxy: adopted.id },
            PoolEvent::StateChanged {
                proxy: replaced.id,
                from: ProxyState::Healthy,
                to: ProxyState::Retired,
            },
        ]
    );
    // An event, not an alert.
    assert_eq!(harness.alerts_of(AlertKind::ProviderReplacedProxy), 0);
    assert_eq!(harness.alerts.alerts(), []);
    let noted = harness.events("provider_replaced_proxy").await;
    assert_eq!(noted.len(), 1);
    assert_eq!(noted[0].subject_id, Some(replaced.id.as_uuid()));
    assert_eq!(noted[0].severity, AlertSeverity::Info);
    assert_eq!(noted[0].details["reason"], "auto_invalidated");
    assert_eq!(noted[0].details["replaced_with"], new_ip);
    assert_eq!(harness.used_this_month().await, 0);

    // With nothing accepted the same swap is an alert again.
    let strict = harness_with(
        PoolRules {
            accepted_auto_replace: AutoReplace::default(),
            ..PoolRules::default()
        },
        2,
    )
    .await;
    let proxies = strict.healthy_pool().await;
    strict.advance(chrono::Duration::hours(1));
    let now = strict.now();
    strict
        .mock
        .with(|state| state.auto_replace(&proxies[0].host, "auto_invalidated", now));
    strict.pool.reconcile().await.unwrap();
    assert_eq!(strict.alerts_of(AlertKind::ProviderReplacedProxy), 1);
    assert_eq!(strict.events("provider_replaced_proxy").await, []);
}

#[tokio::test]
async fn an_address_that_changes_under_the_same_id_is_one_proxy_gone_and_one_arrived() {
    let harness = harness(2).await;
    let proxies = harness.healthy_pool().await;
    let before = proxies[0].clone();
    harness.mock.with(|state| {
        state.proxies[0].ip = "198.51.100.200".to_owned();
    });

    let report = harness.pool.reconcile().await.unwrap();

    assert_eq!(report.retired, [(before.id, "address_changed".to_owned())]);
    assert_eq!(report.added.len(), 1);
    let arrived = harness.at("198.51.100.200").await;
    assert_ne!(arrived.id, before.id);
    assert_eq!(arrived.provider_ref, before.provider_ref);
    assert_eq!(arrived.state, ProxyState::New);
    assert_eq!(arrived.last_health, None);
    assert_eq!(harness.proxy(before.id).await.state, ProxyState::Retired);
}

#[tokio::test]
async fn a_list_that_is_empty_or_far_too_short_retires_nothing() {
    let harness = harness(6).await;
    let proxies = harness.healthy_pool().await;
    let saved = harness
        .mock
        .with(|state| std::mem::take(&mut state.proxies));

    for _ in 0..5 {
        let report = harness.pool.reconcile().await.unwrap();
        assert!(report.suspect);
        assert_eq!(report.missing.len(), 6);
        assert_eq!(report.retired, []);
    }
    assert_eq!(harness.pool.live().await.unwrap().len(), 6);
    // One alert for the whole episode.
    assert_eq!(harness.alerts_of(AlertKind::ProviderListSuspect), 1);

    // Two of six is fewer than half: still not believed.
    harness
        .mock
        .with(|state| state.proxies = saved[..2].to_vec());
    assert!(harness.pool.reconcile().await.unwrap().suspect);
    assert_eq!(harness.pool.live().await.unwrap().len(), 6);

    // Three of six is believed, and then the usual three syncs apply; the
    // syncs that were not believed did not count.
    harness
        .mock
        .with(|state| state.proxies = saved[..3].to_vec());
    for sync in 1..=3 {
        let report = harness.pool.reconcile().await.unwrap();
        assert!(!report.suspect);
        assert_eq!(
            report.retired.len(),
            if sync == 3 { 3 } else { 0 },
            "sync {sync}"
        );
    }
    assert_eq!(harness.pool.live().await.unwrap().len(), 3);
    assert_eq!(proxies.len(), 6);

    // The episode is over: the next one alerts again.
    harness.mock.with(|state| state.proxies.clear());
    assert!(harness.pool.reconcile().await.unwrap().suspect);
    assert_eq!(harness.alerts_of(AlertKind::ProviderListSuspect), 2);
}

#[tokio::test]
async fn a_provider_that_cannot_be_read_raises_one_alert_and_changes_nothing() {
    let harness = harness(3).await;
    harness.healthy_pool().await;
    harness.mock.inject(
        None,
        "/api/v2/proxy/list/",
        401,
        json!({ "detail": "Invalid token." }),
        2,
    );

    for _ in 0..2 {
        let error = harness.pool.reconcile().await.unwrap_err();
        assert!(matches!(
            error,
            PoolError::Provider(ProviderError::Unauthorized { status: 401 })
        ));
    }

    assert_eq!(harness.alerts_of(AlertKind::ProviderSyncFailed), 1);
    assert_eq!(harness.pool.live().await.unwrap().len(), 3);
    // It works again, and the next failure alerts again.
    harness.pool.reconcile().await.unwrap();
    harness.mock.inject(
        None,
        "/api/v2/proxy/list/",
        500,
        json!({ "detail": "boom" }),
        1,
    );
    harness.pool.reconcile().await.unwrap_err();
    assert_eq!(harness.alerts_of(AlertKind::ProviderSyncFailed), 2);
}

#[tokio::test]
async fn sustained_failure_ends_in_one_replacement_and_a_new_proxy() {
    let harness = harness(5).await;
    let proxies = harness.healthy_pool().await;
    let dying = proxies[0].clone();
    harness.pool.sync_account().await.unwrap();
    let mut events = harness.pool.subscribe();

    // The address stops answering. Probes every five minutes, backoffs in
    // between, for as long as it takes.
    let failing_since = harness.now() + chrono::Duration::minutes(5);
    let mut requested_at = None;
    for _ in 0..400 {
        let report = harness.pool.tick().await.unwrap();
        if report.requested.contains(&dying.id) {
            requested_at = Some(harness.now());
            break;
        }
        let proxy = harness.proxy(dying.id).await;
        if proxy.state == ProxyState::Backoff {
            // Nothing is replaced early, however many cycles went by.
            assert!(proxy.replacement.is_none());
            harness.advance(proxy.backoff_until.unwrap() - harness.now());
        } else {
            harness.advance(chrono::Duration::minutes(5));
            harness.fail(&proxy).await;
        }
    }
    let requested_at = requested_at.expect("no replacement was ever requested");

    // Not before six hours of failing without a break.
    let failed_for = requested_at - failing_since;
    assert!(failed_for >= chrono::Duration::hours(6), "{failed_for}");
    assert!(failed_for < chrono::Duration::hours(8), "{failed_for}");
    let replacing = harness.proxy(dying.id).await;
    assert_eq!(replacing.state, ProxyState::Replacing);
    let replacement = replacing.replacement.clone().unwrap();
    assert_eq!(replacement.reason, ReplaceReason::Unreachable);
    assert_eq!(replacement.requested_at, requested_at);
    assert_eq!(replacement.ticket.as_deref(), Some("98315"));
    assert_eq!(harness.used_this_month().await, 1);
    // The request asked for the same country.
    let posted = harness
        .mock
        .requests()
        .await
        .into_iter()
        .find(|request| request.method.as_str() == "POST")
        .unwrap();
    let body: serde_json::Value = posted.body_json().unwrap();
    assert_eq!(body["to_replace"]["ip_addresses"], json!([dying.host]));
    assert_eq!(
        body["replace_with"],
        json!([{ "type": "country", "country_code": "DE", "count": 1 }])
    );

    // While it is in flight nothing else is asked for and nothing is probed.
    harness.advance(chrono::Duration::seconds(1));
    let idle = harness.pool.tick().await.unwrap();
    assert!(idle.requested.is_empty() && idle.replaced.is_empty());

    harness.finish_replacement().await;

    let retired = harness.proxy(dying.id).await;
    assert_eq!(retired.state, ProxyState::Retired);
    assert!(retired.retired_at.is_some());
    // The new address was adopted in the same pass.
    let live = harness.pool.live().await.unwrap();
    assert_eq!(live.len(), 5);
    let adopted = live
        .iter()
        .find(|proxy| !proxies.iter().any(|old| old.id == proxy.id))
        .unwrap();
    assert_eq!(adopted.state, ProxyState::New);
    assert_eq!(adopted.country.unwrap().as_str(), "DE");
    assert_eq!(harness.used_this_month().await, 1);
    assert_eq!(harness.events("proxy_replacement_requested").await.len(), 1);
    let replaced = harness.events("proxy_replaced").await;
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].details["reason"], "unreachable");
    assert_eq!(replaced[0].details["ticket"], "98315");
    // Subscribers saw the whole way down.
    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let PoolEvent::StateChanged { proxy, from, to } = event
            && proxy == dying.id
        {
            seen.push((from, to));
        }
    }
    assert_eq!(
        seen.first(),
        Some(&(ProxyState::Healthy, ProxyState::Degraded))
    );
    assert!(seen.contains(&(ProxyState::Degraded, ProxyState::Backoff)));
    assert!(seen.contains(&(ProxyState::Backoff, ProxyState::Replacing)));
    assert_eq!(
        seen.last(),
        Some(&(ProxyState::Replacing, ProxyState::Retired))
    );
    // No alert: everything went as designed.
    assert_eq!(harness.alerts_of(AlertKind::ReplacementFailed), 0);
    assert_eq!(harness.alerts_of(AlertKind::ReplacementBudgetExhausted), 0);
}

#[tokio::test]
async fn the_budget_is_never_exceeded_and_the_longest_failing_proxy_goes_first() {
    let rules = PoolRules {
        replacements_per_month: 1,
        ..PoolRules::default()
    };
    let harness = harness_with(rules, 8).await;
    let proxies = harness.healthy_pool().await;
    let now = harness.now();
    // Three proxies have been failing for eight, nine and seven hours.
    harness
        .make_due(&proxies[0], now - chrono::Duration::hours(8))
        .await;
    harness
        .make_due(&proxies[1], now - chrono::Duration::hours(9))
        .await;
    harness
        .make_due(&proxies[2], now - chrono::Duration::hours(7))
        .await;
    // And one for five: not due yet.
    harness
        .make_due(&proxies[3], now - chrono::Duration::hours(5))
        .await;

    let report = harness.pool.tick().await.unwrap();

    // One request, for the one failing longest.
    assert_eq!(report.requested, [proxies[1].id]);
    assert_eq!(
        harness.proxy(proxies[1].id).await.state,
        ProxyState::Replacing
    );
    harness.finish_replacement().await;
    assert_eq!(
        harness.proxy(proxies[1].id).await.state,
        ProxyState::Retired
    );
    assert_eq!(harness.used_this_month().await, 1);

    // The budget of one is spent: the others stay in backoff, however often
    // the pool looks, and the operator is told once.
    for _ in 0..5 {
        harness.advance(chrono::Duration::minutes(1));
        let report = harness.pool.tick().await.unwrap();
        assert_eq!(report.requested, []);
    }
    for proxy in &proxies[..1] {
        let waiting = harness.proxy(proxy.id).await;
        assert!(matches!(
            waiting.state,
            ProxyState::Backoff | ProxyState::Degraded
        ));
        assert!(waiting.replacement.is_none());
    }
    assert_eq!(harness.used_this_month().await, 1);
    assert_eq!(harness.alerts_of(AlertKind::ReplacementBudgetExhausted), 1);
    let alert = harness
        .alerts
        .alerts()
        .into_iter()
        .find(|alert| alert.kind == AlertKind::ReplacementBudgetExhausted)
        .unwrap();
    assert_eq!(alert.details["used"], 1);
    assert_eq!(alert.details["budget"], 1);
    // An operator's request is refused by the same budget.
    assert_eq!(
        harness
            .pool
            .request_replacement(proxies[4].id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::BudgetExhausted { used: 1, budget: 1 }
    );
    assert_eq!(
        harness.proxy(proxies[4].id).await.state,
        ProxyState::Healthy
    );
    assert_eq!(
        harness
            .mock
            .with(|state| state.count(Some("POST"), "/api/v3/proxy/replace/")),
        1
    );

    // A new month, a new budget: the next longest-failing proxy goes.
    harness
        .clock
        .set(Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 5).unwrap());
    harness
        .mock
        .with(|state| state.now = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 5).unwrap());
    let now = harness.now();
    for proxy in [&proxies[0], &proxies[2]] {
        let failing_since = harness.proxy(proxy.id).await.failing_since.unwrap();
        harness.make_due(proxy, failing_since).await;
    }
    assert_eq!(harness.used_this_month().await, 0);
    let report = harness.pool.tick().await.unwrap();
    assert_eq!(report.requested, [proxies[0].id]);
    assert_eq!(harness.used_this_month().await, 1);
    assert!(now > start());
}

/// Follow-up F1: Webshare counts replacements per subscription term, so
/// once the account is read Kage's budget is counted over that term and not
/// over the calendar month.
#[tokio::test]
async fn the_budget_follows_the_providers_subscription_term() {
    let rules = PoolRules {
        replacements_per_month: 1,
        ..PoolRules::default()
    };
    let harness = harness_with(rules, 6).await;
    let proxies = harness.healthy_pool().await;
    // Before the account is read the window is the calendar month.
    let summary = harness.pool.summary().await.unwrap();
    assert_eq!(summary.budget_window, BudgetWindow::Month);
    assert_eq!(
        summary.budget_window_start,
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    );

    // The owner's plan: bought on the 9th, thirty days long.
    let term_start = Utc.with_ymd_and_hms(2026, 10, 9, 12, 20, 31).unwrap();
    let term_end = term_start + chrono::Duration::days(30);
    harness.mock.with(|state| {
        state.term = (term_start.to_rfc3339(), term_end.to_rfc3339());
    });
    let account = harness.pool.sync_account().await.unwrap();
    assert_eq!(account.term_started_at, Some(term_start));
    assert_eq!(account.term_ends_at, Some(term_end));
    let summary = harness.pool.summary().await.unwrap();
    assert_eq!(summary.budget_window, BudgetWindow::Term);
    assert_eq!(summary.budget_window_start, term_start);

    let replace = |proxy: ProxyId| {
        let pool = harness.pool.clone();
        async move {
            pool.request_replacement(proxy, ReplaceReason::Operator)
                .await
                .unwrap()
        }
    };
    assert!(matches!(
        replace(proxies[0].id).await,
        ReplacementOutcome::Requested { .. }
    ));
    harness.finish_replacement().await;
    assert_eq!(harness.used_this_month().await, 1);

    // A new calendar month inside the same term: the budget is not new.
    let november = Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 5).unwrap();
    harness.clock.set(november);
    harness.mock.with(|state| state.now = november);
    assert_eq!(harness.used_this_month().await, 1);
    assert_eq!(
        replace(proxies[1].id).await,
        ReplacementOutcome::BudgetExhausted { used: 1, budget: 1 }
    );
    let alert = harness
        .alerts
        .alerts()
        .into_iter()
        .find(|alert| alert.kind == AlertKind::ReplacementBudgetExhausted)
        .unwrap();
    assert_eq!(alert.details["window"], "term");
    assert_eq!(alert.details["window_start"], json!(term_start));

    // The term ran out and the plan renewed. Even on the account as it was
    // last read, the window is the new term.
    let renewed = term_end + chrono::Duration::minutes(1);
    harness.clock.set(renewed);
    harness.mock.with(|state| state.now = renewed);
    let summary = harness.pool.summary().await.unwrap();
    assert_eq!(summary.budget_window_start, term_end);
    assert_eq!(summary.replacements_this_month, 0);
    assert!(matches!(
        replace(proxies[1].id).await,
        ReplacementOutcome::Requested { .. }
    ));
    assert_eq!(harness.used_this_month().await, 1);
}

#[tokio::test]
async fn the_providers_quota_and_its_stock_in_the_country_are_respected() {
    let harness = harness(5).await;
    let proxies = harness.healthy_pool().await;

    // The plan has no replacements left.
    harness
        .mock
        .with(|state| state.replacements_used = state.replacements_total);
    let account = harness.pool.sync_account().await.unwrap();
    assert_eq!(account.replacements_available, Some(0));
    assert_eq!(
        harness
            .pool
            .request_replacement(proxies[0].id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::QuotaExhausted
    );
    assert_eq!(
        harness.proxy(proxies[0].id).await.state,
        ProxyState::Healthy
    );
    assert_eq!(harness.alerts_of(AlertKind::ReplacementBudgetExhausted), 1);

    // The plan has some, but none in Germany.
    harness.mock.with(|state| {
        state.replacements_used = 0;
        state.available_countries.insert("DE".to_owned(), 0);
    });
    harness.pool.sync_account().await.unwrap();
    assert_eq!(
        harness
            .pool
            .request_replacement(proxies[0].id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::CountryUnavailable {
            country: "DE".parse().unwrap()
        }
    );
    assert_eq!(
        harness.proxy(proxies[0].id).await.state,
        ProxyState::Healthy
    );
    assert_eq!(harness.alerts_of(AlertKind::ReplacementFailed), 1);
    // Neither refusal reached the provider or the budget.
    assert_eq!(
        harness
            .mock
            .with(|state| state.count(Some("POST"), "/api/v3/proxy/replace/")),
        0
    );
    assert_eq!(harness.used_this_month().await, 0);

    // The stock is back: the request goes out, and a second one for a proxy
    // that is already being replaced is told so.
    harness
        .mock
        .with(|state| state.available_countries.insert("DE".to_owned(), 3));
    harness.pool.sync_account().await.unwrap();
    assert_eq!(
        harness
            .pool
            .request_replacement(proxies[0].id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::Requested {
            ticket: Some("98315".to_owned())
        }
    );
    assert_eq!(
        harness
            .pool
            .request_replacement(proxies[0].id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::NotEligible {
            state: ProxyState::Replacing
        }
    );
    assert!(matches!(
        harness
            .pool
            .request_replacement(ProxyId::new(), ReplaceReason::Operator)
            .await,
        Err(PoolError::NotFound(_))
    ));
}

#[tokio::test]
async fn a_request_the_provider_refuses_puts_the_proxy_back_and_frees_its_slot() {
    let harness = harness(5).await;
    let proxies = harness.healthy_pool().await;

    // The provider's own quota is used up, which the pool has not read yet.
    harness
        .mock
        .with(|state| state.replacements_used = state.replacements_total);
    let outcome = harness
        .pool
        .request_replacement(proxies[0].id, ReplaceReason::Operator)
        .await
        .unwrap();

    assert_eq!(
        outcome,
        ReplacementOutcome::Failed {
            error: ProviderError::QuotaExhausted
        }
    );
    let back = harness.proxy(proxies[0].id).await;
    assert_eq!(back.state, ProxyState::Backoff);
    assert!(back.replacement.is_none());
    assert!(back.backoff_until.unwrap() > harness.now());
    assert_eq!(harness.used_this_month().await, 0);
    assert_eq!(harness.alerts_of(AlertKind::ReplacementFailed), 1);
    assert_eq!(harness.alerts_of(AlertKind::ReplacementBudgetExhausted), 1);
}

#[tokio::test]
async fn a_replacement_the_provider_fails_frees_its_slot() {
    let harness = harness(5).await;
    let proxies = harness.healthy_pool().await;
    harness.mock.with(|state| {
        state.fail_replacement = Some((
            "no_proxies_available".to_owned(),
            "No proxies available in DE".to_owned(),
        ));
    });

    let outcome = harness
        .pool
        .request_replacement(proxies[0].id, ReplaceReason::Operator)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        ReplacementOutcome::Requested { ticket: Some(_) }
    ));
    assert_eq!(harness.used_this_month().await, 1);

    // Asked too soon, the pool does not ask the provider again.
    let reads = || {
        harness
            .mock
            .with(|state| state.count(Some("GET"), "/api/v3/proxy/replace/"))
    };
    harness.pool.tick().await.unwrap();
    let after_first = reads();
    harness.advance(chrono::Duration::seconds(5));
    harness.pool.tick().await.unwrap();
    assert_eq!(reads(), after_first);

    harness.finish_replacement().await;

    let back = harness.proxy(proxies[0].id).await;
    assert_eq!(back.state, ProxyState::Backoff);
    assert!(back.replacement.is_none());
    assert_eq!(harness.used_this_month().await, 0);
    assert_eq!(harness.pool.live().await.unwrap().len(), 5);
    let alert = harness
        .alerts
        .alerts()
        .into_iter()
        .find(|alert| alert.kind == AlertKind::ReplacementFailed)
        .unwrap();
    assert_eq!(alert.subject, Some(proxies[0].id.as_uuid()));
    assert!(
        alert.details["error"]
            .as_str()
            .unwrap()
            .contains("no_proxies_available: No proxies available in DE")
    );
}

#[tokio::test]
async fn a_request_with_an_unknown_outcome_is_followed_through_the_providers_log() {
    let harness = harness(6).await;
    let proxies = harness.healthy_pool().await;

    // The provider's server fails while the request is on its way.
    harness.mock.inject(
        Some("POST"),
        "/api/v3/proxy/replace/",
        502,
        json!({ "detail": "bad gateway" }),
        2,
    );
    let lost = proxies[0].clone();
    let applied = proxies[1].clone();
    assert_eq!(
        harness
            .pool
            .request_replacement(lost.id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::Requested { ticket: None }
    );
    // It keeps its place in the budget while nobody knows.
    assert_eq!(harness.proxy(lost.id).await.state, ProxyState::Replacing);
    assert_eq!(harness.used_this_month().await, 1);
    // The request is never sent a second time.
    assert_eq!(
        harness
            .mock
            .with(|state| state.count(Some("POST"), "/api/v3/proxy/replace/")),
        1
    );

    // Nothing in the provider's log, and the grace period is not over.
    harness.advance(chrono::Duration::minutes(2));
    let report = harness.pool.tick().await.unwrap();
    assert!(report.failed.is_empty() && report.replaced.is_empty());
    assert_eq!(harness.proxy(lost.id).await.state, ProxyState::Replacing);

    // Five minutes without a trace: it did not happen.
    harness.advance(chrono::Duration::minutes(4));
    let report = harness.pool.tick().await.unwrap();
    assert_eq!(report.failed, [lost.id]);
    let back = harness.proxy(lost.id).await;
    assert_eq!(back.state, ProxyState::Backoff);
    assert_eq!(harness.used_this_month().await, 0);
    assert_eq!(harness.alerts_of(AlertKind::ReplacementFailed), 1);

    // The second time the provider did apply it, though it answered 502.
    assert_eq!(
        harness
            .pool
            .request_replacement(applied.id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::Requested { ticket: None }
    );
    harness.advance(chrono::Duration::seconds(30));
    let now = harness.now();
    harness
        .mock
        .with(|state| state.auto_replace(&applied.host, "proxy_replaced", now));
    let report = harness.pool.tick().await.unwrap();
    assert_eq!(report.replaced, [applied.id]);
    assert_eq!(harness.proxy(applied.id).await.state, ProxyState::Retired);
    assert_eq!(harness.used_this_month().await, 1);
    // The new address is in the pool.
    assert_eq!(harness.pool.live().await.unwrap().len(), 6);
}

#[tokio::test]
async fn the_account_is_reported_and_never_changed() {
    let harness = harness(4).await;
    harness.healthy_pool().await;
    assert_eq!(harness.pool.account(), None);

    let account = harness.pool.sync_account().await.unwrap();

    assert_eq!(account.proxy_count, Some(4));
    assert_eq!(harness.pool.account(), Some(account));
    assert_eq!(harness.events("provider_account").await.len(), 1);
    assert_eq!(harness.alerts.alerts(), []);
    // Read again unchanged: no new event.
    harness.pool.sync_account().await.unwrap();
    assert_eq!(harness.events("provider_account").await.len(), 1);

    // Webshare is set to replace proxies by itself, and the subscription is
    // paused.
    harness.mock.with(|state| {
        state.auto_replace = [true, true, false, false];
        state.paused = true;
    });
    for _ in 0..3 {
        harness.pool.sync_account().await.unwrap();
    }

    assert_eq!(harness.events("provider_account").await.len(), 2);
    // One alert each, not one per sync.
    assert_eq!(harness.alerts_of(AlertKind::ProviderAutoReplaceEnabled), 1);
    assert_eq!(harness.alerts_of(AlertKind::ProviderAccount), 1);
    let alerts = harness.alerts.alerts();
    let auto = alerts
        .iter()
        .find(|alert| alert.kind == AlertKind::ProviderAutoReplaceEnabled)
        .unwrap();
    // The alert names the setting nobody accepted; the accepted one
    // (decision D31) is noted once as an event.
    assert_eq!(
        auto.details["enabled"],
        json!(["auto_replace_low_country_confidence_proxies"])
    );
    assert_eq!(
        auto.details["accepted"],
        json!(["auto_replace_invalid_proxies"])
    );
    let noted = harness.events("provider_auto_replace_accepted").await;
    assert_eq!(noted.len(), 1);
    assert_eq!(noted[0].severity, AlertSeverity::Info);
    assert_eq!(
        noted[0].details["enabled"],
        json!(["auto_replace_invalid_proxies"])
    );
    let paused = alerts
        .iter()
        .find(|alert| alert.kind == AlertKind::ProviderAccount)
        .unwrap();
    assert_eq!(
        paused.details["problems"],
        json!(["the subscription is paused"])
    );

    // The owner switches them off: the next time they are on alerts again.
    harness.mock.with(|state| state.auto_replace = [false; 4]);
    harness.pool.sync_account().await.unwrap();
    harness
        .mock
        .with(|state| state.auto_replace = [false, false, false, true]);
    harness.pool.sync_account().await.unwrap();
    assert_eq!(harness.alerts_of(AlertKind::ProviderAutoReplaceEnabled), 2);

    // Kage read, and only read: no setting was ever written.
    assert!(
        harness
            .mock
            .requests()
            .await
            .iter()
            .all(|request| request.method.as_str() == "GET")
    );
}

#[tokio::test]
async fn a_leak_makes_egress_unsafe_until_a_clean_probe_or_an_operator_clears_it() {
    let harness = harness(4).await;
    let proxies = harness.healthy_pool().await;
    let mut events = harness.pool.subscribe();
    assert!(!harness.pool.egress_unsafe().await.unwrap());
    assert!(!harness.pool.summary().await.unwrap().egress_unsafe);

    let leaking = harness.leak(&proxies[0]).await;

    assert_eq!(leaking.state, ProxyState::Backoff);
    assert!(harness.pool.egress_unsafe().await.unwrap());
    assert!(harness.pool.summary().await.unwrap().egress_unsafe);
    assert_eq!(harness.alerts_of(AlertKind::ProxyLeak), 1);
    assert_eq!(harness.alerts_of(AlertKind::EgressUnsafe), 1);
    let alerts = harness.alerts.alerts();
    assert!(
        alerts
            .iter()
            .all(|alert| alert.severity == AlertSeverity::Critical)
    );
    assert_eq!(alerts[0].subject, Some(leaking.id.as_uuid()));
    assert_eq!(alerts[0].details["exit_ip"], "192.0.2.77");
    let mut flagged = false;
    while let Ok(event) = events.try_recv() {
        flagged |= event == PoolEvent::EgressUnsafe(true);
    }
    assert!(flagged);

    // A second proxy leaks: no second pool-wide alert.
    harness.leak(&proxies[1]).await;
    assert_eq!(harness.alerts_of(AlertKind::ProxyLeak), 2);
    assert_eq!(harness.alerts_of(AlertKind::EgressUnsafe), 1);

    // The first one shows a distinct exit address again. One leak is still
    // on record, so the flag stays.
    harness.clock.set(leaking.backoff_until.unwrap());
    harness.pool.tick().await.unwrap();
    let recovered = harness.pass(&harness.proxy(leaking.id).await).await;
    assert_eq!(recovered.state, ProxyState::Healthy);
    assert!(harness.pool.egress_unsafe().await.unwrap());
    assert_eq!(harness.events("egress_safe").await, []);

    // The second proxy disappears from the provider's list. That does not
    // clear its leak: nobody saw it stop.
    harness.mock.with(|state| {
        state.proxies.remove(1);
    });
    for _ in 0..3 {
        harness.pool.reconcile().await.unwrap();
    }
    assert_eq!(
        harness.proxy(proxies[1].id).await.state,
        ProxyState::Retired
    );
    assert!(harness.pool.egress_unsafe().await.unwrap());

    // The operator clears it.
    let cleared = harness.pool.clear_egress_unsafe().await.unwrap();
    assert_eq!(cleared, [proxies[1].id]);
    assert!(!harness.pool.egress_unsafe().await.unwrap());
    let safe = harness.events("egress_safe").await;
    assert_eq!(safe.len(), 1);
    assert_eq!(safe[0].details["cleared_by"], "operator");
    let mut cleared_event = false;
    while let Ok(event) = events.try_recv() {
        cleared_event |= event == PoolEvent::EgressUnsafe(false);
    }
    assert!(cleared_event);
    // Nothing left to clear.
    assert_eq!(harness.pool.clear_egress_unsafe().await.unwrap(), []);

    // The next leak is a new episode with a new alert.
    harness.leak(&harness.proxy(proxies[2].id).await).await;
    assert_eq!(harness.alerts_of(AlertKind::EgressUnsafe), 2);
    // A clean probe through the same proxy clears the flag by itself.
    let parked = harness.proxy(proxies[2].id).await;
    harness.clock.set(parked.backoff_until.unwrap());
    harness.pool.tick().await.unwrap();
    harness.pass(&harness.proxy(parked.id).await).await;
    assert!(!harness.pool.egress_unsafe().await.unwrap());
    assert_eq!(
        harness.events("egress_safe").await[0].details["cleared_by"],
        "probe"
    );
}

#[tokio::test]
async fn more_than_half_of_the_pool_failing_at_once_freezes_the_counters() {
    let harness = harness(6).await;
    let proxies = harness.healthy_pool().await;

    // Three of six fail: that is not more than half, each one counts.
    for proxy in &proxies[..3] {
        let failed = harness.fail(proxy).await;
        assert_eq!(failed.state, ProxyState::Degraded);
        assert_eq!(failed.consecutive_failures, 1);
    }
    assert_eq!(harness.alerts_of(AlertKind::PoolOutageSuspected), 0);

    // The fourth makes it more than half: from here on nothing counts.
    let fourth = harness.fail(&proxies[3]).await;
    assert_eq!(fourth.state, ProxyState::Healthy);
    assert_eq!(fourth.consecutive_failures, 0);
    assert_eq!(harness.alerts_of(AlertKind::PoolOutageSuspected), 1);
    for round in 0..4 {
        for proxy in &proxies {
            let failed = harness.fail(proxy).await;
            assert!(
                failed.consecutive_failures <= 1,
                "round {round}: {} failures",
                failed.consecutive_failures
            );
            assert_ne!(failed.state, ProxyState::Backoff);
        }
        harness.advance(chrono::Duration::minutes(5));
    }
    assert_eq!(harness.alerts_of(AlertKind::PoolOutageSuspected), 1);
    // The failures are on record all the same.
    for proxy in harness.pool.live().await.unwrap() {
        assert!(!proxy.last_health.unwrap().reachable);
        assert!(proxy.state.is_usable());
    }

    // The path is back: proxies answer one after the other, and once fewer
    // than half are failing the episode is over.
    for proxy in &proxies[..4] {
        assert_eq!(harness.pass(proxy).await.state, ProxyState::Healthy);
    }
    assert_eq!(harness.events("pool_outage_cleared").await.len(), 1);
    // Failures count again.
    let mut last = harness.fail(&proxies[4]).await;
    for _ in 0..2 {
        last = harness.fail(&proxies[4]).await;
    }
    assert_eq!(last.state, ProxyState::Backoff);
}

#[tokio::test]
async fn in_a_pool_of_three_every_failure_counts() {
    let harness = harness(3).await;
    let proxies = harness.healthy_pool().await;

    for proxy in &proxies {
        for _ in 0..3 {
            harness.fail(proxy).await;
        }
        assert_eq!(harness.proxy(proxy.id).await.state, ProxyState::Backoff);
    }

    assert_eq!(harness.alerts_of(AlertKind::PoolOutageSuspected), 0);
    // Too small a pool for a share to mean anything.
    harness.pool.tick().await.unwrap();
    assert_eq!(harness.alerts_of(AlertKind::PoolCapacityLow), 0);
}

#[tokio::test]
async fn a_quarter_of_the_pool_unusable_raises_one_alert_until_it_recovers() {
    let harness = harness(8).await;
    let proxies = harness.healthy_pool().await;

    // One of eight in backoff: an eighth.
    for _ in 0..3 {
        harness.fail(&proxies[0]).await;
    }
    harness.pool.tick().await.unwrap();
    assert_eq!(harness.alerts_of(AlertKind::PoolCapacityLow), 0);

    // Two of eight: a quarter.
    for _ in 0..3 {
        harness.fail(&proxies[1]).await;
    }
    for _ in 0..3 {
        harness.pool.tick().await.unwrap();
    }
    assert_eq!(harness.alerts_of(AlertKind::PoolCapacityLow), 1);
    let summary = harness.pool.summary().await.unwrap();
    assert_eq!(summary.live, 8);
    assert_eq!(summary.usable, 6);
    assert_eq!(summary.by_state[&ProxyState::Backoff], 2);
    assert_eq!(summary.by_state[&ProxyState::Healthy], 6);
    assert_eq!(summary.by_state[&ProxyState::Retired], 0);
    assert_eq!(summary.replacements_per_month, 5);

    // The backoffs run out, the probes pass, and the next time it alerts
    // again.
    harness.advance(chrono::Duration::minutes(2));
    let report = harness.pool.tick().await.unwrap();
    assert_eq!(report.expired.len(), 2);
    for proxy in &proxies[..2] {
        assert_eq!(harness.proxy(proxy.id).await.state, ProxyState::Degraded);
        harness.pass(&harness.proxy(proxy.id).await).await;
    }
    harness.pool.tick().await.unwrap();
    for proxy in &proxies[..2] {
        for _ in 0..3 {
            harness.fail(&harness.proxy(proxy.id).await).await;
        }
    }
    harness.pool.tick().await.unwrap();
    assert_eq!(harness.alerts_of(AlertKind::PoolCapacityLow), 2);
}

// One proxy through every finding, each step building on the one before.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn geo_and_reputation_findings_are_stored_and_alerted() {
    let harness = harness(4).await;
    let proxies = harness.healthy_pool().await;
    let proxy = proxies[0].clone();
    let geo = |country: &str, timezone: &str, at: DateTime<Utc>| GeoRecord {
        country: country.parse().unwrap(),
        city: Some("Vienna".to_owned()),
        region: None,
        asn: Some("AS8447".to_owned()),
        org: Some("A1 Telekom Austria AG".to_owned()),
        timezone: Some(timezone.to_owned()),
        loc: None,
        ip: Harness::exit(&proxy),
        checked_at: at,
    };
    let probe = |record: GeoRecord| ProbeReport {
        at: record.checked_at,
        exit: Ok(ExitObservation {
            ip: record.ip,
            latency_ms: 90,
            connect_ms: None,
            tls_ms: None,
        }),
        leak: Some(false),
        geo: Some(record),
    };
    let mut events = harness.pool.subscribe();

    // The provider says Germany, the lookup says Austria, twice.
    harness
        .pool
        .record_probe(proxy.id, &probe(geo("AT", "Europe/Vienna", harness.now())))
        .await
        .unwrap();
    harness.advance(chrono::Duration::days(1));
    let degraded = harness
        .pool
        .record_probe(proxy.id, &probe(geo("AT", "Europe/Vienna", harness.now())))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(degraded.state, ProxyState::Degraded);
    assert_eq!(degraded.asn.as_deref(), Some("AS8447"));
    // The provider's country is kept; the observed one is what counts.
    assert_eq!(degraded.country.unwrap().as_str(), "DE");
    assert_eq!(degraded.effective_country().unwrap().as_str(), "AT");
    assert_eq!(
        degraded.observed_geo().unwrap().timezone.as_deref(),
        Some("Europe/Vienna")
    );
    assert_eq!(harness.alerts_of(AlertKind::ProxyGeoMismatch), 1);
    assert_eq!(harness.events("proxy_geo_observed").await.len(), 1);
    assert_eq!(
        events.recv().await.unwrap(),
        PoolEvent::GeoChanged { proxy: proxy.id }
    );

    // The record changes: an event, because a persona's clock hangs on it.
    harness.advance(chrono::Duration::days(1));
    harness
        .pool
        .record_probe(proxy.id, &probe(geo("AT", "Europe/Berlin", harness.now())))
        .await
        .unwrap();
    let changed = harness.events("proxy_geo_changed").await;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].details["previous"]["timezone"], "Europe/Vienna");
    assert_eq!(changed[0].details["current"]["timezone"], "Europe/Berlin");

    // The lookup agrees with the provider again.
    harness.advance(chrono::Duration::days(1));
    let cleared = harness
        .pool
        .record_probe(proxy.id, &probe(geo("DE", "Europe/Berlin", harness.now())))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(cleared.state, ProxyState::Healthy);
    assert_eq!(harness.events("proxy_geo_mismatch_cleared").await.len(), 1);

    // Reputation: hosting space is reported at once, a bad score after two
    // lookups in a row, and then the proxy is replaced within the budget.
    let lookup = |score, usage: &str| ReputationReport {
        at: harness.now(),
        score,
        usage_type: Some(usage.to_owned()),
    };
    harness
        .pool
        .record_reputation(proxy.id, &lookup(80, "Data Center/Web Hosting/Transit"))
        .await
        .unwrap();
    assert_eq!(
        harness.alerts_of(AlertKind::ReputationUsageTypeSuspicious),
        1
    );
    assert_eq!(harness.alerts_of(AlertKind::ProxyReputationBad), 0);
    assert_eq!(harness.pool.tick().await.unwrap().requested, []);

    let bad = harness
        .pool
        .record_reputation(proxy.id, &lookup(90, "Data Center/Web Hosting/Transit"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bad.state, ProxyState::Degraded);
    assert_eq!(bad.last_health.as_ref().unwrap().reputation_score, Some(90));
    assert_eq!(harness.alerts_of(AlertKind::ProxyReputationBad), 1);
    assert_eq!(
        harness.alerts_of(AlertKind::ReputationUsageTypeSuspicious),
        1
    );
    let report = harness.pool.tick().await.unwrap();
    assert_eq!(report.requested, [proxy.id]);
    assert_eq!(
        harness.proxy(proxy.id).await.replacement.unwrap().reason,
        ReplaceReason::Reputation
    );
    // A lookup for a proxy that does not exist is nothing.
    assert!(
        harness
            .pool
            .record_reputation(ProxyId::new(), &lookup(1, "Fixed Line ISP"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_bad_reputation_without_budget_leaves_the_proxy_usable() {
    let rules = PoolRules {
        replacements_per_month: 0,
        ..PoolRules::default()
    };
    let harness = harness_with(rules, 4).await;
    let proxies = harness.healthy_pool().await;
    let lookup = ReputationReport {
        at: harness.now(),
        score: 100,
        usage_type: None,
    };

    for _ in 0..2 {
        harness
            .pool
            .record_reputation(proxies[0].id, &lookup)
            .await
            .unwrap();
    }
    for _ in 0..3 {
        assert_eq!(harness.pool.tick().await.unwrap().requested, []);
    }

    let proxy = harness.proxy(proxies[0].id).await;
    assert_eq!(proxy.state, ProxyState::Degraded);
    assert!(proxy.state.is_usable());
    assert_eq!(harness.alerts_of(AlertKind::ReplacementBudgetExhausted), 1);
    // A probe that passes keeps it degraded while the reputation stands.
    assert_eq!(harness.pass(&proxy).await.state, ProxyState::Degraded);
}

#[tokio::test]
async fn real_traffic_is_remembered_for_the_health_checker() {
    let harness = harness(2).await;
    let proxies = harness.healthy_pool().await;
    assert_eq!(harness.pool.last_traffic(proxies[0].id), None);

    harness.advance(chrono::Duration::minutes(3));
    harness.pool.report_traffic_ok(proxies[0].id);

    assert_eq!(
        harness.pool.last_traffic(proxies[0].id),
        Some(harness.now())
    );
    assert_eq!(harness.pool.last_traffic(proxies[1].id), None);
    assert_eq!(harness.pool.seed(), &Seed::from_u64(7));
    assert_eq!(harness.pool.rules(), &PoolRules::default());
    assert!(format!("{:?}", harness.pool).contains("Pool"));
}

#[tokio::test]
async fn a_probe_for_a_proxy_that_is_gone_is_nothing() {
    let harness = harness(1).await;
    harness.pool.reconcile().await.unwrap();
    let report = ProbeReport {
        at: harness.now(),
        exit: Err(HealthFailure::Unreachable),
        leak: None,
        geo: None,
    };

    assert!(
        harness
            .pool
            .record_probe(ProxyId::new(), &report)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        harness
            .pool
            .end_backoff(ProxyId::new())
            .await
            .unwrap()
            .is_none()
    );
    assert!(harness.pool.get(ProxyId::new()).await.unwrap().is_none());
    assert_eq!(harness.pool.proxies().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_static_list_is_synced_but_never_replaced() {
    let db = TestDb::migrated().await;
    let clock = Arc::new(ManualClock::new(start()));
    let alerts = Arc::new(MemoryAlertSink::new());
    let list = |lines: &str| -> Arc<dyn ProxyProvider> {
        Arc::new(StaticListProvider::new(
            StaticProxy::parse_list(lines).unwrap(),
        ))
    };
    let pool = |provider| {
        Pool::new(PoolParts {
            store: db.store.clone(),
            provider: Some(provider),
            cipher: CredentialCipher::new(&MASTER_KEY_BYTES),
            rules: PoolRules::default(),
            missing_syncs: 2,
            alerts: alerts.clone(),
            clock: clock.clone(),
            seed: Seed::from_u64(1),
        })
    };
    let two = pool(list(
        "198.51.100.10:8080:user-a:pass-a:DE\n198.51.100.11:8080:user-a:pass-a:AT\n",
    ));

    let report = two.reconcile().await.unwrap();

    assert_eq!(report.added.len(), 2);
    assert_eq!(two.provider_kind(), Some(ProxyProviderKind::Static));
    let proxies = two.live().await.unwrap();
    assert!(
        proxies
            .iter()
            .all(|proxy| proxy.provider == ProxyProviderKind::Static)
    );
    assert_eq!(proxies[0].provider_ref, "198.51.100.10:8080");
    assert_eq!(
        two.endpoint(&proxies[0]).unwrap().password.expose(),
        "pass-a"
    );

    // A static list cannot replace, whoever asks.
    assert_eq!(
        two.request_replacement(proxies[0].id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::Unsupported
    );
    db.store
        .proxies()
        .modify(proxies[0].id, |stored| {
            stored.state = ProxyState::Backoff;
            stored.backoff_count = 9;
            stored.backoff_until = Some(start() + chrono::Duration::hours(1));
            stored.failing_since = Some(start() - chrono::Duration::days(2));
            stored.last_health = Some(ProxyHealth {
                failure: Some(HealthFailure::Unreachable),
                ..ProxyHealth::new(start(), false)
            });
        })
        .await
        .unwrap();
    let report = two.tick().await.unwrap();
    assert_eq!(report.requested, []);
    assert_eq!(
        two.get(proxies[0].id).await.unwrap().unwrap().state,
        ProxyState::Backoff
    );
    // Its account has nothing to alert on.
    let account = two.sync_account().await.unwrap();
    assert_eq!(account.proxy_count, Some(2));
    assert_eq!(alerts.alerts(), []);

    // The operator removes a line: after two syncs the proxy is retired.
    let one = pool(list("198.51.100.11:8080:user-a:pass-a:AT\n"));
    assert_eq!(one.reconcile().await.unwrap().retired, []);
    let report = one.reconcile().await.unwrap();
    assert_eq!(
        report.retired,
        [(proxies[0].id, "missing_from_provider".to_owned())]
    );
}

#[tokio::test]
async fn without_a_provider_the_pool_only_keeps_what_it_has() {
    let db = TestDb::migrated().await;
    let alerts = Arc::new(MemoryAlertSink::new());
    let env = |extra: &[(&str, &str)]| {
        let mut pairs = vec![
            ("KAGE__DATABASE__URL", db.url.as_str()),
            ("KAGE__SIDECAR__TOKEN", "token"),
            ("KAGE__SECRETS__MASTER_KEY", MASTER_KEY),
        ];
        pairs.extend_from_slice(extra);
        pairs
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect::<Vec<_>>()
    };

    // Nothing configured: no provider.
    let config = Config::load(None, env(&[])).unwrap();
    let pool = Pool::from_config(&config, db.store.clone(), alerts.clone()).unwrap();
    assert_eq!(pool.provider_kind(), None);
    assert_eq!(
        pool.reconcile().await.unwrap(),
        kage_proxy::ReconcileReport::default()
    );
    assert!(matches!(
        pool.sync_account().await,
        Err(PoolError::NoProvider)
    ));
    assert!(matches!(
        pool.request_replacement(ProxyId::new(), ReplaceReason::Operator)
            .await,
        Err(PoolError::NoProvider)
    ));
    assert_eq!(
        pool.tick().await.unwrap(),
        kage_proxy::TickReport::default()
    );
    let summary = pool.summary().await.unwrap();
    assert_eq!((summary.live, summary.usable), (0, 0));
    assert!(
        PoolError::NoProvider
            .to_string()
            .contains("KAGE__WEBSHARE__API_KEY")
    );

    // A static list in the configuration.
    let config = Config::load(
        None,
        env(&[(
            "KAGE__PROXIES__STATIC_LIST",
            "198.51.100.10:8080:user-a:pass-a:DE",
        )]),
    )
    .unwrap();
    let pool = Pool::from_config(&config, db.store.clone(), alerts.clone()).unwrap();
    assert_eq!(pool.provider_kind(), Some(ProxyProviderKind::Static));
    assert_eq!(pool.reconcile().await.unwrap().added.len(), 1);
    assert_eq!(pool.rules(), &PoolRules::from_config(&config));

    // A Webshare key in the configuration.
    let config = Config::load(None, env(&[("KAGE__WEBSHARE__API_KEY", "ws-key-51aa")])).unwrap();
    let pool = Pool::from_config(&config, db.store.clone(), alerts).unwrap();
    assert_eq!(pool.provider_kind(), Some(ProxyProviderKind::Webshare));
}

/// Waits until `condition` holds, for at most twenty seconds.
async fn eventually(what: &str, mut condition: impl AsyncFnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !condition().await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting until {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_upkeep_loop_syncs_reads_the_account_and_keeps_going_until_stopped() {
    let harness = harness(3).await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(Arc::clone(&harness.pool).run(
        Duration::from_millis(100),
        Duration::from_millis(20),
        async {
            let _ = stopped.await;
        },
    ));

    // At the start: the list and the account, without anyone asking.
    eventually("the list is adopted", async || {
        harness.pool.live().await.unwrap().len() == 3
    })
    .await;
    eventually("the account is read", async || {
        harness.pool.account().is_some()
    })
    .await;

    // The provider fails for a while: the loop goes on, and says so once.
    harness.mock.inject(
        None,
        "/api/v2/proxy/list/",
        500,
        json!({ "detail": "boom" }),
        3,
    );
    eventually("the failed sync is reported", async || {
        harness.alerts_of(AlertKind::ProviderSyncFailed) == 1
    })
    .await;

    // A proxy is added at the provider: adopted at a later sync.
    harness.mock.with(|state| {
        state.add_proxy("DE");
    });
    eventually("the new proxy is adopted", async || {
        harness.pool.live().await.unwrap().len() == 4
    })
    .await;
    assert_eq!(harness.alerts_of(AlertKind::ProviderSyncFailed), 1);

    // A backoff runs out: the maintenance pass ends it.
    let proxy = harness.pool.live().await.unwrap().remove(0);
    for _ in 0..3 {
        harness.fail(&proxy).await;
    }
    assert_eq!(harness.proxy(proxy.id).await.state, ProxyState::Backoff);
    harness.advance(chrono::Duration::minutes(2));
    eventually("the backoff is ended", async || {
        harness.proxy(proxy.id).await.state == ProxyState::Degraded
    })
    .await;

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the loop did not stop")
        .unwrap();
}
