//! IP reputation: the AbuseIPDB client against a mock of its `check`
//! endpoint, and the lookups the health checker schedules with it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use common::{MASTER_KEY, MASTER_KEY_BYTES, TestDb};
use kage_core::alert::{AlertKind, MemoryAlertSink};
use kage_core::config::{Config, ReputationConfig, StaticProxy};
use kage_core::domain::ProxyState;
use kage_core::secret::Secret;
use kage_core::seed::Seed;
use kage_proxy::health::{
    AbuseIpDb, GeoPolicy, HostIp, ProbeOutcome, Prober, Reputation, ReputationError,
    ReputationProvider,
};
use kage_proxy::machine::ExitObservation;
use kage_proxy::{
    CredentialCipher, HealthChecker, HealthSettings, HealthSetupError, ManualClock, Pool,
    PoolParts, PoolRules, ProxyEndpoint, StaticListProvider,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const KEY: &str = "abuse-key-93bd";

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
}

/// What the mock says about addresses, and what it was asked.
#[derive(Default)]
struct AbuseState {
    /// Score and usage type per address; anything else scores 0.
    known: HashMap<String, (i64, &'static str)>,
    /// When set, every request gets this status, with this `Retry-After`.
    failing: Option<(u16, Option<u64>)>,
    asked: Vec<String>,
}

struct AbuseResponder(Arc<Mutex<AbuseState>>);

impl Respond for AbuseResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut state = self.0.lock().unwrap();
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let ip = query.get("ipAddress").cloned().unwrap_or_default();
        state.asked.push(ip.clone());
        if request.headers.get("key").and_then(|key| key.to_str().ok()) != Some(KEY) {
            return ResponseTemplate::new(401).set_body_json(json!({
                "errors": [{ "detail": "Authentication failed.", "status": 401 }]
            }));
        }
        if let Some((status, retry_after)) = state.failing {
            let mut response = ResponseTemplate::new(status).set_body_json(json!({
                "errors": [{ "detail": "Daily rate limit of 1000 requests exceeded for this endpoint.", "status": status }]
            }));
            if let Some(seconds) = retry_after {
                response = response.insert_header("Retry-After", seconds.to_string().as_str());
            }
            return response;
        }
        if query.get("maxAgeInDays").map(String::as_str) != Some("90") {
            return ResponseTemplate::new(422).set_body_json(json!({
                "errors": [{ "detail": "maxAgeInDays must be 90 in this mock", "status": 422 }]
            }));
        }
        let (score, usage) = state
            .known
            .get(&ip)
            .copied()
            .unwrap_or((0, "Fixed Line ISP"));
        ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "ipAddress": ip,
                "isPublic": true,
                "ipVersion": 4,
                "isWhitelisted": false,
                "abuseConfidenceScore": score,
                "countryCode": "DE",
                "usageType": usage,
                "isp": "Example Telecom",
                "domain": "example.net",
                "hostnames": [],
                "isTor": false,
                "totalReports": score / 10,
                "numDistinctUsers": 1,
                "lastReportedAt": "2026-10-01T10:00:00+00:00"
            }
        }))
    }
}

struct MockAbuse {
    server: MockServer,
    state: Arc<Mutex<AbuseState>>,
}

impl MockAbuse {
    async fn start() -> Self {
        let server = MockServer::start().await;
        let state = Arc::new(Mutex::new(AbuseState::default()));
        Mock::given(method("GET"))
            .and(path("/api/v2/check"))
            .respond_with(AbuseResponder(Arc::clone(&state)))
            .mount(&server)
            .await;
        Self { server, state }
    }

    fn config(&self, key: &str) -> ReputationConfig {
        ReputationConfig {
            abuseipdb_key: Some(Secret::new(key)),
            base_url: format!("{}/api/v2/", self.server.uri()),
            ..ReputationConfig::default()
        }
    }

    fn client(&self) -> AbuseIpDb {
        AbuseIpDb::from_config(&self.config(KEY)).unwrap().unwrap()
    }

    fn set(&self, ip: &str, score: i64, usage: &'static str) {
        self.state
            .lock()
            .unwrap()
            .known
            .insert(ip.to_owned(), (score, usage));
    }

    fn fail(&self, failing: Option<(u16, Option<u64>)>) {
        self.state.lock().unwrap().failing = failing;
    }

    fn asked(&self) -> Vec<String> {
        self.state.lock().unwrap().asked.clone()
    }
}

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

