//! The health checker (spec §7.4): probes every proxy on its own jittered
//! schedule and hands the results to the pool's state machine.
//!
//! | Check | When | How |
//! |---|---|---|
//! | Reachability, latency, exit address | every `interval` × (1 ± `jitter`), per proxy | the provider's echo through the proxy; a second echo once before a failure counts |
//! | Leak | with every probe | exit address ≠ the host's own, read directly at start and hourly |
//! | Country | first probe, exit address change, once per `geo_interval` | a geo lookup through the proxy |
//! | Reputation | once per `reputation.interval`, only with an API key | AbuseIPDB, directly |
//!
//! No two proxies are probed in step: every proxy draws its own waits from
//! the pool seed, the first probes after a start are spread over
//! `startup_spread`, and at most `max_concurrent` probes are in flight. A
//! proxy that real traffic just went through has its next probe postponed
//! (see [`Pool::report_traffic_ok`]).
//!
//! Probes only ever go to the configured echo and geo endpoints, never to
//! a target site.

mod host_ip;
mod probe;
mod reputation;

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, Utc};
use kage_core::alert::{AlertKind, AlertSeverity};
use kage_core::config::Config;
use kage_core::domain::{GeoRecord, Proxy, ProxyState};
use kage_core::ids::ProxyId;
use serde_json::json;
use tokio::task::JoinSet;

pub use host_ip::{HostIp, HostIpError};
pub use probe::{
    EmulatedClientFactory, GeoLookup, GeoPolicy, HttpProber, ProbeClientError, ProbeClientFactory,
    ProbeOutcome, Prober, parse_geo, parse_ip, profile_names,
};
pub use reputation::{AbuseIpDb, Reputation, ReputationError, ReputationProvider};

use crate::machine::{ProbeReport, ReputationReport};
use crate::pool::{Pool, PoolError};

/// After a geo lookup that did not answer, the next one waits this long, so
/// a lookup service that is down or rate limiting is not asked at every
/// probe.
const GEO_RETRY: chrono::Duration = chrono::Duration::hours(1);

/// After a failed reputation lookup, the next one for that proxy waits this
/// long.
const REPUTATION_RETRY: chrono::Duration = chrono::Duration::hours(1);

/// Longest pause after the reputation provider says the allowance is used
/// up or refuses the key.
const REPUTATION_PAUSE: chrono::Duration = chrono::Duration::hours(24);

/// A failed read of the host address is tried again after this long.
const HOST_IP_RETRY: chrono::Duration = chrono::Duration::minutes(1);

/// Real traffic postpones a proxy's probe, but never beyond this many
/// intervals after the last one: the exit address and the leak check still
/// need a real probe now and then.
const LONGEST_POSTPONEMENT: i32 = 3;

