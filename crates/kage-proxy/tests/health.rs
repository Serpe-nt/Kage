//! Health checks against a local echo, through local proxies, with a real
//! Postgres: reachability and latency, the leak check, the country check and
//! its cadence, failures and their confirmation, and the schedule.
//!
//! Every test proxy leaves through a loopback address of its own, so the
//! echo sees a distinct "exit address" per proxy; the host itself is seen as
//! 127.0.0.1.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use common::{Echo, GeoFixture, MASTER_KEY_BYTES, TestDb, TestProxy, loopback};
use kage_core::alert::{AlertKind, MemoryAlertSink};
use kage_core::config::{ProxyHealthConfig, StaticProxy};
use kage_core::domain::{HealthFailure, Proxy, ProxyProviderKind, ProxyState, ReplaceReason};
use kage_core::duration::HumanDuration;
use kage_core::ids::ProxyId;
use kage_core::seed::Seed;
use kage_proxy::health::{HostIp, HttpProber};
use kage_proxy::{
    Clock, CredentialCipher, HealthChecker, HealthSettings, ManualClock, Pool, PoolParts,
    PoolRules, ReplacementOutcome, StaticListProvider,
};

fn start() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap()
}

struct Net {
    db: TestDb,
    echo: Echo,
    proxies: Vec<TestProxy>,
    pool: Arc<Pool>,
    checker: Arc<HealthChecker>,
    clock: Arc<ManualClock>,
    alerts: Arc<MemoryAlertSink>,
}

fn settings() -> HealthSettings {
    HealthSettings {
        interval: chrono::Duration::minutes(5),
        jitter: 0.25,
        max_concurrent: 3,
        startup_spread: chrono::Duration::minutes(2),
        geo: true,
        geo_interval: chrono::Duration::hours(24),
        reputation_interval: chrono::Duration::hours(24),
        host_ip_refresh: chrono::Duration::hours(1),
        tick: Duration::from_secs(5),
    }
}

/// A pool of local proxies, one per `(last octet of the exit address,
/// country the provider states)`, already synced but not yet checked.
async fn net_with(exits: &[(u8, &str)], settings: HealthSettings) -> Net {
    let db = TestDb::migrated().await;
    let echo = Echo::start().await;
    let mut proxies = Vec::new();
    let mut lines = String::new();
    for (last, country) in exits {
        let proxy = TestProxy::with_exit(*last).await;
        lines.push_str(&proxy.line(country));
        lines.push('\n');
        proxies.push(proxy);
    }
    let clock = Arc::new(ManualClock::new(start()));
    let alerts = Arc::new(MemoryAlertSink::new());
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
        seed: Seed::from_u64(11),
    }));
    pool.reconcile().await.unwrap();

    let health = ProxyHealthConfig {
        timeout: HumanDuration::from_secs(3),
        echo_url: echo.url("/ip"),
        confirm_url: Some(echo.url("/confirm")),
        geo_url: Some(echo.url("/json")),
        host_ip_url: echo.url("/host"),
        ..ProxyHealthConfig::default()
    };
    let checker = Arc::new(HealthChecker::new(
        Arc::clone(&pool),
        Arc::new(HttpProber::from_config(&health).unwrap()),
        Arc::new(HostIp::new(health.host_ip_url.clone(), health.timeout.get()).unwrap()),
        None,
        settings,
    ));
    Net {
        db,
        echo,
        proxies,
        pool,
        checker,
        clock,
        alerts,
    }
}

async fn net(exits: &[(u8, &str)]) -> Net {
    net_with(exits, settings()).await
}

impl Net {
    /// The stored proxy behind the `index`th test proxy.
    async fn stored(&self, index: usize) -> Proxy {
        let port = self.proxies[index].addr.port();
        self.pool
            .proxies()
            .await
            .unwrap()
            .into_iter()
            .find(|proxy| proxy.port == port)
            .unwrap()
    }