#[tokio::test]
async fn a_lookup_returns_the_score_and_the_usage_type() {
    let mock = MockAbuse::start().await;
    mock.set("203.0.113.20", 87, "Data Center/Web Hosting/Transit");
    mock.set("203.0.113.21", 250, " ");
    let client = mock.client();

    assert_eq!(
        client.lookup(ip("203.0.113.20")).await.unwrap(),
        Reputation {
            score: 87,
            usage_type: Some("Data Center/Web Hosting/Transit".to_owned()),
            reports: Some(8),
        }
    );
    // An address nobody reported.
    assert_eq!(
        client.lookup(ip("203.0.113.99")).await.unwrap(),
        Reputation {
            score: 0,
            usage_type: Some("Fixed Line ISP".to_owned()),
            reports: Some(0),
        }
    );
    // A score outside the documented range is brought into it, and a blank
    // usage type is no usage type.
    let odd = client.lookup(ip("203.0.113.21")).await.unwrap();
    assert_eq!(odd.score, 100);
    assert_eq!(odd.usage_type, None);

    // The request is the documented one.
    let requests = mock.server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].url.path(), "/api/v2/check");
    assert_eq!(
        requests[0].url.query(),
        Some("ipAddress=203.0.113.20&maxAgeInDays=90")
    );
    assert_eq!(requests[0].headers["key"], KEY);
    assert_eq!(requests[0].headers["accept"], "application/json");
    assert_eq!(
        requests[0].headers["user-agent"],
        format!("kage/{}", env!("CARGO_PKG_VERSION"))
    );
}

#[tokio::test]
async fn failures_are_told_apart() {
    let mock = MockAbuse::start().await;
    let client = mock.client();
    let lookup = || client.lookup(ip("203.0.113.20"));

    mock.fail(Some((429, Some(3600))));
    assert_eq!(
        lookup().await.unwrap_err(),
        ReputationError::RateLimited {
            retry_after: Some(Duration::from_hours(1))
        }
    );
    mock.fail(Some((429, None)));
    assert_eq!(
        lookup().await.unwrap_err(),
        ReputationError::RateLimited { retry_after: None }
    );
    mock.fail(Some((503, None)));
    assert_eq!(lookup().await.unwrap_err(), ReputationError::Status(503));
    mock.fail(None);

    // A wrong key, which the error never repeats.
    let wrong = AbuseIpDb::from_config(&mock.config("wrong-key-11aa"))
        .unwrap()
        .unwrap();
    let error = wrong.lookup(ip("203.0.113.20")).await.unwrap_err();
    assert_eq!(error, ReputationError::Unauthorized(401));
    assert!(!error.to_string().contains("wrong-key-11aa"));

    // An answer that is not the documented one.
    let other = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "isp": "x" } })))
        .mount(&other)
        .await;
    let strange = AbuseIpDb::from_config(&ReputationConfig {
        abuseipdb_key: Some(Secret::new(KEY)),
        base_url: format!("{}/api/v2/", other.uri()),
        ..ReputationConfig::default()
    })
    .unwrap()
    .unwrap();
    assert!(matches!(
        strange.lookup(ip("203.0.113.20")).await.unwrap_err(),
        ReputationError::Decode(_)
    ));

    // Nothing listens.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let unreachable = AbuseIpDb::from_config(&ReputationConfig {
        abuseipdb_key: Some(Secret::new(KEY)),
        base_url: format!("http://127.0.0.1:{port}/api/v2/"),
        ..ReputationConfig::default()
    })
    .unwrap()
    .unwrap();
    assert!(matches!(
        unreachable.lookup(ip("203.0.113.20")).await.unwrap_err(),
        ReputationError::Transport(_)
    ));
}

/// A prober that never touches the network: every proxy answers, and its
/// exit address is its own address.
#[derive(Debug)]
struct EveryProxyAnswers;

#[async_trait]
impl Prober for EveryProxyAnswers {
    async fn probe(&self, target: &ProxyEndpoint, _geo: GeoPolicy) -> ProbeOutcome {
        ProbeOutcome {
            exit: Ok(ExitObservation {
                ip: target.host.parse().unwrap(),
                latency_ms: 40,
                connect_ms: None,
                tls_ms: None,
            }),
            geo_attempted: false,
            geo: None,
        }
    }
}

struct Rig {
    // Keeps the database alive for as long as the rig.
    _db: TestDb,
    pool: Arc<Pool>,
    checker: Arc<HealthChecker>,
    clock: Arc<ManualClock>,
    alerts: Arc<MemoryAlertSink>,
}

const ADDRESSES: [&str; 3] = ["198.51.100.10", "198.51.100.11", "198.51.100.12"];

/// Minutes between two passes of the rig.
const PASS_MINUTES: i64 = 5;