/// Why the health checker could not be built.
#[derive(Debug, thiserror::Error)]
pub enum HealthSetupError {
    /// The probe client cannot be built.
    #[error(transparent)]
    Probe(#[from] ProbeClientError),
    /// The host address lookup cannot be built.
    #[error("host address lookup: {0}")]
    HostIp(#[from] HostIpError),
    /// The reputation client cannot be built.
    #[error(transparent)]
    Reputation(#[from] ReputationError),
}

/// The numbers the health checker works with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HealthSettings {
    /// Time between two probes of one proxy, before jitter.
    pub interval: chrono::Duration,
    /// Each wait is the interval times a factor from `1 - jitter` to
    /// `1 + jitter`.
    pub jitter: f64,
    /// Most probes in flight at once.
    pub max_concurrent: usize,
    /// The first probes after a start are spread over this long.
    pub startup_spread: chrono::Duration,
    /// Whether probes include geo lookups at all.
    pub geo: bool,
    /// Time between two geo lookups of one proxy, before jitter.
    pub geo_interval: chrono::Duration,
    /// Time between two reputation lookups of one proxy, before jitter.
    pub reputation_interval: chrono::Duration,
    /// How often the host's own address is read again.
    pub host_ip_refresh: chrono::Duration,
    /// How often the checker looks for due probes.
    pub tick: std::time::Duration,
}

impl HealthSettings {
    /// The settings in the configuration.
    pub fn from_config(config: &Config) -> Self {
        let duration = |value: std::time::Duration| {
            chrono::Duration::from_std(value).unwrap_or(chrono::Duration::MAX)
        };
        let health = &config.proxies.health;
        Self {
            interval: duration(health.interval.get()),
            jitter: health.jitter,
            max_concurrent: usize::try_from(health.max_concurrent).unwrap_or(usize::MAX),
            startup_spread: duration(health.startup_spread.get()),
            geo: health.geo_url.is_some(),
            geo_interval: duration(health.geo_interval.get()),
            reputation_interval: duration(config.reputation.interval.get()),
            host_ip_refresh: duration(health.host_ip_refresh.get()),
            tick: std::time::Duration::from_secs(5).min(health.interval.get()),
        }
    }
}

/// One proxy's place in the schedule.
#[derive(Debug, Clone, Copy)]
struct Slot {
    next_probe: DateTime<Utc>,
    last_probe: Option<DateTime<Utc>>,
    /// Probes so far; part of the seed of the next wait.
    probes: u64,
    /// The checker last saw the proxy in backoff. When it sees it out of
    /// backoff, the proxy is probed at once: it is usable again, and
    /// nothing has verified it since it failed.
    parked: bool,
    geo_retry_at: Option<DateTime<Utc>>,
    next_reputation: Option<DateTime<Utc>>,
}

#[derive(Debug, Default)]
struct Memory {
    schedule: HashMap<ProxyId, Slot>,
    reputation_paused_until: Option<DateTime<Utc>>,
    host_ip_tried_at: Option<DateTime<Utc>>,
}

/// One finished check of one proxy.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    /// The proxy as it is after the check.
    pub proxy: Proxy,
    /// What the probe found.
    pub report: ProbeReport,
}

/// The health checker. See the module documentation.
#[derive(Debug)]
pub struct HealthChecker {
    pool: Arc<Pool>,
    prober: Arc<dyn Prober>,
    host_ip: Arc<HostIp>,
    reputation: Option<Arc<dyn ReputationProvider>>,
    settings: HealthSettings,
    memory: Mutex<Memory>,
}

impl HealthChecker {
    /// A checker from its parts.
    pub fn new(
        pool: Arc<Pool>,
        prober: Arc<dyn Prober>,
        host_ip: Arc<HostIp>,
        reputation: Option<Arc<dyn ReputationProvider>>,
        settings: HealthSettings,
    ) -> Self {
        Self {
            pool,
            prober,
            host_ip,
            reputation,
            settings,
            memory: Mutex::new(Memory::default()),
        }
    }

    /// The checker the configuration describes: probes through the
    /// configured emulation profile, the configured echo for the host
    /// address, and AbuseIPDB when its key is set.
    ///
    /// # Errors
    ///
    /// Fails when the emulation profile is unknown or a client cannot be
    /// built.
    pub fn from_config(config: &Config, pool: Arc<Pool>) -> Result<Self, HealthSetupError> {
        let clients = EmulatedClientFactory::from_config(&config.proxies.health)?;
        Self::with_clients(config, pool, clients)
    }