    /// Probes one proxy now and returns it as it is afterwards.
    async fn check(&self, index: usize) -> Proxy {
        let id = self.stored(index).await.id;
        let results = self.checker.check_now(Some(&[id])).await.unwrap();
        assert_eq!(results.len(), 1);
        results.into_iter().next().unwrap().proxy
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
async fn a_reachable_proxy_becomes_healthy_with_latency_exit_address_and_geo() {
    let net = net(&[(2, "DE"), (3, "AT")]).await;
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    net.echo.set_geo(loopback(3), GeoFixture::vienna());
    assert_eq!(net.checker.host_ip(), None);

    let results = net.checker.check_now(None).await.unwrap();

    assert_eq!(results.len(), 2);
    // The host's own address was read first, directly.
    assert_eq!(net.checker.host_ip(), Some(loopback(1)));
    for (index, (exit, country, city, asn, org, timezone)) in [
        (
            2,
            "DE",
            "Berlin",
            "AS3320",
            "Deutsche Telekom AG",
            "Europe/Berlin",
        ),
        (
            3,
            "AT",
            "Vienna",
            "AS8447",
            "A1 Telekom Austria AG",
            "Europe/Vienna",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let proxy = net.stored(index).await;
        assert_eq!(proxy.state, ProxyState::Healthy);
        assert_eq!(proxy.consecutive_failures, 0);
        let health = proxy.last_health.as_ref().unwrap();
        assert!(health.reachable);
        assert_eq!(health.failure, None);
        assert_eq!(health.exit_ip, Some(loopback(exit)));
        assert!(health.latency_ms.is_some_and(|latency| latency < 3000));
        assert_eq!(health.leak_detected, Some(false));
        assert_eq!(health.exit_country.unwrap().as_str(), country);
        assert_eq!(health.country_match, Some(true));
        assert_eq!(health.checked_at, start());
        // The observed record, with everything a persona will be derived
        // from, and the network in the proxy's own column.
        let geo = proxy.observed_geo().unwrap();
        assert_eq!(geo.country.as_str(), country);
        assert_eq!(geo.city.as_deref(), Some(city));
        assert_eq!(geo.asn.as_deref(), Some(asn));
        assert_eq!(geo.org.as_deref(), Some(org));
        assert_eq!(geo.timezone.as_deref(), Some(timezone));
        assert_eq!(geo.ip, loopback(exit));
        assert_eq!(proxy.asn.as_deref(), Some(asn));
        assert_eq!(proxy.effective_country().unwrap().as_str(), country);
        // Both requests of the probe went through the proxy.
        assert_eq!(net.proxies[index].requests(), 2);
        assert_eq!(net.proxies[index].refused(), 0);
        // The result that came back is the stored one.
        assert!(results.iter().any(|result| result.proxy == proxy));
    }

    // Who saw what: the echo saw each exit address once, the geo lookup
    // once per proxy, the second echo was never needed, and the host's own
    // address showed up only for the direct lookup.
    let hits = net.echo.hits();
    let peers = |path: &str| -> BTreeSet<IpAddr> {
        hits.iter()
            .filter(|hit| hit.path == path)
            .map(|hit| hit.peer)
            .collect()
    };
    assert_eq!(peers("/ip"), BTreeSet::from([loopback(2), loopback(3)]));
    assert_eq!(peers("/json"), BTreeSet::from([loopback(2), loopback(3)]));
    assert_eq!(peers("/host"), BTreeSet::from([loopback(1)]));
    assert_eq!(net.echo.count("/ip"), 2);
    assert_eq!(net.echo.count("/host"), 1);
    assert_eq!(net.echo.count("/confirm"), 0);

    // The probe presents itself as the configured browser.
    let probe = hits.iter().find(|hit| hit.path == "/ip").unwrap();
    let header = |name: &str| probe.headers[name].to_str().unwrap().to_owned();
    assert!(
        header("user-agent").contains("Chrome/149."),
        "{}",
        header("user-agent")
    );
    assert!(header("user-agent").contains("Windows NT 10.0"));
    assert!(header("sec-ch-ua").contains("\"Google Chrome\";v=\"149\""));
    assert_eq!(header("sec-ch-ua-platform"), "\"Windows\"");
    assert_eq!(header("sec-ch-ua-mobile"), "?0");
    assert_eq!(header("sec-fetch-mode"), "navigate");
    for encoding in ["gzip", "deflate", "br", "zstd"] {
        assert!(header("accept-encoding").contains(encoding), "{encoding}");
    }
    // The direct lookup of the host address says honestly who is asking.
    let direct = hits.iter().find(|hit| hit.path == "/host").unwrap();
    assert_eq!(
        direct.headers["user-agent"],
        format!("kage/{}", env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(net.alerts.alerts(), []);
}

#[tokio::test]
async fn the_hosts_own_rows_are_never_probed_and_take_the_place_the_host_looks_up() {
    let net = net(&[(2, "DE")]).await;
    net.echo.set_geo(loopback(1), GeoFixture::vienna());
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    let ids = |rows: &[Proxy]| rows.iter().map(|row| row.id).collect::<Vec<_>>();

    // Two rows for the host's own connection, declared Austrian.
    let rows = net
        .pool
        .keep_host_rows(2, Some("AT".parse().unwrap()))
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row.provider_ref.as_str())
            .collect::<Vec<_>>(),
        ["host-1", "host-2"]
    );
    for row in &rows {
        assert_eq!(row.provider, ProxyProviderKind::Host);
        assert_eq!(row.state, ProxyState::Healthy);
        assert_eq!(
            row.country.map(|country| country.to_string()).as_deref(),
            Some("AT")
        );
    }
    // Kept again: nothing new.
    let again = net.pool.keep_host_rows(2, None).await.unwrap();
    assert_eq!(ids(&again), ids(&rows));

    // The checker probes the pool's proxy and never a host row, whose exit
    // is the host's own address and no leak.
    let results = net.checker.check_now(None).await.unwrap();
    assert_eq!(results.len(), 1);
    net.checker.tick().await.unwrap();
    assert!(!net.pool.egress_unsafe().await.unwrap());
    for row in net.pool.host_rows().await.unwrap() {
        assert_eq!(row.last_health, None);
    }

    // The host looks itself up, directly, and the row takes the place.
    let host = HostIp::new(net.echo.url("/host"), Duration::from_secs(3)).unwrap();
    let ip = host.refresh(start()).await.unwrap();
    let lookup = host.lookup(&net.echo.url("/json")).await.unwrap();
    assert_eq!(lookup.country.to_string(), "AT");
    let placed = net
        .pool
        .record_host_place(rows[0].id, ip, lookup.clone(), start())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(placed.observed_geo().unwrap().ip, ip);
    assert_eq!(placed.state, ProxyState::Healthy);
    assert!(!placed.leak_detected());
    assert!(!net.pool.egress_unsafe().await.unwrap());
    // A proxy of the pool is no host row.
    let pooled = net.stored(0).await.id;
    assert!(
        net.pool
            .record_host_place(pooled, ip, lookup, start())
            .await
            .unwrap()
            .is_none()
    );

    // Nobody replaces the host.
    assert_eq!(
        net.pool
            .request_replacement(rows[1].id, ReplaceReason::Operator)
            .await
            .unwrap(),
        ReplacementOutcome::Unsupported
    );

    // Fewer rows retire the rest, and none at all retires every one.
    assert_eq!(
        ids(&net.pool.keep_host_rows(1, None).await.unwrap()),
        ids(&rows[..1])
    );
    assert_eq!(net.pool.keep_host_rows(0, None).await.unwrap(), []);
    assert_eq!(net.pool.live().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_proxy_that_exits_through_the_host_address_is_a_leak() {
    // The first proxy leaves through 127.0.0.1, which is what the echo sees
    // the host itself as.
    let net = net(&[(1, "DE"), (2, "DE")]).await;
    net.echo.set_geo(loopback(1), GeoFixture::berlin());
    net.echo.set_geo(loopback(2), GeoFixture::berlin());

    net.checker.check_now(None).await.unwrap();

    let leaking = net.stored(0).await;
    assert_eq!(leaking.state, ProxyState::Backoff);
    assert!(leaking.leak_detected());
    let health = leaking.last_health.as_ref().unwrap();
    assert_eq!(health.failure, Some(HealthFailure::Leak));
    assert_eq!(health.exit_ip, Some(loopback(1)));
    assert!(leaking.backoff_until.unwrap() > start());
    assert_eq!(net.stored(1).await.state, ProxyState::Healthy);
    // One leak makes the whole pool unsafe to use.
    assert!(net.pool.egress_unsafe().await.unwrap());
    assert_eq!(net.alerts_of(AlertKind::ProxyLeak), 1);
    assert_eq!(net.alerts_of(AlertKind::EgressUnsafe), 1);

    // Checking again while it still leaks keeps it parked and stays quiet.
    let still = net.check(0).await;
    assert_eq!(still.state, ProxyState::Backoff);
    assert!(net.pool.egress_unsafe().await.unwrap());
    assert_eq!(net.alerts_of(AlertKind::ProxyLeak), 1);

    // The proxy leaves through an address of its own again: the next probe
    // through it clears the flag.
    net.proxies[0].set_exit(loopback(9));
    net.echo.set_geo(loopback(9), GeoFixture::berlin());
    let cleared = net.check(0).await;

    assert_eq!(cleared.state, ProxyState::Healthy);
    assert!(!cleared.leak_detected());
    assert_eq!(cleared.last_health.unwrap().exit_ip, Some(loopback(9)));
    assert!(!net.pool.egress_unsafe().await.unwrap());
}

#[tokio::test]
async fn a_country_mismatch_degrades_after_two_lookups_and_lookups_are_rare() {
    // The provider says Germany; the lookup says Austria.
    let net = net(&[(2, "DE")]).await;
    net.echo.set_geo(loopback(2), GeoFixture::vienna());

    let first = net.check(0).await;

    assert_eq!(first.state, ProxyState::Healthy);
    let health = first.last_health.as_ref().unwrap();
    assert_eq!(health.exit_country.unwrap().as_str(), "AT");
    assert_eq!(health.country_match, Some(false));
    assert_eq!(health.geo_mismatches, 1);
    assert_eq!(net.echo.count("/json"), 1);

    // Probes go on every five minutes; the lookup service is not asked
    // again while the exit address stays the same.
    for _ in 0..12 {
        net.clock.advance(chrono::Duration::minutes(5));
        assert_eq!(net.check(0).await.state, ProxyState::Healthy);
    }
    assert_eq!(net.echo.count("/ip"), 13);
    assert_eq!(net.echo.count("/json"), 1);

    // A day later (the wait is jittered up to thirty hours) it is.
    net.clock.advance(chrono::Duration::hours(30));
    let second = net.check(0).await;
    assert_eq!(net.echo.count("/json"), 2);
    assert_eq!(second.state, ProxyState::Degraded);
    assert_eq!(second.last_health.as_ref().unwrap().geo_mismatches, 2);
    assert_eq!(net.alerts_of(AlertKind::ProxyGeoMismatch), 1);
    // Still usable, and what it is observed as is what counts.
    assert!(second.state.is_usable());
    assert_eq!(second.country.unwrap().as_str(), "DE");
    assert_eq!(second.effective_country().unwrap().as_str(), "AT");

    // The exit address changes: looked up at once, whatever the schedule.
    net.proxies[0].set_exit(loopback(7));
    net.echo.set_geo(loopback(7), GeoFixture::berlin());
    net.clock.advance(chrono::Duration::minutes(5));
    let moved = net.check(0).await;
    assert_eq!(net.echo.count("/json"), 3);
    assert_eq!(moved.state, ProxyState::Healthy);
    assert_eq!(moved.observed_geo().unwrap().ip, loopback(7));
    assert_eq!(
        moved.observed_geo().unwrap().timezone.as_deref(),
        Some("Europe/Berlin")
    );
    assert_eq!(
        moved.last_health.as_ref().unwrap().country_match,
        Some(true)
    );
    let changed: Vec<_> = net
        .db
        .store
        .health_events()
        .recent(None, 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "proxy_geo_changed")
        .collect();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].details["previous"]["country"], "AT");
    assert_eq!(changed[0].details["current"]["country"], "DE");
}

#[tokio::test]
async fn a_lookup_service_that_does_not_answer_is_left_alone_for_an_hour() {
    let net = net(&[(2, "DE")]).await;
    // The lookup knows nothing about the address, then is down.
    let unknown = net.check(0).await;
    assert_eq!(unknown.state, ProxyState::Healthy);
    assert_eq!(unknown.observed_geo(), None);
    assert_eq!(unknown.last_health.as_ref().unwrap().country_match, None);
    // Without a lookup the provider's word stands.
    assert_eq!(unknown.effective_country().unwrap().as_str(), "DE");
    assert_eq!(net.echo.count("/json"), 1);

    net.echo.set_down("/json", true);
    for _ in 0..6 {
        net.clock.advance(chrono::Duration::minutes(5));
        assert_eq!(net.check(0).await.state, ProxyState::Healthy);
    }
    assert_eq!(net.echo.count("/json"), 1);

    net.clock.advance(chrono::Duration::minutes(31));
    net.check(0).await;
    assert_eq!(net.echo.count("/json"), 2);
    // Failed again: another hour.
    net.clock.advance(chrono::Duration::minutes(30));
    net.check(0).await;
    assert_eq!(net.echo.count("/json"), 2);

    // It is back and knows the address.
    net.echo.set_down("/json", false);
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    net.clock.advance(chrono::Duration::minutes(31));
    let found = net.check(0).await;
    assert_eq!(net.echo.count("/json"), 3);
    assert_eq!(
        found.observed_geo().unwrap().city.as_deref(),
        Some("Berlin")
    );
}

#[tokio::test]
async fn an_unreachable_proxy_is_degraded_then_parked_and_comes_back() {
    let net = net(&[(2, "DE"), (3, "DE"), (4, "DE"), (5, "DE")]).await;
    for exit in 2..=5 {
        net.echo.set_geo(loopback(exit), GeoFixture::berlin());
    }
    net.checker.check_now(None).await.unwrap();
    assert_eq!(net.stored(0).await.state, ProxyState::Healthy);

    // The proxy stops accepting connections.
    net.proxies[0].set_up(false);
    let mut states = Vec::new();
    for _ in 0..3 {
        net.clock.advance(chrono::Duration::minutes(5));
        // The scheduled pass, not an operator's check: a parked proxy is
        // left alone.
        let proxy = net.stored(0).await;
        let result = net.checker.check(&proxy).await.unwrap();
        states.push(result.proxy.state);
        assert_eq!(result.report.exit, Err(HealthFailure::Unreachable));
    }
    assert_eq!(
        states,
        [
            ProxyState::Degraded,
            ProxyState::Degraded,
            ProxyState::Backoff
        ]
    );
    let parked = net.stored(0).await;
    let health = parked.last_health.as_ref().unwrap();
    assert!(!health.reachable);
    assert_eq!(health.failure, Some(HealthFailure::Unreachable));
    assert_eq!(health.latency_ms, None);
    // What was learned while it worked is kept.
    assert_eq!(health.exit_ip, Some(loopback(2)));
    assert!(parked.observed_geo().is_some());
    assert_eq!(parked.consecutive_failures, 3);
    assert_eq!(
        parked.failing_since,
        Some(start() + chrono::Duration::minutes(5))
    );
    // Nobody saw a request from it: the proxy never forwarded one.
    assert_eq!(net.proxies[0].requests(), 2);
    // The others are untouched.
    for index in 1..4 {
        assert_eq!(net.stored(index).await.state, ProxyState::Healthy);
    }

    // The scheduled pass does not probe a parked proxy.
    net.clock.advance(chrono::Duration::seconds(10));
    net.checker.tick().await.unwrap();
    let still = net.stored(0).await;
    assert_eq!(still.state, ProxyState::Backoff);
    assert_eq!(still.last_health, parked.last_health);

    // It works again. When the backoff has run out the pool makes it
    // degraded, and the very next pass probes it: it does not wait for its
    // regular turn while requests could already use it.
    net.proxies[0].set_up(true);
    net.clock.set(parked.backoff_until.unwrap());
    net.pool.tick().await.unwrap();
    assert_eq!(net.stored(0).await.state, ProxyState::Degraded);
    net.checker.tick().await.unwrap();
    let back = net.stored(0).await;
    assert_eq!(back.state, ProxyState::Healthy);
    assert_eq!(back.consecutive_failures, 0);
    assert_eq!(back.backoff_count, 0);
    assert_eq!(back.failing_since, None);
}

#[tokio::test]
async fn a_failure_counts_only_when_a_second_echo_fails_too() {
    let net = net(&[(2, "DE"), (3, "DE")]).await;
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    net.echo.set_geo(loopback(3), GeoFixture::berlin());
    net.checker.check_now(None).await.unwrap();
    assert_eq!(net.echo.count("/confirm"), 0);

    // The provider's echo is down; the proxies are fine.
    net.echo.set_down("/ip", true);
    net.clock.advance(chrono::Duration::minutes(5));
    net.checker.check_now(None).await.unwrap();

    for index in 0..2 {
        let proxy = net.stored(index).await;
        assert_eq!(proxy.state, ProxyState::Healthy);
        assert_eq!(proxy.consecutive_failures, 0);
        let health = proxy.last_health.unwrap();
        assert!(health.reachable);
        assert_eq!(health.leak_detected, Some(false));
    }
    assert_eq!(net.echo.count("/confirm"), 2);

    // Both echoes are down: now it counts, with nothing read.
    net.echo.set_down("/confirm", true);
    net.clock.advance(chrono::Duration::minutes(5));
    let failed = net.check(0).await;
    assert_eq!(failed.state, ProxyState::Degraded);
    assert_eq!(
        failed.last_health.unwrap().failure,
        Some(HealthFailure::Unreachable)
    );
    assert_eq!(net.echo.count("/confirm"), 3);
}

#[tokio::test]
async fn refused_credentials_are_told_apart_from_a_dead_proxy() {
    let net = net(&[(2, "DE")]).await;
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    assert_eq!(net.check(0).await.state, ProxyState::Healthy);

    // The account's proxy password was changed at the provider.
    net.proxies[0].set_credentials("wsuser", "rotated-91cc");
    net.clock.advance(chrono::Duration::minutes(5));
    let refused = net.check(0).await;

    assert_eq!(refused.state, ProxyState::Degraded);
    assert_eq!(
        refused.last_health.unwrap().failure,
        Some(HealthFailure::ProxyAuth)
    );
    // Both echoes were tried, both times the proxy said no.
    assert_eq!(net.proxies[0].refused(), 2);
    assert_eq!(net.proxies[0].requests(), 2);
}

#[tokio::test]
async fn probes_are_spread_over_time_and_never_run_in_step() {
    let exits: Vec<(u8, &str)> = (2..=7).map(|last| (last, "DE")).collect();
    let net = net_with(
        &exits,
        HealthSettings {
            max_concurrent: 2,
            startup_spread: chrono::Duration::minutes(1),
            ..settings()
        },
    )
    .await;
    for (last, _) in &exits {
        net.echo.set_geo(loopback(*last), GeoFixture::berlin());
    }

    // Passes every five seconds for nine minutes.
    let mut probes: HashMap<ProxyId, Vec<DateTime<Utc>>> = HashMap::new();
    let mut busiest = 0;
    let mut ticks_with_first_probes = BTreeSet::new();
    for tick in 0..108 {
        let ran = net.checker.tick().await.unwrap();
        busiest = busiest.max(ran);
        for proxy in net.pool.live().await.unwrap() {
            let Some(health) = &proxy.last_health else {
                continue;
            };
            let seen = probes.entry(proxy.id).or_default();
            if seen.last() != Some(&health.checked_at) {
                if seen.is_empty() {
                    ticks_with_first_probes.insert(tick);
                }
                seen.push(health.checked_at);
            }
        }
        net.clock.advance(chrono::Duration::seconds(5));
    }

    // Every proxy was probed, the first time within the startup spread.
    assert_eq!(probes.len(), 6);
    for times in probes.values() {
        assert!(
            times[0] < start() + chrono::Duration::seconds(70),
            "{times:?}"
        );
        // And again after five minutes, give or take a quarter, never
        // sooner and never much later.
        assert!(times.len() >= 2, "{times:?}");
        for pair in times.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(gap >= chrono::Duration::seconds(225), "{gap}");
            assert!(gap <= chrono::Duration::seconds(390), "{gap}");
        }
    }
    // Never more than two at once, and not all six in the same passes.
    assert!(busiest <= 2, "{busiest} probes in one pass");
    assert!(
        ticks_with_first_probes.len() >= 3,
        "{ticks_with_first_probes:?}"
    );
    // The waits differ from proxy to proxy: no two proxies keep the same
    // rhythm.
    let gaps: BTreeSet<i64> = probes
        .values()
        .map(|times| (times[1] - times[0]).num_seconds())
        .collect();
    assert!(gaps.len() >= 4, "{gaps:?}");
    for proxy in net.pool.live().await.unwrap() {
        assert_eq!(proxy.state, ProxyState::Healthy);
    }
}

#[tokio::test]
async fn real_traffic_postpones_a_probe_but_not_forever() {
    let net = net(&[(2, "DE")]).await;
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    let id = net.check(0).await.id;
    let probes = || net.echo.count("/ip");
    assert_eq!(probes(), 1);
    let minutes = chrono::Duration::minutes;

    // Three minutes in, a real request goes through the proxy.
    net.clock.advance(minutes(3));
    net.pool.report_traffic_ok(id);
    // Seven minutes in the probe would be overdue (the longest wait is six
    // and a quarter), but the proxy served a page four minutes ago.
    net.clock.advance(minutes(4));
    assert_eq!(net.checker.tick().await.unwrap(), 0);
    assert_eq!(probes(), 1);
    // Five minutes after that request it is probed.
    net.clock.advance(minutes(1) + chrono::Duration::seconds(5));
    assert_eq!(net.checker.tick().await.unwrap(), 1);
    assert_eq!(probes(), 2);

    // Traffic keeps coming every four minutes. The probe still happens
    // fifteen minutes after the last one at the latest: the exit address
    // and the leak check need a real probe now and then.
    let last_probe = net.clock.now();
    while net.clock.now() < last_probe + minutes(14) {
        net.clock.advance(minutes(2));
        net.pool.report_traffic_ok(id);
        assert_eq!(
            net.checker.tick().await.unwrap(),
            0,
            "{}",
            net.clock.now() - last_probe
        );
    }
    net.clock.advance(minutes(1) + chrono::Duration::seconds(5));
    assert_eq!(net.checker.tick().await.unwrap(), 1);
    assert_eq!(probes(), 3);

    // A proxy that is not healthy is not taken on trust.
    net.proxies[0].set_up(false);
    net.clock.advance(minutes(7));
    assert_eq!(net.checker.tick().await.unwrap(), 1);
    assert_eq!(net.stored(0).await.state, ProxyState::Degraded);
    net.clock.advance(minutes(3));
    net.pool.report_traffic_ok(id);
    net.clock.advance(minutes(4));
    assert_eq!(net.checker.tick().await.unwrap(), 1);
}

#[tokio::test]
async fn without_the_host_address_probes_run_and_an_alert_says_the_leak_check_cannot() {
    let net = net(&[(2, "DE")]).await;
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    net.echo.set_down("/host", true);

    let proxy = net.check(0).await;

    assert_eq!(proxy.state, ProxyState::Healthy);
    assert_eq!(proxy.last_health.unwrap().leak_detected, None);
    assert_eq!(net.checker.host_ip(), None);
    assert_eq!(net.alerts_of(AlertKind::HostIpUnknown), 1);
    // Not asked again within a minute, and not alerted twice.
    net.checker.tick().await.unwrap();
    assert_eq!(net.echo.count("/host"), 1);

    net.echo.set_down("/host", false);
    net.clock.advance(chrono::Duration::minutes(2));
    net.checker.tick().await.unwrap();
    assert_eq!(net.checker.host_ip(), Some(loopback(1)));
    assert_eq!(net.alerts_of(AlertKind::HostIpUnknown), 1);
    // From here on every probe is compared.
    net.clock.advance(chrono::Duration::minutes(7));
    net.checker.tick().await.unwrap();
    assert_eq!(
        net.stored(0).await.last_health.unwrap().leak_detected,
        Some(false)
    );

    // An hour later it is read again; a failure then keeps the old address.
    net.echo.set_down("/host", true);
    net.clock.advance(chrono::Duration::minutes(61));
    net.checker.tick().await.unwrap();
    assert_eq!(net.echo.count("/host"), 3);
    assert_eq!(net.checker.host_ip(), Some(loopback(1)));
    assert_eq!(net.alerts_of(AlertKind::HostIpUnknown), 1);
}

#[tokio::test]
async fn the_checker_runs_until_it_is_told_to_stop() {
    let net = net_with(
        &[(2, "DE"), (3, "DE")],
        HealthSettings {
            startup_spread: chrono::Duration::zero(),
            tick: Duration::from_millis(20),
            ..settings()
        },
    )
    .await;
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    net.echo.set_geo(loopback(3), GeoFixture::berlin());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(Arc::clone(&net.checker).run(async {
        let _ = stopped.await;
    }));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let healthy = net
            .pool
            .live()
            .await
            .unwrap()
            .iter()
            .filter(|proxy| proxy.state == ProxyState::Healthy)
            .count();
        if healthy == 2 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the proxies were never probed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the checker did not stop")
        .unwrap();
    // The clock stood still, so nothing was probed twice.
    assert_eq!(net.echo.count("/ip"), 2);
}

#[tokio::test]
async fn a_proxy_whose_credentials_do_not_open_is_skipped_not_probed() {
    let net = net(&[(2, "DE"), (3, "DE")]).await;
    net.echo.set_geo(loopback(2), GeoFixture::berlin());
    net.echo.set_geo(loopback(3), GeoFixture::berlin());
    // The stored password of the first proxy is not something this master
    // key sealed.
    let broken = net.stored(0).await;
    net.db
        .store
        .proxies()
        .modify(broken.id, |stored| {
            stored.password = kage_core::secret::SealedSecret::new(vec![1; 60]);
        })
        .await
        .unwrap();

    let results = net.checker.check_now(None).await.unwrap();

    // The other proxy is checked as usual.
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].proxy.id, net.stored(1).await.id);
    assert_eq!(net.stored(0).await.state, ProxyState::New);
    assert_eq!(net.proxies[0].requests(), 0);

    // The scheduled pass skips it too, and does not try again at once.
    net.clock.advance(chrono::Duration::minutes(3));
    assert_eq!(net.checker.tick().await.unwrap(), 0);
    net.clock.advance(chrono::Duration::seconds(5));
    assert_eq!(net.checker.tick().await.unwrap(), 0);
    assert_eq!(net.proxies[0].requests(), 0);

    // The next sync with the provider seals the credentials again.
    assert_eq!(net.pool.reconcile().await.unwrap().updated, [broken.id]);
    assert_eq!(net.check(0).await.state, ProxyState::Healthy);
}