/// Minutes after the start by which every proxy has had its first lookup,
/// whatever ids the pool gave the proxies. The first pass only probes: a
/// proxy is `new` until then, and nothing is looked up for a new proxy. The
/// second pass, five minutes in, draws each proxy's first lookup from the
/// hour that follows it, by a hash of the proxy's random id. A pass sends
/// one lookup at most, so the other two proxies may each wait a pass longer.
///
/// The tests below used to stop at 65 minutes, which a proxy with an offset
/// above 55 minutes, or two proxies due in one pass, did not make.
const FIRST_LOOKUPS_WITHIN: i64 = PASS_MINUTES + 60 + 2 * PASS_MINUTES;

async fn rig(reputation: Option<Arc<dyn ReputationProvider>>) -> Rig {
    let db = TestDb::migrated().await;
    let clock = Arc::new(ManualClock::new(start()));
    let alerts = Arc::new(MemoryAlertSink::new());
    let lines = ADDRESSES
        .map(|address| format!("{address}:8080:user:pass:DE"))
        .join("\n");
    let pool = Arc::new(Pool::new(PoolParts {
        store: db.store.clone(),
        provider: Some(Arc::new(StaticListProvider::new(
            StaticProxy::parse_list(&lines).unwrap(),
        ))),
        cipher: CredentialCipher::new(&MASTER_KEY_BYTES),
        rules: PoolRules::default(),
        missing_syncs: 3,
        alerts: alerts.clone(),
        clock: clock.clone(),
        seed: Seed::from_u64(21),
    }));
    pool.reconcile().await.unwrap();
    let checker = Arc::new(HealthChecker::new(
        Arc::clone(&pool),
        Arc::new(EveryProxyAnswers),
        Arc::new(HostIp::fixed(ip("192.0.2.77"), start() + chrono::Duration::days(365)).unwrap()),
        reputation,
        HealthSettings {
            interval: chrono::Duration::minutes(5),
            jitter: 0.25,
            max_concurrent: 3,
            startup_spread: chrono::Duration::zero(),
            geo: false,
            geo_interval: chrono::Duration::hours(24),
            reputation_interval: chrono::Duration::hours(24),
            host_ip_refresh: chrono::Duration::days(3650),
            tick: Duration::from_secs(5),
        },
    ));
    Rig {
        _db: db,
        pool,
        checker,
        clock,
        alerts,
    }
}

impl Rig {
    /// Runs passes every five minutes for the given time.
    async fn run_for(&self, time: chrono::Duration) {
        let until = self.pool.clock().now() + time;
        while self.pool.clock().now() < until {
            self.checker.tick().await.unwrap();
            self.clock.advance(chrono::Duration::minutes(5));
        }
    }

    /// Runs passes like [`Rig::run_for`] and notes, in minutes since the
    /// start, when which address was looked up.
    async fn watch_for(
        &self,
        mock: &MockAbuse,
        time: chrono::Duration,
        seen: &mut Vec<(i64, String)>,
    ) {
        let until = self.pool.clock().now() + time;
        while self.pool.clock().now() < until {
            let before = mock.asked().len();
            self.checker.tick().await.unwrap();
            let asked = mock.asked();
            assert!(asked.len() <= before + 1, "a pass sent {asked:?}");
            if let Some(address) = asked.get(before) {
                let minute = (self.pool.clock().now() - start()).num_minutes();
                seen.push((minute, address.clone()));
            }
            self.clock.advance(chrono::Duration::minutes(PASS_MINUTES));
        }
    }

    async fn state_of(&self, address: &str) -> ProxyState {
        self.pool
            .live()
            .await
            .unwrap()
            .into_iter()
            .find(|proxy| proxy.host == address)
            .unwrap()
            .state
    }

    fn alerts_of(&self, kind: AlertKind) -> usize {
        self.alerts
            .kinds()
            .into_iter()
            .filter(|raised| *raised == kind)
            .count()
    }
}