    /// The checker the configuration describes, probing with the clients
    /// of `clients` instead of the one configured profile. The persona
    /// manager passes a factory that answers with the client of the
    /// persona behind each proxy, so that a probe carries the fingerprint
    /// of that persona's real traffic (spec §7.4).
    ///
    /// # Errors
    ///
    /// Fails when the host address lookup or the reputation client cannot
    /// be built.
    pub fn with_clients<F: ProbeClientFactory + 'static>(
        config: &Config,
        pool: Arc<Pool>,
        clients: F,
    ) -> Result<Self, HealthSetupError> {
        let health = &config.proxies.health;
        let reputation: Option<Arc<dyn ReputationProvider>> =
            match AbuseIpDb::from_config(&config.reputation)? {
                Some(provider) => Some(Arc::new(provider)),
                None => None,
            };
        tracing::info!(
            echo = %health.echo_url,
            geo = health.geo_url.as_deref().unwrap_or("off"),
            profile = %health.profile,
            reputation = reputation.as_ref().map_or("off", |provider| provider.name()),
            "proxy health checks configured"
        );
        Ok(Self::new(
            pool,
            Arc::new(HttpProber::new(clients, health)),
            Arc::new(HostIp::new(
                health.host_ip_url.clone(),
                health.timeout.get(),
            )?),
            reputation,
            HealthSettings::from_config(config),
        ))
    }

    /// The host's own address as the checker last read it.
    pub fn host_ip(&self) -> Option<IpAddr> {
        self.host_ip.current()
    }

    fn memory(&self) -> MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A factor from `1 - jitter` to `1 + jitter`, fixed by the pool seed
    /// and `parts`.
    fn spread(&self, label: &str, parts: &[&[u8]]) -> f64 {
        let jitter = self.settings.jitter.clamp(0.0, 0.9);
        2.0f64.mul_add(jitter * self.pool.unit(label, parts), 1.0 - jitter)
    }

    /// The wait before a proxy's next probe.
    fn wait(&self, proxy: ProxyId, probes: u64) -> chrono::Duration {
        scale(
            self.settings.interval,
            self.spread(
                "probe-wait",
                &[proxy.as_uuid().as_bytes(), &probes.to_le_bytes()],
            ),
        )
    }

    /// Where a proxy the checker has not scheduled yet starts.
    fn first_slot(&self, proxy: &Proxy, now: DateTime<Utc>) -> Slot {
        let last_probe = proxy.last_health.as_ref().map(|health| health.checked_at);
        // Never checked, or overdue after a restart: somewhere in the
        // startup spread, so a pool does not wake up as one.
        let spread = || {
            now + scale(
                self.settings.startup_spread,
                self.pool
                    .unit("probe-start", &[proxy.id.as_uuid().as_bytes()]),
            )
        };
        let next_probe = last_probe
            .map(|at| at + self.wait(proxy.id, 0))
            .filter(|next| *next > now)
            .unwrap_or_else(spread);
        Slot {
            next_probe,
            last_probe,
            probes: 0,
            parked: false,
            geo_retry_at: None,
            next_reputation: None,
        }
    }

    /// When a scheduled proxy is due, with real traffic taken into account.
    fn due_at(&self, proxy: &Proxy, slot: &Slot) -> DateTime<Utc> {
        let (Some(traffic), Some(last)) = (self.pool.last_traffic(proxy.id), slot.last_probe)
        else {
            return slot.next_probe;
        };
        // Only a healthy proxy is taken on trust; one that is degraded or
        // on probation gets its probe when it is due.
        if proxy.state != ProxyState::Healthy || traffic <= last {
            return slot.next_probe;
        }
        let postponed = traffic + self.settings.interval;
        let ceiling = last + self.settings.interval * LONGEST_POSTPONEMENT;
        slot.next_probe.max(postponed.min(ceiling))
    }

    /// Whether this probe should look the exit address up.
    fn geo_policy(&self, proxy: &Proxy, now: DateTime<Utc>) -> GeoPolicy {
        if !self.settings.geo {
            return GeoPolicy::Never;
        }
        let retry_at = self
            .memory()
            .schedule
            .get(&proxy.id)
            .and_then(|slot| slot.geo_retry_at);
        if retry_at.is_some_and(|at| at > now) {
            return GeoPolicy::Never;
        }
        match proxy.observed_geo() {
            None => GeoPolicy::Always,
            Some(geo) => {
                let wait = scale(
                    self.settings.geo_interval,
                    self.spread(
                        "geo-wait",
                        &[
                            proxy.id.as_uuid().as_bytes(),
                            &geo.checked_at.timestamp_millis().to_le_bytes(),
                        ],
                    ),
                );
                if now.signed_duration_since(geo.checked_at) >= wait {
                    GeoPolicy::Always
                } else {
                    GeoPolicy::OnChange { known: geo.ip }
                }
            }
        }
    }

    /// Probes one proxy now and applies the result.
    ///
    /// # Errors
    ///
    /// Fails when the proxy's credentials do not open (the next sync with
    /// the provider seals them again), the proxy is gone, or the store
    /// fails.
    pub async fn check(&self, proxy: &Proxy) -> Result<CheckResult, PoolError> {
        let started = self.pool.clock().now();
        let endpoint = self.pool.endpoint(proxy)?;
        let policy = self.geo_policy(proxy, started);
        let outcome = self.prober.probe(&endpoint, policy).await;
        let at = self.pool.clock().now();

        let host = self.host_ip.current();
        let leak = match (&outcome.exit, host) {
            (Ok(exit), Some(host)) => Some(exit.ip == host),
            _ => None,
        };
        let geo = match (&outcome.exit, outcome.geo) {
            (Ok(exit), Some(lookup)) => Some(GeoRecord {
                country: lookup.country,
                city: lookup.city,
                region: lookup.region,
                asn: lookup.asn,
                org: lookup.org,
                timezone: lookup.timezone,
                loc: lookup.loc,
                ip: exit.ip,
                checked_at: at,
            }),
            _ => None,
        };
        let geo_failed = outcome.geo_attempted && geo.is_none();
        let report = ProbeReport {
            at,
            exit: outcome.exit,
            leak,
            geo,
        };

        {
            let mut memory = self.memory();
            let probes = memory
                .schedule
                .get(&proxy.id)
                .map_or(0, |slot| slot.probes)
                .saturating_add(1);
            let next_probe = at + self.wait(proxy.id, probes);
            let slot = memory.schedule.entry(proxy.id).or_insert(Slot {
                next_probe,
                last_probe: None,
                probes: 0,
                parked: false,
                geo_retry_at: None,
                next_reputation: None,
            });
            slot.next_probe = next_probe;
            slot.last_probe = Some(at);
            slot.probes = probes;
            if geo_failed {
                slot.geo_retry_at = Some(at + GEO_RETRY);
            } else if report.geo.is_some() {
                slot.geo_retry_at = None;
            }
        }

        let after = self
            .pool
            .record_probe(proxy.id, &report)
            .await?
            .ok_or(PoolError::NotFound(proxy.id))?;
        Ok(CheckResult {
            proxy: after,
            report,
        })
    }

    /// Probes proxies now, whatever their schedule says: every live proxy,
    /// or only those in `only`. A proxy in backoff has its backoff ended
    /// first; a proxy that is being replaced is left alone. The host
    /// address is read first if it is not known yet. For an operator's
    /// `kage proxy health`.
    ///
    /// # Errors
    ///
    /// Fails when the store fails. A proxy whose credentials do not open is
    /// skipped with an error in the log.
    pub async fn check_now(
        self: &Arc<Self>,
        only: Option<&[ProxyId]>,
    ) -> Result<Vec<CheckResult>, PoolError> {
        let now = self.pool.clock().now();
        self.refresh_host_ip(now).await;
        let mut proxies = Vec::new();
        for proxy in self.pool.live().await? {
            if only.is_some_and(|ids| !ids.contains(&proxy.id)) {
                continue;
            }
            match proxy.state {
                ProxyState::Backoff => {
                    if let Some(ended) = self.pool.end_backoff(proxy.id).await? {
                        proxies.push(ended);
                    }
                }
                ProxyState::New | ProxyState::Healthy | ProxyState::Degraded => {
                    proxies.push(proxy);
                }
                ProxyState::Replacing | ProxyState::Retired => {}
            }
        }

        let mut results = Vec::with_capacity(proxies.len());
        let mut waiting = proxies.into_iter();
        let mut running = JoinSet::new();
        loop {
            while running.len() < self.settings.max_concurrent.max(1)
                && let Some(proxy) = waiting.next()
            {
                let checker = Arc::clone(self);
                running.spawn(async move {
                    checker
                        .check(&proxy)
                        .await
                        .map_err(|error| (proxy.id, error))
                });
            }
            let Some(joined) = running.join_next().await else {
                break;
            };
            match joined {
                Ok(Ok(result)) => results.push(result),
                Ok(Err((proxy, PoolError::Store(error)))) => {
                    tracing::error!(%proxy, %error, "cannot store the probe result");
                    return Err(error.into());
                }
                Ok(Err((proxy, error))) => {
                    tracing::error!(%proxy, %error, "cannot probe the proxy");
                }
                Err(error) => tracing::error!(%error, "a probe task failed"),
            }
        }
        results.sort_by_key(|result| result.proxy.id);
        Ok(results)
    }