#[tokio::test]
async fn every_proxy_is_looked_up_once_a_day_and_never_all_at_once() {
    let mock = MockAbuse::start().await;
    mock.set(ADDRESSES[0], 90, "Data Center/Web Hosting/Transit");
    mock.set(ADDRESSES[1], 10, "Fixed Line ISP");
    let rig = rig(Some(Arc::new(mock.client()))).await;

    // The first pass probes; the first lookups are spread over the hour
    // after the second, one per pass.
    rig.checker.tick().await.unwrap();
    assert_eq!(rig.state_of(ADDRESSES[0]).await, ProxyState::Healthy);
    let mut seen_after = Vec::new();
    for _ in 0..FIRST_LOOKUPS_WITHIN / PASS_MINUTES {
        rig.clock.advance(chrono::Duration::minutes(PASS_MINUTES));
        rig.checker.tick().await.unwrap();
        seen_after.push(mock.asked().len());
    }

    // Each address exactly once, and never more than one per pass.
    assert_eq!(
        mock.asked().into_iter().collect::<BTreeSet<_>>(),
        ADDRESSES
            .iter()
            .map(|address| (*address).to_owned())
            .collect()
    );
    assert_eq!(mock.asked().len(), 3);
    assert!(seen_after.windows(2).all(|pair| pair[1] - pair[0] <= 1));
    // Hosting space is reported at once; a bad score needs a second lookup.
    assert_eq!(rig.alerts_of(AlertKind::ReputationUsageTypeSuspicious), 1);
    assert_eq!(rig.alerts_of(AlertKind::ProxyReputationBad), 0);
    assert_eq!(rig.state_of(ADDRESSES[0]).await, ProxyState::Healthy);
    let proxies = rig.pool.live().await.unwrap();
    let scored = proxies
        .iter()
        .find(|proxy| proxy.host == ADDRESSES[0])
        .unwrap();
    let health = scored.last_health.as_ref().unwrap();
    assert_eq!(health.reputation_score, Some(90));
    assert_eq!(
        health.usage_type.as_deref(),
        Some("Data Center/Web Hosting/Transit")
    );
    assert!(health.reputation_checked_at.is_some());

    // Nothing more for the rest of the day: the next lookup of a proxy is
    // 18 to 30 hours after its first (a day, give or take the jitter).
    rig.run_for(chrono::Duration::hours(16)).await;
    assert_eq!(mock.asked().len(), 3);

    // The next day each one is looked up again: the bad one is confirmed.
    // By now 32 hours and a quarter have passed: more than the 30 hours
    // after the last first lookup, less than 18 hours after a second one.
    rig.run_for(chrono::Duration::hours(15)).await;
    assert_eq!(mock.asked().len(), 6);
    assert_eq!(rig.alerts_of(AlertKind::ProxyReputationBad), 1);
    assert_eq!(rig.alerts_of(AlertKind::ReputationUsageTypeSuspicious), 1);
    assert_eq!(rig.state_of(ADDRESSES[0]).await, ProxyState::Degraded);
    assert_eq!(rig.state_of(ADDRESSES[1]).await, ProxyState::Healthy);
    assert_eq!(rig.state_of(ADDRESSES[2]).await, ProxyState::Healthy);
    let alert = rig
        .alerts
        .alerts()
        .into_iter()
        .find(|alert| alert.kind == AlertKind::ProxyReputationBad)
        .unwrap();
    assert_eq!(alert.details["score"], 90);
    assert_eq!(alert.subject, Some(scored.id.as_uuid()));
}

#[tokio::test]
async fn a_provider_that_says_stop_is_left_alone_for_as_long_as_it_says() {
    let mock = MockAbuse::start().await;
    let rig = rig(Some(Arc::new(mock.client()))).await;
    mock.fail(Some((429, Some(7200))));

    // The first lookup that is due is refused. The earliest of the three is
    // due 65 minutes in at the latest; one pass more than that is run.
    rig.run_for(chrono::Duration::minutes(70)).await;
    assert_eq!(mock.asked().len(), 1);

    // Nothing is asked for the two hours it named, though all three are due.
    rig.run_for(chrono::Duration::minutes(50)).await;
    assert_eq!(mock.asked().len(), 1);

    // Then the lookups resume, one per pass. (The pause began with the
    // first lookup, some time in the first hour.)
    mock.fail(None);
    rig.run_for(chrono::Duration::minutes(100)).await;
    assert_eq!(mock.asked().len(), 4);
    assert_eq!(mock.asked()[1..].iter().collect::<BTreeSet<_>>().len(), 3);
    for proxy in rig.pool.live().await.unwrap() {
        assert_eq!(proxy.last_health.unwrap().reputation_score, Some(0));
    }

    // A refused key stops the lookups for a day.
    mock.fail(Some((401, None)));
    rig.run_for(chrono::Duration::hours(31)).await;
    let after_refusal = mock.asked().len();
    assert_eq!(after_refusal, 5);
    rig.run_for(chrono::Duration::hours(9)).await;
    assert_eq!(mock.asked().len(), after_refusal);
    assert_eq!(rig.alerts.alerts(), []);
}