    /// Reads the host address when it is unknown or due for a refresh.
    async fn refresh_host_ip(&self, now: DateTime<Utc>) {
        let fresh = self
            .host_ip
            .learned_at()
            .is_some_and(|at| now.signed_duration_since(at) < self.settings.host_ip_refresh);
        if fresh {
            return;
        }
        {
            let mut memory = self.memory();
            if memory
                .host_ip_tried_at
                .is_some_and(|at| now.signed_duration_since(at) < HOST_IP_RETRY)
            {
                return;
            }
            memory.host_ip_tried_at = Some(now);
        }
        match self.host_ip.refresh(now).await {
            Ok(_) => {
                self.pool.settle(AlertKind::HostIpUnknown, None);
                tracing::debug!("the host's public address was read");
            }
            Err(error) if self.host_ip.current().is_none() => {
                self.pool.raise_once(self.pool.alert(
                    AlertKind::HostIpUnknown,
                    AlertSeverity::Warning,
                    "the host's own public address could not be read, so the leak check cannot \
                     run",
                    json!({ "error": error.to_string() }),
                ));
            }
            Err(error) => {
                tracing::warn!(%error, "cannot refresh the host's public address; keeping the last one");
            }
        }
    }

    /// One pass: refreshes the host address when due, probes the proxies
    /// that are due (at most `max_concurrent`, the most overdue first), and
    /// sends at most one reputation lookup. Returns how many probes ran.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn tick(self: &Arc<Self>) -> Result<usize, PoolError> {
        let now = self.pool.clock().now();
        self.refresh_host_ip(now).await;
        let live = self.pool.live().await?;

        let mut due: Vec<(DateTime<Utc>, &Proxy)> = Vec::new();
        {
            let mut memory = self.memory();
            memory
                .schedule
                .retain(|id, _| live.iter().any(|proxy| proxy.id == *id));
            for proxy in &live {
                let slot = memory
                    .schedule
                    .entry(proxy.id)
                    .or_insert_with(|| self.first_slot(proxy, now));
                if !matches!(
                    proxy.state,
                    ProxyState::New | ProxyState::Healthy | ProxyState::Degraded
                ) {
                    slot.parked = proxy.state == ProxyState::Backoff;
                    continue;
                }
                if slot.parked {
                    slot.parked = false;
                    slot.next_probe = now;
                }
                let slot = *slot;
                let at = self.due_at(proxy, &slot);
                if at <= now {
                    due.push((at, proxy));
                }
            }
        }
        due.sort_by_key(|(at, proxy)| (*at, proxy.id));
        due.truncate(self.settings.max_concurrent.max(1));

        let mut running = JoinSet::new();
        for (_, proxy) in &due {
            let checker = Arc::clone(self);
            let proxy = (*proxy).clone();
            running.spawn(async move {
                checker
                    .check(&proxy)
                    .await
                    .map_err(|error| (proxy.id, error))
            });
        }
        let mut probed = 0;
        let mut store_error = None;
        while let Some(joined) = running.join_next().await {
            match joined {
                Ok(Ok(_)) => probed += 1,
                Ok(Err((proxy, error))) => {
                    tracing::error!(%proxy, %error, "cannot probe the proxy");
                    // Not again at the very next pass.
                    let later = now + self.settings.interval;
                    if let Some(slot) = self.memory().schedule.get_mut(&proxy) {
                        slot.next_probe = slot.next_probe.max(later);
                    }
                    if let PoolError::Store(error) = error {
                        store_error = Some(error);
                    }
                }
                Err(error) => tracing::error!(%error, "a probe task failed"),
            }
        }
        if let Some(error) = store_error {
            return Err(error.into());
        }

        self.reputation_pass(&live, now).await?;
        Ok(probed)
    }

    /// Looks up the reputation of at most one proxy that is due.
    async fn reputation_pass(&self, live: &[Proxy], now: DateTime<Utc>) -> Result<(), PoolError> {
        let Some(provider) = &self.reputation else {
            return Ok(());
        };
        let candidate = {
            let mut memory = self.memory();
            if memory
                .reputation_paused_until
                .is_some_and(|until| until > now)
            {
                return Ok(());
            }
            let mut candidate: Option<(DateTime<Utc>, ProxyId, IpAddr)> = None;
            for proxy in live {
                if !matches!(
                    proxy.state,
                    ProxyState::Healthy | ProxyState::Degraded | ProxyState::Backoff
                ) {
                    continue;
                }
                let Some(ip) = proxy
                    .last_health
                    .as_ref()
                    .and_then(|health| health.exit_ip)
                    .or_else(|| proxy.host.parse().ok())
                else {
                    continue;
                };
                let Some(slot) = memory.schedule.get_mut(&proxy.id) else {
                    continue;
                };
                let due = *slot.next_reputation.get_or_insert_with(|| {
                    let looked_up = proxy
                        .last_health
                        .as_ref()
                        .and_then(|health| health.reputation_checked_at);
                    match looked_up {
                        Some(at) => at + self.reputation_wait(proxy.id, at),
                        // Never looked up: within the first hour, not all
                        // at once.
                        None => {
                            now + scale(
                                self.settings
                                    .reputation_interval
                                    .min(chrono::Duration::hours(1)),
                                self.pool
                                    .unit("reputation-start", &[proxy.id.as_uuid().as_bytes()]),
                            )
                        }
                    }
                });
                if due <= now && candidate.is_none_or(|(earliest, _, _)| due < earliest) {
                    candidate = Some((due, proxy.id, ip));
                }
            }
            candidate
        };
        let Some((_, id, ip)) = candidate else {
            return Ok(());
        };

        let next = match provider.lookup(ip).await {
            Ok(reputation) => {
                self.pool
                    .record_reputation(
                        id,
                        &ReputationReport {
                            at: now,
                            score: reputation.score,
                            usage_type: reputation.usage_type,
                        },
                    )
                    .await?;
                now + self.reputation_wait(id, now)
            }
            Err(error) => {
                let pause = match &error {
                    ReputationError::RateLimited { retry_after } => Some(
                        retry_after
                            .and_then(|wait| chrono::Duration::from_std(wait).ok())
                            .unwrap_or(REPUTATION_RETRY)
                            .min(REPUTATION_PAUSE),
                    ),
                    ReputationError::Unauthorized(_) => Some(REPUTATION_PAUSE),
                    _ => None,
                };
                tracing::warn!(
                    proxy = %id, provider = provider.name(), %error,
                    paused = pause.is_some(),
                    "reputation lookup failed"
                );
                if let Some(pause) = pause {
                    self.memory().reputation_paused_until = Some(now + pause);
                }
                now + REPUTATION_RETRY
            }
        };
        if let Some(slot) = self.memory().schedule.get_mut(&id) {
            slot.next_reputation = Some(next);
        }
        Ok(())
    }

    fn reputation_wait(&self, proxy: ProxyId, since: DateTime<Utc>) -> chrono::Duration {
        scale(
            self.settings.reputation_interval,
            self.spread(
                "reputation-wait",
                &[
                    proxy.as_uuid().as_bytes(),
                    &since.timestamp_millis().to_le_bytes(),
                ],
            ),
        )
    }

    /// Runs passes until `shutdown` resolves. The host address is read
    /// before the first probe.
    pub async fn run(self: Arc<Self>, shutdown: impl Future<Output = ()> + Send) {
        tokio::pin!(shutdown);
        loop {
            if let Err(error) = self.tick().await {
                tracing::error!(%error, "proxy health pass failed");
            }
            tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(self.settings.tick) => {}
            }
        }
    }
}