#[tokio::test]
async fn a_lookup_that_fails_is_tried_again_an_hour_later() {
    let mock = MockAbuse::start().await;
    let rig = rig(Some(Arc::new(mock.client()))).await;
    mock.fail(Some((503, None)));
    let mut seen = Vec::new();

    // Long enough for every first lookup, and one pass more.
    let first = chrono::Duration::minutes(FIRST_LOOKUPS_WITHIN + PASS_MINUTES);
    rig.watch_for(&mock, first, &mut seen).await;
    // A failure for one proxy does not stop the lookups for the others.
    let failed: BTreeSet<&str> = seen.iter().map(|(_, address)| address.as_str()).collect();
    assert_eq!(failed, ADDRESSES.into_iter().collect(), "{seen:?}");

    // The provider answers again. A lookup that failed in the last pass
    // above is tried again an hour after it, and the other two may be due
    // in the same pass: an hour and two passes cover every retry.
    mock.fail(None);
    let retries = chrono::Duration::minutes(60 + 2 * PASS_MINUTES);
    rig.watch_for(&mock, retries, &mut seen).await;

    for address in ADDRESSES {
        let at: Vec<i64> = seen
            .iter()
            .filter(|(_, asked)| asked == address)
            .map(|(minute, _)| *minute)
            .collect();
        // It failed at least once and was asked again.
        assert!(at.len() >= 2, "{address}: {seen:?}");
        // Nobody is asked again within the hour, and everybody an hour
        // later: in the pass that is due then, or behind the other two.
        for pair in at.windows(2) {
            let waited = pair[1] - pair[0];
            assert!(
                (60..=60 + 2 * PASS_MINUTES).contains(&waited),
                "{address} was asked again after {waited} minutes: {seen:?}"
            );
        }
    }
    // The last lookup of each succeeded, and nothing follows it that day.
    for proxy in rig.pool.live().await.unwrap() {
        assert_eq!(proxy.last_health.unwrap().reputation_score, Some(0));
    }
    assert!(seen.len() <= 3 * ADDRESSES.len(), "{seen:?}");
}

#[tokio::test]
async fn without_a_key_nothing_is_ever_looked_up() {
    let mock = MockAbuse::start().await;
    let provider: Option<Arc<dyn ReputationProvider>> = AbuseIpDb::from_config(&ReputationConfig {
        base_url: format!("{}/api/v2/", mock.server.uri()),
        ..ReputationConfig::default()
    })
    .unwrap()
    .map(|client| -> Arc<dyn ReputationProvider> { Arc::new(client) });
    assert!(provider.is_none());
    let rig = rig(provider).await;

    rig.run_for(chrono::Duration::days(3)).await;

    assert_eq!(mock.asked(), Vec::<String>::new());
    for proxy in rig.pool.live().await.unwrap() {
        assert_eq!(proxy.state, ProxyState::Healthy);
        let health = proxy.last_health.unwrap();
        assert_eq!(health.reputation_score, None);
        assert_eq!(health.reputation_checked_at, None);
    }
}

#[tokio::test]
async fn the_checker_is_built_from_configuration() {
    let db = TestDb::migrated().await;
    let env = |extra: &[(&str, &str)]| {
        let mut pairs = vec![
            ("KAGE__DATABASE__URL", db.url.as_str()),
            ("KAGE__SIDECAR__TOKEN", "token"),
            ("KAGE__SECRETS__MASTER_KEY", MASTER_KEY),
        ];
        pairs.extend_from_slice(extra);
        Config::load(
            None,
            pairs
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned())),
        )
        .unwrap()
    };
    let pool = |config: &Config| {
        Arc::new(
            Pool::from_config(config, db.store.clone(), Arc::new(MemoryAlertSink::new())).unwrap(),
        )
    };

    // The defaults, without and with a reputation key.
    let config = env(&[]);
    let checker = HealthChecker::from_config(&config, pool(&config)).unwrap();
    assert_eq!(checker.host_ip(), None);
    assert!(format!("{checker:?}").contains("HealthChecker"));
    let config = env(&[("KAGE__REPUTATION__ABUSEIPDB_KEY", KEY)]);
    let checker = HealthChecker::from_config(&config, pool(&config)).unwrap();
    let printed = format!("{checker:?}");
    assert!(printed.contains("AbuseIpDb"), "{printed}");
    assert!(!printed.contains(KEY), "{printed}");

    // An emulation profile this build does not ship.
    let config = env(&[("KAGE__PROXIES__HEALTH__PROFILE", "chrome_1")]);
    let error = HealthChecker::from_config(&config, pool(&config)).unwrap_err();
    assert!(matches!(error, HealthSetupError::Probe(_)));
    assert!(error.to_string().contains("chrome_1"));
}