/// A duration times a factor, in whole milliseconds.
fn scale(duration: chrono::Duration, factor: f64) -> chrono::Duration {
    let factor = if factor.is_finite() {
        factor.max(0.0)
    } else {
        1.0
    };
    // Intervals are far below 2^53 milliseconds.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let millis = (duration.num_milliseconds() as f64 * factor) as i64;
    chrono::Duration::milliseconds(millis)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::config::Config;

    use super::{HealthSettings, scale};

    #[test]
    fn durations_scale_by_a_factor() {
        let five_minutes = chrono::Duration::minutes(5);

        assert_eq!(scale(five_minutes, 1.0), five_minutes);
        assert_eq!(scale(five_minutes, 0.75), chrono::Duration::seconds(225));
        assert_eq!(scale(five_minutes, 1.25), chrono::Duration::seconds(375));
        assert_eq!(scale(five_minutes, 0.0), chrono::Duration::zero());
        // A factor that is not a number, or negative, never yields a
        // negative or absurd wait.
        assert_eq!(scale(five_minutes, -3.0), chrono::Duration::zero());
        assert_eq!(scale(five_minutes, f64::NAN), five_minutes);
        assert_eq!(scale(five_minutes, f64::INFINITY), five_minutes);
    }

    #[test]
    fn the_settings_follow_the_configuration() {
        let env = |extra: &[(&str, &str)]| {
            let mut pairs = vec![
                ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
                ("KAGE__SIDECAR__TOKEN", "token"),
                (
                    "KAGE__SECRETS__MASTER_KEY",
                    "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
                ),
            ];
            pairs.extend_from_slice(extra);
            pairs
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect::<Vec<_>>()
        };

        let defaults = HealthSettings::from_config(&Config::load(None, env(&[])).unwrap());
        assert_eq!(defaults.interval, chrono::Duration::minutes(5));
        assert!((defaults.jitter - 0.25).abs() < f64::EPSILON);
        assert_eq!(defaults.max_concurrent, 3);
        assert_eq!(defaults.startup_spread, chrono::Duration::minutes(2));
        assert!(defaults.geo);
        assert_eq!(defaults.geo_interval, chrono::Duration::hours(24));
        assert_eq!(defaults.reputation_interval, chrono::Duration::hours(24));
        assert_eq!(defaults.host_ip_refresh, chrono::Duration::hours(1));
        assert_eq!(defaults.tick, Duration::from_secs(5));

        let tuned = HealthSettings::from_config(
            &Config::load(
                None,
                env(&[
                    ("KAGE__PROXIES__HEALTH__INTERVAL", "2s"),
                    ("KAGE__PROXIES__HEALTH__MAX_CONCURRENT", "7"),
                    ("KAGE__REPUTATION__INTERVAL", "2d"),
                ]),
            )
            .unwrap(),
        );
        assert_eq!(tuned.interval, chrono::Duration::seconds(2));
        // The checker never looks less often than a proxy is due.
        assert_eq!(tuned.tick, Duration::from_secs(2));
        assert_eq!(tuned.max_concurrent, 7);
        assert_eq!(tuned.reputation_interval, chrono::Duration::days(2));
    }
}
