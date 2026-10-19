//! The proxy pool (spec §7.2, §7.3): the proxies Kage knows, kept in step
//! with the provider, moved through the state machine by health checks, and
//! replaced within the monthly budget.
//!
//! The pool keeps its state in Postgres through `kage-store`. Every change
//! to a proxy reads the row, applies a function of [`crate::machine`] and
//! writes it back inside one transaction that holds the row lock, so the
//! server and a CLI command running next to it never overwrite each other.
//! The decision that spends the replacement budget additionally takes one
//! advisory lock, so the budget cannot be overspent by two callers.
//!
//! # The `egress_unsafe` flag: a contract with the scheduler
//!
//! A proxy whose probe sees the host's own address as the exit address has
//! leaked. A leak is a fault on Kage's path to the proxies, not in one
//! address, so one recorded leak makes the whole pool unsafe:
//! [`Pool::egress_unsafe`] returns `true`.
//!
//! **While the flag is set nothing may be fetched.** The scheduler
//! (milestone M7) must refuse to dispatch any request, on any proxy, and
//! answer `provider_error` instead: the host address must never reach a
//! target (decision D17).
//!
//! The flag clears only when a probe through the same proxy shows a
//! distinct exit address again, or when an operator clears it with
//! `kage proxy egress --clear` after fixing the cause. Retiring the proxy
//! does not clear it. The flag lives in the stored health record, so it
//! survives a restart and is the same for every process.
//!
//! # Hooks for later milestones
//!
//! [`Pool::subscribe`] delivers [`PoolEvent`]s. Personas (M6) are generated
//! when a proxy first becomes `healthy`, cool down when it enters `backoff`
//! and retire when it is `retired`; a changed geo record means the
//! persona's timezone and locale no longer match its address.
//! [`Pool::report_traffic_ok`] lets the scheduler (M7) tell the pool that a
//! real request just went through a proxy, which postpones that proxy's
//! next active probe.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, Datelike, TimeZone, Utc};
use kage_core::alert::{Alert, AlertKind, AlertSeverity, AlertSink};
use kage_core::config::{Config, ResolvedProvider};
// `ProxyEndpoint` lives in kage-core so that the http path can take one
// without depending on this crate; it stays reachable under its old path.
pub use kage_core::domain::ProxyEndpoint;
use kage_core::domain::{
    CountryCode, HealthEvent, Proxy, ProxyProviderKind, ProxyScheme, ProxyState, ReplaceReason,
    SubjectKind,
};
use kage_core::ids::{HealthEventId, ProxyId};
use kage_core::seed::Seed;
use kage_store::{Store, StoreError};
use serde_json::json;
use tokio::sync::broadcast;

use crate::clock::{Clock, SystemClock};
use crate::crypto::{CredentialCipher, CredentialField, CryptoError};
use crate::machine::{
    self, Effect, PoolRules, ProbeReport, ReplacementDecision, ReputationReport,
    budget_window_start,
};
use crate::provider::{
    AccountInfo, ProviderError, ProviderProxy, ProxyProvider, ReplaceSpec, ReplaceState,
    ReplaceTicket, ReplacedProxy, StaticListProvider, WebshareProvider,
};

/// A replacement whose request left no trace at the provider is given up
/// after this long, and its place in the budget freed.
const UNTRACED_REPLACEMENT_GRACE: chrono::Duration = chrono::Duration::minutes(5);

/// A replacement still pending at the provider after this long raises an
/// alert. It is not given up: the provider may still finish it.
const SLOW_REPLACEMENT: chrono::Duration = chrono::Duration::hours(1);

/// A replacement in flight is asked about at most this often.
const REPLACEMENT_POLL: chrono::Duration = chrono::Duration::seconds(15);

/// How far back the provider's log of replaced proxies is read when a proxy
/// is missing from its list.
const REPLACED_LOOKBACK: chrono::Duration = chrono::Duration::days(31);

/// The outage guard and the capacity alert only apply to pools of at least
/// this many proxies: in a smaller pool a single proxy is already a large
/// share.
const MIN_POOL_FOR_SHARES: usize = 4;

/// Why a pool operation failed.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The provider failed.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// A credential could not be sealed or opened.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
    /// No proxy provider is configured.
    #[error(
        "no proxy provider is configured: set KAGE__WEBSHARE__API_KEY or KAGE__PROXIES__STATIC_LIST"
    )]
    NoProvider,
    /// There is no such proxy.
    #[error("there is no proxy {0}")]
    NotFound(ProxyId),
    /// The pool could not be built from the configuration.
    #[error("the proxy pool cannot be set up: {0}")]
    Setup(String),
}

/// Something that happened in the pool, for subscribers in other crates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolEvent {
    /// The provider listed a proxy the pool did not know.
    Added {
        /// The new proxy.
        proxy: ProxyId,
    },
    /// A proxy changed state.
    StateChanged {
        /// The proxy.
        proxy: ProxyId,
        /// The state before.
        from: ProxyState,
        /// The state after.
        to: ProxyState,
    },
    /// A geo lookup placed a proxy somewhere else than before, or for the
    /// first time.
    GeoChanged {
        /// The proxy.
        proxy: ProxyId,
    },
    /// The `egress_unsafe` flag was set or cleared.
    EgressUnsafe(bool),
}

/// What one sync with the provider did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    /// Proxies the provider listed.
    pub listed: usize,
    /// Proxies added to the pool.
    pub added: Vec<ProxyId>,
    /// Proxies whose address, credentials or location were updated.
    pub updated: Vec<ProxyId>,
    /// Proxies retired, with the reason.
    pub retired: Vec<(ProxyId, String)>,
    /// Proxies missing from the list but not retired yet.
    pub missing: Vec<ProxyId>,
    /// The list was empty or much shorter than the pool, so nothing was
    /// retired on its word.
    pub suspect: bool,
}

/// What became of a request to replace a proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplacementOutcome {
    /// The provider accepted the request, or may have: without a ticket its
    /// outcome is not known yet and the pool keeps looking.
    Requested {
        /// The provider's handle for the request.
        ticket: Option<String>,
    },
    /// This month's budget is used up.
    BudgetExhausted {
        /// Replacements requested this month.
        used: u32,
        /// The budget.
        budget: u32,
    },
    /// The provider's plan has no replacements left.
    QuotaExhausted,
    /// The provider has no proxy to offer in the same country, and Kage
    /// does not trade the pool's country mix for a faster replacement.
    CountryUnavailable {
        /// The country.
        country: CountryCode,
    },
    /// The provider cannot replace proxies.
    Unsupported,
    /// The proxy is already being replaced or is retired.
    NotEligible {
        /// The state it is in.
        state: ProxyState,
    },
    /// The provider refused the request; the proxy is back in `backoff`.
    Failed {
        /// Why.
        error: ProviderError,
    },
}

/// The pool at a glance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PoolSummary {
    /// Proxies per state, retired ones included.
    pub by_state: BTreeMap<ProxyState, usize>,
    /// Proxies that belong to the pool (every state but `retired`).
    pub live: usize,
    /// Proxies a request may use (`healthy` or `degraded`).
    pub usable: usize,
    /// Whether a leak is on record; see the module documentation.
    pub egress_unsafe: bool,
    /// Replacements requested in the current budget window.
    pub replacements_this_month: u32,
    /// The budget of one window.
    pub replacements_per_month: u32,
    /// Where the current budget window began.
    pub budget_window_start: DateTime<Utc>,
    /// What the window is: the provider's subscription term, or the UTC
    /// calendar month when the account reports none.
    pub budget_window: BudgetWindow,
}

/// What the replacement budget is counted over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetWindow {
    /// The provider's subscription term, which is what the provider counts
    /// its own quota over.
    Term,
    /// The UTC calendar month: the provider reported no term.
    Month,
}

impl BudgetWindow {
    /// The stable lowercase name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Term => "term",
            Self::Month => "month",
        }
    }
}

/// What one maintenance pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TickReport {
    /// Proxies whose backoff ran out.
    pub expired: Vec<ProxyId>,
    /// Proxies the provider finished replacing.
    pub replaced: Vec<ProxyId>,
    /// Proxies whose replacement the provider failed, or lost.
    pub failed: Vec<ProxyId>,
    /// Proxies for which a replacement was requested.
    pub requested: Vec<ProxyId>,
}

/// What the pool is built from.
pub struct PoolParts {
    /// The store.
    pub store: Store,
    /// The provider; `None` when none is configured.
    pub provider: Option<Arc<dyn ProxyProvider>>,
    /// The cipher for credentials at rest.
    pub cipher: CredentialCipher,
    /// The state machine's numbers.
    pub rules: PoolRules,
    /// Syncs in a row a proxy must be missing before it is retired.
    pub missing_syncs: u32,
    /// Where alerts go.
    pub alerts: Arc<dyn AlertSink>,
    /// The source of time.
    pub clock: Arc<dyn Clock>,
    /// Seed of every random choice the pool and its health checks make.
    pub seed: Seed,
}

impl std::fmt::Debug for PoolParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolParts")
            .field("provider", &self.provider)
            .field("rules", &self.rules)
            .field("missing_syncs", &self.missing_syncs)
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
struct Memory {
    /// Syncs in a row each proxy has been missing from the provider's list.
    missing: HashMap<ProxyId, u32>,
    /// The account as last read.
    account: Option<AccountInfo>,
    /// Alerts that were raised and whose cause still stands.
    standing: HashSet<(AlertKind, Option<ProxyId>)>,
    /// When real traffic last went through each proxy.
    traffic: HashMap<ProxyId, DateTime<Utc>>,
    /// When each replacement in flight was last asked about.
    polled: HashMap<ProxyId, DateTime<Utc>>,
    /// The budget window the budget alert was last raised for.
    budget_alert: Option<DateTime<Utc>>,
    /// The account problems last alerted on.
    account_problems: Vec<String>,
    /// The accepted automatic replacement settings last noted as on.
    accepted_auto_replace: Vec<&'static str>,
}

/// The proxy pool. See the module documentation.
pub struct Pool {
    store: Store,
    provider: Option<Arc<dyn ProxyProvider>>,
    cipher: CredentialCipher,
    rules: PoolRules,
    missing_syncs: u32,
    alerts: Arc<dyn AlertSink>,
    clock: Arc<dyn Clock>,
    seed: Seed,
    memory: Mutex<Memory>,
    events: broadcast::Sender<PoolEvent>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("provider", &self.provider)
            .field("rules", &self.rules)
            .field("missing_syncs", &self.missing_syncs)
            .field("seed", &self.seed)
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// A pool from its parts.
    pub fn new(parts: PoolParts) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            store: parts.store,
            provider: parts.provider,
            cipher: parts.cipher,
            rules: parts.rules,
            missing_syncs: parts.missing_syncs.max(1),
            alerts: parts.alerts,
            clock: parts.clock,
            seed: parts.seed,
            memory: Mutex::new(Memory::default()),
            events,
        }
    }

    /// The pool the configuration describes: its provider, its rules, the
    /// system clock and a fresh seed, which is logged.
    ///
    /// # Errors
    ///
    /// Fails when the provider client cannot be built. A configuration that
    /// loaded has a valid master key and static list.
    pub fn from_config(
        config: &Config,
        store: Store,
        alerts: Arc<dyn AlertSink>,
    ) -> Result<Self, PoolError> {
        let provider: Option<Arc<dyn ProxyProvider>> = match config.proxy_provider() {
            ResolvedProvider::Webshare => Some(Arc::new(WebshareProvider::new(&config.webshare)?)),
            ResolvedProvider::Static => Some(Arc::new(
                StaticListProvider::from_config(&config.proxies)
                    .map_err(|error| PoolError::Setup(error.to_string()))?,
            )),
            ResolvedProvider::None => None,
        };
        // A UUIDv7 carries 74 random bits; the seed spreads schedules, it
        // does not protect anything.
        let seed = Seed::derive("proxy-pool", &[uuid::Uuid::now_v7().as_bytes()]);
        tracing::info!(
            provider = config.proxy_provider().as_str(),
            %seed,
            "proxy pool configured"
        );
        Ok(Self::new(PoolParts {
            store,
            provider,
            cipher: CredentialCipher::from_config(&config.secrets)?,
            rules: PoolRules::from_config(config),
            missing_syncs: config.proxies.missing_syncs,
            alerts,
            clock: Arc::new(SystemClock),
            seed,
        }))
    }

    /// The state machine's numbers.
    pub const fn rules(&self) -> &PoolRules {
        &self.rules
    }

    /// The pool's source of time.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The seed of the pool's random choices.
    pub const fn seed(&self) -> &Seed {
        &self.seed
    }

    /// Which provider the pool syncs with, if any.
    pub fn provider_kind(&self) -> Option<ProxyProviderKind> {
        self.provider.as_ref().map(|provider| provider.kind())
    }

    /// Pool events as they happen. A subscriber that falls more than 256
    /// events behind loses the oldest ones; the stored health events are
    /// the complete record.
    pub fn subscribe(&self) -> broadcast::Receiver<PoolEvent> {
        self.events.subscribe()
    }

    fn memory(&self) -> MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn provider(&self) -> Result<&Arc<dyn ProxyProvider>, PoolError> {
        self.provider.as_ref().ok_or(PoolError::NoProvider)
    }

    /// A number in `0..1` that depends only on the pool seed and `parts`,
    /// so a recorded seed replays the same choices.
    pub(crate) fn unit(&self, label: &str, parts: &[&[u8]]) -> f64 {
        let mut all: Vec<&[u8]> = Vec::with_capacity(parts.len() + 1);
        all.push(self.seed.as_bytes());
        all.extend_from_slice(parts);
        Seed::derive(label, &all).rng().unit()
    }

    fn backoff_jitter(&self, proxy: ProxyId, at: DateTime<Utc>) -> f64 {
        self.unit(
            "proxy-backoff",
            &[
                proxy.as_uuid().as_bytes(),
                &at.timestamp_millis().to_le_bytes(),
            ],
        )
    }

    // --- Reading ----------------------------------------------------------

    /// Every proxy, retired ones included, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn proxies(&self) -> Result<Vec<Proxy>, PoolError> {
        Ok(self.store.proxies().list(None).await?)
    }

    /// Every proxy that still belongs to the pool, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn live(&self) -> Result<Vec<Proxy>, PoolError> {
        Ok(self.store.proxies().list_live(None).await?)
    }

    /// One proxy.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn get(&self, id: ProxyId) -> Result<Option<Proxy>, PoolError> {
        Ok(self.store.proxies().get(id).await?)
    }

    /// The proxy's address with its credentials opened.
    ///
    /// # Errors
    ///
    /// Fails when a credential does not open: the master key is not the one
    /// it was sealed with. The next sync with the provider seals the
    /// credentials again.
    pub fn endpoint(&self, proxy: &Proxy) -> Result<ProxyEndpoint, CryptoError> {
        Ok(ProxyEndpoint {
            id: proxy.id,
            host: proxy.host.clone(),
            port: proxy.port,
            username: self
                .cipher
                .open(proxy.id, CredentialField::Username, &proxy.username)?,
            password: self
                .cipher
                .open(proxy.id, CredentialField::Password, &proxy.password)?,
            scheme: proxy.scheme,
        })
    }

    /// Whether a leak is on record. While this is `true` nothing may be
    /// fetched; see the module documentation for the contract.
    ///
    /// # Errors
    ///
    /// Fails when the store fails. A caller that cannot read the flag must
    /// treat it as set.
    pub async fn egress_unsafe(&self) -> Result<bool, PoolError> {
        Ok(self.store.proxies().any_leak().await?)
    }

    /// The account as the provider last reported it.
    pub fn account(&self) -> Option<AccountInfo> {
        self.memory().account.clone()
    }

    /// The pool at a glance.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn summary(&self) -> Result<PoolSummary, PoolError> {
        let proxies = self.proxies().await?;
        let mut by_state: BTreeMap<ProxyState, usize> =
            ProxyState::ALL.iter().map(|state| (*state, 0)).collect();
        for proxy in &proxies {
            *by_state.entry(proxy.state).or_insert(0) += 1;
        }
        let now = self.clock.now();
        let (window, kind) = self.budget_window(now);
        Ok(PoolSummary {
            live: proxies.iter().filter(|proxy| proxy.state.is_live()).count(),
            usable: proxies
                .iter()
                .filter(|proxy| proxy.state.is_usable())
                .count(),
            by_state,
            egress_unsafe: proxies.iter().any(Proxy::leak_detected),
            replacements_this_month: self.store.proxies().replacements_since(window).await?,
            replacements_per_month: self.rules.replacements_per_month,
            budget_window_start: window,
            budget_window: kind,
        })
    }

    /// Where the window of the replacement budget began at `now`, and what
    /// it is. The provider counts its own quota per subscription term, so
    /// the budget follows the term the account last reported (spec §7.2);
    /// before the account was read, or for a provider without terms, it is
    /// the UTC calendar month.
    pub fn budget_window(&self, now: DateTime<Utc>) -> (DateTime<Utc>, BudgetWindow) {
        let term = self
            .memory()
            .account
            .as_ref()
            .and_then(|account| account.term_started_at.zip(account.term_ends_at));
        let start = budget_window_start(now, term);
        let kind = if term.is_some_and(|(from, to)| from < to && from <= now) {
            BudgetWindow::Term
        } else {
            BudgetWindow::Month
        };
        (start, kind)
    }

    /// When the window of [`Pool::budget_window`] ends: the end of the
    /// provider's term, or the start of the next UTC month.
    pub fn budget_window_end(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let (start, kind) = self.budget_window(now);
        let term = self
            .memory()
            .account
            .as_ref()
            .and_then(|account| account.term_started_at.zip(account.term_ends_at));
        if let (BudgetWindow::Term, Some((from, to))) = (kind, term) {
            return start + (to - from);
        }
        // The first day of the month after the one `start` is in.
        let (year, month) = if start.month() == 12 {
            (start.year() + 1, 1)
        } else {
            (start.year(), start.month() + 1)
        };
        Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
            .single()
            .unwrap_or(start + chrono::Duration::days(31))
    }

    /// The bytes the provider counted for the whole account from `since`
    /// on: what it bills, whoever sent them. `None` without a provider, or
    /// when the provider reports no traffic figures.
    ///
    /// # Errors
    ///
    /// Fails when the provider cannot be asked.
    pub async fn provider_traffic_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Option<u64>, PoolError> {
        let Some(provider) = &self.provider else {
            return Ok(None);
        };
        let hours = provider.stats(since).await?;
        if hours.is_empty() {
            return Ok(None);
        }
        Ok(Some(hours.iter().fold(0_u64, |total, hour| {
            total.saturating_add(hour.bytes)
        })))
    }

    // --- Hooks ------------------------------------------------------------

    /// Tells the pool that a real request just succeeded through a proxy.
    /// The health checker postpones that proxy's next active probe: a proxy
    /// that just served a page is known to be reachable. Nothing calls this
    /// before the scheduler exists (milestone M7).
    pub fn report_traffic_ok(&self, proxy: ProxyId) {
        let now = self.clock.now();
        self.memory().traffic.insert(proxy, now);
    }

    /// When real traffic last succeeded through a proxy, as reported with
    /// [`Pool::report_traffic_ok`].
    pub fn last_traffic(&self, proxy: ProxyId) -> Option<DateTime<Utc>> {
        self.memory().traffic.get(&proxy).copied()
    }

    // --- Alerts and events ------------------------------------------------

    /// Raises an alert unless the same one is already standing.
    pub(crate) fn raise_once(&self, alert: Alert) {
        let key = (alert.kind, alert.subject.map(ProxyId::from_uuid));
        if self.memory().standing.insert(key) {
            self.alerts.raise(alert);
        }
    }

    /// Marks an alert's cause as gone, so it is raised again next time.
    /// Returns whether it was standing.
    pub(crate) fn settle(&self, kind: AlertKind, proxy: Option<ProxyId>) -> bool {
        self.memory().standing.remove(&(kind, proxy))
    }

    pub(crate) fn alert(
        &self,
        kind: AlertKind,
        severity: AlertSeverity,
        message: &str,
        details: serde_json::Value,
    ) -> Alert {
        Alert::new(kind, severity, message, details).at(self.clock.now())
    }

    /// A health event about a proxy, or about the pool when `subject` is
    /// `None`, dated now.
    fn health_event(
        &self,
        subject: Option<ProxyId>,
        kind: &str,
        severity: AlertSeverity,
        details: serde_json::Value,
    ) -> HealthEvent {
        HealthEvent {
            id: HealthEventId::new(),
            at: self.clock.now(),
            subject_kind: if subject.is_some() {
                SubjectKind::Proxy
            } else {
                SubjectKind::System
            },
            subject_id: subject.map(|proxy| proxy.as_uuid()),
            kind: kind.to_owned(),
            severity,
            details,
        }
    }

    /// Stores a health event that records nothing the pool wrote at the
    /// same moment. A failure to store it is logged and does not fail the
    /// operation it describes.
    async fn event(
        &self,
        subject: Option<ProxyId>,
        kind: &str,
        severity: AlertSeverity,
        details: serde_json::Value,
    ) {
        let event = self.health_event(subject, kind, severity, details);
        if let Err(error) = self.store.health_events().insert(&event).await {
            tracing::error!(%error, kind, "cannot store the health event");
        }
    }

    fn publish(&self, event: PoolEvent) {
        // No subscriber is not an error.
        let _ = self.events.send(event);
    }

    /// The health events that record what the state machine did to a proxy,
    /// `proxy` being the proxy as it is afterwards. They are written in the
    /// transaction that changes the proxy (`modify_noting`), so that nobody
    /// reads a proxy in a state whose event is not there yet (follow-up
    /// F65); what [`Pool::handle`] does afterwards is only announced.
    fn records(&self, proxy: &Proxy, effects: &[Effect], cause: &str) -> Vec<HealthEvent> {
        let subject = Some(proxy.id);
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::StateChanged { from, to } => Some(self.health_event(
                    subject,
                    "proxy_state_changed",
                    AlertSeverity::Info,
                    json!({
                        "from": from, "to": to, "cause": cause,
                        "consecutive_failures": proxy.consecutive_failures,
                        "backoff_count": proxy.backoff_count,
                        "backoff_until": proxy.backoff_until,
                    }),
                )),
                Effect::GeoObserved { current } => Some(self.health_event(
                    subject,
                    "proxy_geo_observed",
                    AlertSeverity::Info,
                    json!({ "current": current, "provider_country": proxy.country }),
                )),
                Effect::GeoChanged { previous, current } => Some(self.health_event(
                    subject,
                    "proxy_geo_changed",
                    AlertSeverity::Warning,
                    json!({ "previous": previous, "current": current }),
                )),
                Effect::GeoMismatchCleared => Some(self.health_event(
                    subject,
                    "proxy_geo_mismatch_cleared",
                    AlertSeverity::Info,
                    json!({}),
                )),
                Effect::ReputationRecovered => Some(self.health_event(
                    subject,
                    "proxy_reputation_recovered",
                    AlertSeverity::Info,
                    json!({}),
                )),
                Effect::Leak { .. }
                | Effect::LeakCleared
                | Effect::GeoMismatch { .. }
                | Effect::ReputationBad { .. }
                | Effect::UsageTypeSuspicious { .. } => None,
            })
            .collect()
    }

    /// Announces what the state machine reported, once the proxy and the
    /// events of [`Pool::records`] are stored: the log, the alerts and the
    /// published events.
    // One arm per effect; splitting it would only scatter the list.
    #[allow(clippy::too_many_lines)]
    async fn handle(&self, proxy: &Proxy, effects: Vec<Effect>, cause: &str) {
        for effect in effects {
            match effect {
                Effect::StateChanged { from, to } => {
                    tracing::info!(
                        proxy = %proxy.id,
                        from = from.as_str(),
                        to = to.as_str(),
                        cause,
                        "proxy changed state"
                    );
                    self.publish(PoolEvent::StateChanged {
                        proxy: proxy.id,
                        from,
                        to,
                    });
                }
                Effect::Leak { exit_ip } => {
                    self.raise_once(
                        self.alert(
                            AlertKind::ProxyLeak,
                            AlertSeverity::Critical,
                            "a proxy let the host's own address through; it is parked in backoff",
                            json!({ "host": proxy.host, "port": proxy.port, "exit_ip": exit_ip }),
                        )
                        .about(proxy.id),
                    );
                    self.raise_once(self.alert(
                        AlertKind::EgressUnsafe,
                        AlertSeverity::Critical,
                        "egress is unsafe: a proxy leaked the host address, so nothing may be \
                         fetched until a probe through it shows a distinct exit address or an \
                         operator clears the flag",
                        json!({ "proxy": proxy.id }),
                    ));
                    self.publish(PoolEvent::EgressUnsafe(true));
                }
                Effect::LeakCleared => self.leak_cleared(proxy.id, "probe").await,
                Effect::GeoObserved { .. } => {
                    self.publish(PoolEvent::GeoChanged { proxy: proxy.id });
                }
                Effect::GeoChanged { .. } => {
                    tracing::warn!(proxy = %proxy.id, "the geo record of a proxy changed");
                    self.publish(PoolEvent::GeoChanged { proxy: proxy.id });
                }
                Effect::GeoMismatch { provider, observed } => self.raise_once(
                    self.alert(
                        AlertKind::ProxyGeoMismatch,
                        AlertSeverity::Warning,
                        "a proxy's exit address is in another country than the provider states",
                        json!({ "provider": provider, "observed": observed, "host": proxy.host }),
                    )
                    .about(proxy.id),
                ),
                Effect::GeoMismatchCleared => {
                    self.settle(AlertKind::ProxyGeoMismatch, Some(proxy.id));
                }
                Effect::ReputationBad { score } => self.raise_once(
                    self.alert(
                        AlertKind::ProxyReputationBad,
                        AlertSeverity::Warning,
                        "the reputation provider scores a proxy's address as bad",
                        json!({ "score": score, "bad_score": self.rules.bad_score }),
                    )
                    .about(proxy.id),
                ),
                Effect::ReputationRecovered => {
                    self.settle(AlertKind::ProxyReputationBad, Some(proxy.id));
                }
                Effect::UsageTypeSuspicious { usage_type } => {
                    // Raised on every change into a suspicious type.
                    self.settle(AlertKind::ReputationUsageTypeSuspicious, Some(proxy.id));
                    self.raise_once(
                        self.alert(
                            AlertKind::ReputationUsageTypeSuspicious,
                            AlertSeverity::Warning,
                            "the reputation provider classifies a residential proxy's address as \
                             data center or hosting space; consider `kage proxy replace`",
                            json!({ "usage_type": usage_type, "host": proxy.host }),
                        )
                        .about(proxy.id),
                    );
                }
            }
        }
    }

    /// A leak record is gone; if it was the last one the flag is cleared.
    async fn leak_cleared(&self, proxy: ProxyId, by: &str) {
        self.settle(AlertKind::ProxyLeak, Some(proxy));
        match self.store.proxies().any_leak().await {
            Ok(false) => {
                self.settle(AlertKind::EgressUnsafe, None);
                tracing::warn!(%proxy, by, "egress is safe again: no leak is on record");
                self.event(
                    None,
                    "egress_safe",
                    AlertSeverity::Warning,
                    json!({ "proxy": proxy, "cleared_by": by }),
                )
                .await;
                self.publish(PoolEvent::EgressUnsafe(false));
            }
            Ok(true) => {}
            Err(error) => tracing::error!(%error, "cannot read the egress flag"),
        }
    }

    /// An operator declares every recorded leak dealt with. Returns the
    /// proxies whose leak record was cleared. The proxies stay parked until
    /// their next probe, which decides again.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn clear_egress_unsafe(&self) -> Result<Vec<ProxyId>, PoolError> {
        let mut cleared = Vec::new();
        for proxy in self.proxies().await? {
            if !proxy.leak_detected() {
                continue;
            }
            let changed = self
                .store
                .proxies()
                .modify(proxy.id, machine::clear_leak)
                .await?;
            if changed == Some(true) {
                cleared.push(proxy.id);
                self.leak_cleared(proxy.id, "operator").await;
            }
        }
        Ok(cleared)
    }

    // --- Health -------------------------------------------------------------

    /// Whether more than half of the pool is failing right now, counting
    /// `failing` as failed whatever its record says. The cause is then on
    /// the path (the host's network, the provider's gateway, the echo), not
    /// in the addresses.
    fn outage_suspected(live: &[Proxy], failing: Option<ProxyId>) -> bool {
        let probed: Vec<&Proxy> = live
            .iter()
            .filter(|proxy| {
                matches!(
                    proxy.state,
                    ProxyState::New
                        | ProxyState::Healthy
                        | ProxyState::Degraded
                        | ProxyState::Backoff
                )
            })
            .collect();
        if probed.len() < MIN_POOL_FOR_SHARES {
            return false;
        }
        let down = probed
            .iter()
            .filter(|proxy| {
                failing == Some(proxy.id)
                    || proxy
                        .last_health
                        .as_ref()
                        .is_some_and(|health| !health.reachable)
            })
            .count();
        down * 2 > probed.len()
    }

    /// Applies one probe to a proxy and stores the result. Returns the
    /// proxy as it is afterwards, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Fails when the store fails; nothing is changed in that case.
    pub async fn record_probe(
        &self,
        id: ProxyId,
        report: &ProbeReport,
    ) -> Result<Option<Proxy>, PoolError> {
        let live = self.store.proxies().list_live(None).await?;
        let failed = report.exit.is_err();
        let outage = failed && Self::outage_suspected(&live, Some(id));
        let jitter = self.backoff_jitter(id, report.at);
        let rules = self.rules;
        let cause = match (&report.exit, report.leak) {
            (Ok(_), Some(true)) => "leak",
            (Ok(_), _) => "probe_ok",
            (Err(failure), _) => failure.as_str(),
        };
        let Some((proxy, effects)) = self
            .store
            .proxies()
            .modify_noting(id, |proxy| {
                let effects = machine::apply_probe(proxy, report, outage, jitter, &rules);
                let records = self.records(proxy, &effects, cause);
                ((proxy.clone(), effects), records)
            })
            .await?
        else {
            return Ok(None);
        };

        if outage {
            self.raise_once(self.alert(
                AlertKind::PoolOutageSuspected,
                AlertSeverity::Critical,
                "more than half of the proxy pool is failing at once; failure counters are \
                 frozen because the cause is on the path, not in the addresses",
                json!({ "live": live.len() }),
            ));
        } else if !failed {
            // This proxy answers: see whether the outage is over.
            let rest: Vec<Proxy> = live.into_iter().filter(|other| other.id != id).collect();
            let mut now_live = rest;
            now_live.push(proxy.clone());
            if !Self::outage_suspected(&now_live, None)
                && self.settle(AlertKind::PoolOutageSuspected, None)
            {
                self.event(None, "pool_outage_cleared", AlertSeverity::Info, json!({}))
                    .await;
            }
        }
        self.handle(&proxy, effects, cause).await;
        Ok(Some(proxy))
    }

    /// Applies one reputation lookup to a proxy and stores the result.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn record_reputation(
        &self,
        id: ProxyId,
        report: &ReputationReport,
    ) -> Result<Option<Proxy>, PoolError> {
        let rules = self.rules;
        let Some((proxy, effects)) = self
            .store
            .proxies()
            .modify_noting(id, |proxy| {
                let effects = machine::apply_reputation(proxy, report, &rules);
                let records = self.records(proxy, &effects, "reputation");
                ((proxy.clone(), effects), records)
            })
            .await?
        else {
            return Ok(None);
        };
        self.handle(&proxy, effects, "reputation").await;
        Ok(Some(proxy))
    }

    /// Ends a proxy's backoff now, whatever time it has left, so that it
    /// can be probed at once. For an operator's `kage proxy health`.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn end_backoff(&self, id: ProxyId) -> Result<Option<Proxy>, PoolError> {
        self.expire(id, true).await
    }

    async fn expire(&self, id: ProxyId, force: bool) -> Result<Option<Proxy>, PoolError> {
        let now = self.clock.now();
        let cause = if force { "operator" } else { "backoff_expired" };
        let Some((proxy, effects)) = self
            .store
            .proxies()
            .modify_noting(id, |proxy| {
                let effects = machine::expire_backoff(proxy, now, force);
                let records = self.records(proxy, &effects, cause);
                ((proxy.clone(), effects), records)
            })
            .await?
        else {
            return Ok(None);
        };
        self.handle(&proxy, effects, cause).await;
        Ok(Some(proxy))
    }

    // --- Sync with the provider ---------------------------------------------

    /// Brings the pool in step with the provider's list (spec §7.2): new
    /// addresses are added as `new`, changed credentials and locations are
    /// taken over, and addresses the provider no longer lists are retired,
    /// carefully. A proxy is retired at once only when the provider's own
    /// log says it was replaced; otherwise only after it has been missing
    /// from `missing_syncs` lists in a row; and never on the word of a list
    /// that is empty or shorter than half the pool.
    ///
    /// Without a provider this does nothing.
    ///
    /// # Errors
    ///
    /// Fails when the provider or the store fails; an alert is raised for
    /// a provider failure.
    // The steps of one sync, in the order they must happen.
    #[allow(clippy::too_many_lines)]
    pub async fn reconcile(&self) -> Result<ReconcileReport, PoolError> {
        let Some(provider) = &self.provider else {
            return Ok(ReconcileReport::default());
        };
        let kind = provider.kind();
        let listed = match provider.list().await {
            Ok(listed) => {
                self.settle(AlertKind::ProviderSyncFailed, None);
                listed
            }
            Err(error) => {
                self.raise_once(self.alert(
                    AlertKind::ProviderSyncFailed,
                    AlertSeverity::Warning,
                    "the proxy provider's list could not be read; the pool keeps what it has",
                    json!({ "error": error.to_string() }),
                ));
                return Err(error.into());
            }
        };
        let now = self.clock.now();
        let live = self.store.proxies().list_live(Some(kind)).await?;
        let mut report = ReconcileReport {
            listed: listed.len(),
            ..ReconcileReport::default()
        };

        for item in &listed {
            match live
                .iter()
                .find(|proxy| proxy.provider_ref == item.provider_ref)
            {
                Some(known) if known.host == item.host && known.port == item.port => {
                    if self.take_over(known, item).await? {
                        report.updated.push(known.id);
                    }
                }
                Some(known) => {
                    // The provider gave the id to another address: to the
                    // pool that is one proxy gone and another one arrived.
                    self.retire(known, now, "address_changed", &mut report)
                        .await?;
                    self.adopt(kind, item, now, &mut report).await?;
                }
                None => self.adopt(kind, item, now, &mut report).await?,
            }
        }

        let gone: Vec<&Proxy> = live
            .iter()
            .filter(|proxy| {
                !listed
                    .iter()
                    .any(|item| item.provider_ref == proxy.provider_ref)
            })
            .collect();
        report.suspect = !live.is_empty() && (listed.is_empty() || listed.len() * 2 < live.len());
        if report.suspect {
            self.raise_once(self.alert(
                AlertKind::ProviderListSuspect,
                AlertSeverity::Critical,
                "the provider's proxy list is empty or shorter than half the pool; nothing was \
                 retired on its word",
                json!({ "listed": listed.len(), "known": live.len() }),
            ));
            report.missing = gone.iter().map(|proxy| proxy.id).collect();
            return Ok(report);
        }
        self.settle(AlertKind::ProviderListSuspect, None);

        let replaced = if gone.is_empty() {
            Vec::new()
        } else {
            match provider.replaced(now - REPLACED_LOOKBACK).await {
                Ok(replaced) => replaced,
                Err(error) => {
                    tracing::warn!(%error, "cannot read the provider's log of replaced proxies");
                    Vec::new()
                }
            }
        };
        for proxy in &gone {
            let confirmed = replaced.iter().find(|entry| replaced_this(entry, proxy));
            let missed = {
                let mut memory = self.memory();
                let count = memory.missing.entry(proxy.id).or_insert(0);
                *count += 1;
                *count
            };
            if let Some(entry) = confirmed {
                if proxy.state != ProxyState::Replacing {
                    // Not a replacement Kage asked for.
                    let details = json!({
                        "reason": entry.reason, "host": proxy.host,
                        "replaced_with": entry.new_host, "at": entry.at,
                    });
                    if self
                        .rules
                        .accepted_auto_replace
                        .covers_reason(&entry.reason)
                    {
                        // The operator accepted this setting (decision D31):
                        // the swap is noted, and handled like any other.
                        tracing::info!(
                            proxy = %proxy.id, reason = %entry.reason,
                            "the provider replaced a proxy by itself, as accepted"
                        );
                        self.event(
                            Some(proxy.id),
                            "provider_replaced_proxy",
                            AlertSeverity::Info,
                            details,
                        )
                        .await;
                    } else {
                        self.alerts.raise(
                            self.alert(
                                AlertKind::ProviderReplacedProxy,
                                AlertSeverity::Warning,
                                "the provider replaced a proxy by itself",
                                details,
                            )
                            .about(proxy.id),
                        );
                    }
                }
                self.retire(proxy, now, &entry.reason, &mut report).await?;
            } else if missed >= self.missing_syncs {
                self.retire(proxy, now, "missing_from_provider", &mut report)
                    .await?;
            } else {
                report.missing.push(proxy.id);
            }
        }
        // A proxy that is listed again starts from zero.
        self.memory()
            .missing
            .retain(|id, _| report.missing.contains(id));
        Ok(report)
    }

    /// Adds a proxy the provider lists and the pool does not know.
    async fn adopt(
        &self,
        kind: ProxyProviderKind,
        item: &ProviderProxy,
        now: DateTime<Utc>,
        report: &mut ReconcileReport,
    ) -> Result<(), PoolError> {
        let id = ProxyId::new();
        let proxy = Proxy {
            id,
            provider: kind,
            provider_ref: item.provider_ref.clone(),
            host: item.host.clone(),
            port: item.port,
            username: self
                .cipher
                .seal(id, CredentialField::Username, item.username.expose())?,
            password: self
                .cipher
                .seal(id, CredentialField::Password, item.password.expose())?,
            scheme: ProxyScheme::Http,
            country: item.country,
            city: item.city.clone(),
            asn: item.asn.clone(),
            state: ProxyState::New,
            consecutive_failures: 0,
            backoff_count: 0,
            backoff_until: None,
            failing_since: None,
            replacement: None,
            last_health: None,
            created_at: now,
            retired_at: None,
        };
        // The proxy and the event of its arrival are one write: a probe that
        // finds the proxy finds the event too.
        let added = self.health_event(
            Some(id),
            "proxy_added",
            AlertSeverity::Info,
            json!({
                "provider": kind, "provider_ref": proxy.provider_ref, "host": proxy.host,
                "port": proxy.port, "country": proxy.country, "city": proxy.city,
            }),
        );
        match self.store.proxies().insert_noting(&proxy, &[added]).await {
            Ok(()) => {}
            // Another process adopted it a moment ago.
            Err(error) if error.is_conflict() => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        tracing::info!(
            proxy = %id, host = %proxy.host, port = proxy.port,
            country = proxy.country.map(|country| country.to_string()),
            "proxy added to the pool"
        );
        self.publish(PoolEvent::Added { proxy: id });
        report.added.push(id);
        Ok(())
    }

    /// Takes over credentials and location the provider changed. Returns
    /// whether anything changed.
    async fn take_over(&self, known: &Proxy, item: &ProviderProxy) -> Result<bool, PoolError> {
        let mut changed = Vec::new();
        // Credentials that no longer open (the master key was replaced) are
        // sealed again like changed ones.
        let stored = self.endpoint(known).ok();
        let same_login = stored.as_ref().is_some_and(|stored| {
            stored.username.expose() == item.username.expose()
                && stored.password.expose() == item.password.expose()
        });
        if !same_login {
            changed.push("credentials");
        }
        if known.country != item.country {
            changed.push("country");
        }
        if known.city != item.city {
            changed.push("city");
        }
        // An observed network wins over the provider's word.
        let fill_asn = known.asn.is_none() && item.asn.is_some();
        if fill_asn {
            changed.push("asn");
        }
        if changed.is_empty() {
            return Ok(false);
        }

        let username =
            self.cipher
                .seal(known.id, CredentialField::Username, item.username.expose())?;
        let password =
            self.cipher
                .seal(known.id, CredentialField::Password, item.password.expose())?;
        self.store
            .proxies()
            .modify(known.id, |proxy| {
                if !same_login {
                    proxy.username = username;
                    proxy.password = password;
                }
                proxy.country = item.country;
                proxy.city.clone_from(&item.city);
                if proxy.asn.is_none() {
                    proxy.asn.clone_from(&item.asn);
                }
            })
            .await?;
        self.event(
            Some(known.id),
            "proxy_updated",
            AlertSeverity::Info,
            json!({ "changed": changed }),
        )
        .await;
        Ok(true)
    }

    async fn retire(
        &self,
        proxy: &Proxy,
        now: DateTime<Utc>,
        reason: &str,
        report: &mut ReconcileReport,
    ) -> Result<(), PoolError> {
        let from = proxy.state;
        let retired = self
            .store
            .proxies()
            .modify_noting(proxy.id, |stored| {
                if !machine::retire(stored, now) {
                    return (false, Vec::new());
                }
                let event = self.health_event(
                    Some(proxy.id),
                    "proxy_retired",
                    AlertSeverity::Warning,
                    json!({ "reason": reason, "from": from, "host": proxy.host, "port": proxy.port }),
                );
                (true, vec![event])
            })
            .await?;
        if retired != Some(true) {
            return Ok(());
        }
        tracing::warn!(proxy = %proxy.id, host = %proxy.host, reason, "proxy retired");
        self.publish(PoolEvent::StateChanged {
            proxy: proxy.id,
            from,
            to: ProxyState::Retired,
        });
        {
            let mut memory = self.memory();
            memory.missing.remove(&proxy.id);
            memory.traffic.remove(&proxy.id);
            memory.polled.remove(&proxy.id);
            memory
                .standing
                .retain(|(_, subject)| *subject != Some(proxy.id));
        }
        report.retired.push((proxy.id, reason.to_owned()));
        Ok(())
    }

    /// Reads the provider account and alerts on what needs attention:
    /// automatic replacement switched on, a paused or throttled
    /// subscription, no replacements left. Kage reports these settings; it
    /// never changes them.
    ///
    /// # Errors
    ///
    /// Fails when there is no provider or it fails.
    pub async fn sync_account(&self) -> Result<AccountInfo, PoolError> {
        let account = self.provider()?.account().await?;
        let previous = self.memory().account.replace(account.clone());
        if previous.as_ref() != Some(&account) {
            self.event(
                None,
                "provider_account",
                AlertSeverity::Info,
                serde_json::to_value(&account).unwrap_or_default(),
            )
            .await;
        }

        // A setting the operator accepted (`webshare.accepted_auto_replace`)
        // is noted when it is found on; any other one is an alert.
        let on = account.auto_replace.unwrap_or_default();
        let accepted = on.both(&self.rules.accepted_auto_replace).enabled();
        let unexpected = on.without(&self.rules.accepted_auto_replace).enabled();
        let newly_noted = {
            let mut memory = self.memory();
            let changed = memory.accepted_auto_replace != accepted;
            memory.accepted_auto_replace.clone_from(&accepted);
            changed && !accepted.is_empty()
        };
        if newly_noted {
            tracing::info!(
                enabled = ?accepted,
                "the provider account replaces proxies automatically, as accepted in \
                 webshare.accepted_auto_replace"
            );
            self.event(
                None,
                "provider_auto_replace_accepted",
                AlertSeverity::Info,
                json!({ "enabled": accepted }),
            )
            .await;
        }
        if unexpected.is_empty() {
            self.settle(AlertKind::ProviderAutoReplaceEnabled, None);
        } else {
            self.raise_once(self.alert(
                AlertKind::ProviderAutoReplaceEnabled,
                AlertSeverity::Warning,
                "the provider account replaces proxies automatically, so an address and its \
                 persona can change without Kage deciding it; switch these settings off in \
                 the provider's dashboard, or name them in webshare.accepted_auto_replace",
                json!({ "enabled": unexpected, "accepted": accepted }),
            ));
        }

        let mut problems = Vec::new();
        if account.paused == Some(true) {
            problems.push("the subscription is paused".to_owned());
        }
        if account.throttled == Some(true) {
            problems.push("the account is throttled".to_owned());
        }
        if account.replacements_available == Some(0) && account.replacements_total != Some(0) {
            problems.push("the plan has no proxy replacements left".to_owned());
        }
        let changed = {
            let mut memory = self.memory();
            let changed = memory.account_problems != problems;
            memory.account_problems.clone_from(&problems);
            changed
        };
        if changed && !problems.is_empty() {
            self.alerts.raise(self.alert(
                AlertKind::ProviderAccount,
                AlertSeverity::Warning,
                "the proxy provider account needs attention",
                json!({ "problems": problems }),
            ));
        }
        Ok(account)
    }

    // --- Replacement --------------------------------------------------------

    /// Asks the provider to replace a proxy, if the monthly budget, the
    /// provider's own quota and its stock in the same country allow it.
    ///
    /// # Errors
    ///
    /// Fails when there is no provider, no such proxy, or the store fails.
    /// A refusal by the budget or by the provider is an outcome, not an
    /// error.
    // Every way a request can end, in one place.
    #[allow(clippy::too_many_lines)]
    pub async fn request_replacement(
        &self,
        id: ProxyId,
        reason: ReplaceReason,
    ) -> Result<ReplacementOutcome, PoolError> {
        let provider = self.provider()?;
        let proxy = self
            .store
            .proxies()
            .get(id)
            .await?
            .ok_or(PoolError::NotFound(id))?;
        if provider.kind() == ProxyProviderKind::Static {
            return Ok(ReplacementOutcome::Unsupported);
        }
        let account = self.account();
        // The provider's stock is counted by the country it states.
        let country = proxy.country.or_else(|| proxy.effective_country());
        if let (Some(country), Some(stock)) = (
            country,
            account
                .as_ref()
                .and_then(|account| account.available_countries.as_ref()),
        ) && stock.get(country.as_str()).copied().unwrap_or(0) == 0
            && matches!(
                proxy.state,
                ProxyState::New | ProxyState::Healthy | ProxyState::Degraded | ProxyState::Backoff
            )
        {
            self.raise_once(
                self.alert(
                    AlertKind::ReplacementFailed,
                    AlertSeverity::Warning,
                    "a proxy qualifies for replacement but the provider has none to offer in \
                     the same country; no request was sent",
                    json!({ "country": country, "reason": reason }),
                )
                .about(id),
            );
            return Ok(ReplacementOutcome::CountryUnavailable { country });
        }

        let now = self.clock.now();
        let rules = self.rules;
        let available = account.and_then(|account| account.replacements_available);
        let (window, _) = self.budget_window(now);
        let decision = self
            .store
            .proxies()
            .modify_counting_replacements(id, window, |stored, used| {
                // The pool decided on what it read a moment ago; a probe may
                // have passed since. Only an operator's request is taken
                // whatever the proxy's health.
                if reason != ReplaceReason::Operator
                    && machine::replacement_due(stored, now, &rules) != Some(reason)
                {
                    let state = stored.state;
                    return (ReplacementDecision::NotEligible { state }, Vec::new());
                }
                let decision =
                    machine::start_replacement(stored, reason, used, available, now, &rules);
                // The request is on record with the state it put the proxy in.
                let events = if decision == ReplacementDecision::Started {
                    vec![self.health_event(
                        Some(id),
                        "proxy_replacement_requested",
                        AlertSeverity::Warning,
                        json!({
                            "reason": reason, "from": proxy.state, "host": proxy.host,
                            "country": country,
                        }),
                    )]
                } else {
                    Vec::new()
                };
                (decision, events)
            })
            .await?
            .ok_or(PoolError::NotFound(id))?;

        match decision {
            ReplacementDecision::Started => {}
            ReplacementDecision::BudgetExhausted { used, budget } => {
                self.budget_exhausted(id, reason, used, budget, now);
                return Ok(ReplacementOutcome::BudgetExhausted { used, budget });
            }
            ReplacementDecision::QuotaExhausted => {
                self.quota_exhausted(id, reason);
                return Ok(ReplacementOutcome::QuotaExhausted);
            }
            ReplacementDecision::NotEligible { state } => {
                return Ok(ReplacementOutcome::NotEligible { state });
            }
        }

        metrics::counter!("kage_replacements_total", "reason" => reason.as_str()).increment(1);
        tracing::warn!(
            proxy = %id, host = %proxy.host, reason = reason.as_str(),
            "requesting a proxy replacement"
        );
        self.publish(PoolEvent::StateChanged {
            proxy: id,
            from: proxy.state,
            to: ProxyState::Replacing,
        });

        match provider.replace(&proxy, ReplaceSpec { country }).await {
            Ok(ticket) => {
                self.store
                    .proxies()
                    .modify(id, |proxy| {
                        if let Some(replacement) = proxy.replacement.as_mut() {
                            replacement.ticket = Some(ticket.0.clone());
                        }
                    })
                    .await?;
                Ok(ReplacementOutcome::Requested {
                    ticket: Some(ticket.0),
                })
            }
            // The request may have reached the provider. The proxy stays
            // `replacing`; the maintenance pass finds out what became of it.
            Err(ProviderError::OutcomeUnknown(detail)) => {
                tracing::warn!(proxy = %id, detail, "the outcome of the replacement request is unknown");
                Ok(ReplacementOutcome::Requested { ticket: None })
            }
            Err(error) => {
                self.give_up(&proxy, &error.to_string(), now).await?;
                if error == ProviderError::QuotaExhausted {
                    self.quota_exhausted(id, reason);
                }
                Ok(ReplacementOutcome::Failed { error })
            }
        }
    }

    fn budget_exhausted(
        &self,
        id: ProxyId,
        reason: ReplaceReason,
        used: u32,
        budget: u32,
        now: DateTime<Utc>,
    ) {
        // Once per budget window, however many proxies wait.
        let (window, kind) = self.budget_window(now);
        if self.memory().budget_alert.replace(window) == Some(window) {
            return;
        }
        self.alerts.raise(
            self.alert(
                AlertKind::ReplacementBudgetExhausted,
                AlertSeverity::Critical,
                "a proxy qualifies for replacement but the replacement budget of this \
                 subscription term or month is used up; it stays where it is and recovers by \
                 itself if the address comes back",
                json!({
                    "used": used, "budget": budget, "reason": reason,
                    "window_start": window, "window": kind,
                }),
            )
            .about(id),
        );
    }

    fn quota_exhausted(&self, id: ProxyId, reason: ReplaceReason) {
        self.raise_once(
            self.alert(
                AlertKind::ReplacementBudgetExhausted,
                AlertSeverity::Critical,
                "a proxy qualifies for replacement but the provider's plan has no replacements \
                 left",
                json!({ "reason": reason, "limit": "provider_quota" }),
            )
            .about(id),
        );
    }

    /// A replacement did not happen: back to `backoff`, slot freed, alert.
    async fn give_up(&self, proxy: &Proxy, why: &str, now: DateTime<Utc>) -> Result<(), PoolError> {
        let jitter = self.backoff_jitter(proxy.id, now);
        let rules = self.rules;
        self.store
            .proxies()
            .modify(proxy.id, |proxy| {
                machine::fail_replacement(proxy, now, jitter, &rules)
            })
            .await?;
        self.memory().polled.remove(&proxy.id);
        self.alerts.raise(
            self.alert(
                AlertKind::ReplacementFailed,
                AlertSeverity::Warning,
                "a proxy replacement did not happen; the proxy is back in backoff and its place \
                 in the budget is free again",
                json!({ "error": why, "host": proxy.host }),
            )
            .about(proxy.id),
        );
        self.publish(PoolEvent::StateChanged {
            proxy: proxy.id,
            from: ProxyState::Replacing,
            to: ProxyState::Backoff,
        });
        Ok(())
    }

    /// Finds out what became of a replacement in flight.
    // Every way a replacement can end, in one place.
    #[allow(clippy::too_many_lines)]
    async fn follow_replacement(
        &self,
        proxy: &Proxy,
        now: DateTime<Utc>,
        report: &mut TickReport,
    ) -> Result<(), PoolError> {
        let provider = self.provider()?;
        let Some(replacement) = &proxy.replacement else {
            // `replacing` without a record cannot be followed.
            self.give_up(proxy, "the replacement record is missing", now)
                .await?;
            report.failed.push(proxy.id);
            return Ok(());
        };
        {
            let mut memory = self.memory();
            if memory
                .polled
                .get(&proxy.id)
                .is_some_and(|last| now.signed_duration_since(*last) < REPLACEMENT_POLL)
            {
                return Ok(());
            }
            memory.polled.insert(proxy.id, now);
        }
        let age = now.signed_duration_since(replacement.requested_at);

        let state = match &replacement.ticket {
            Some(ticket) => match provider
                .replace_status(&ReplaceTicket(ticket.clone()))
                .await
            {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!(proxy = %proxy.id, %error, "cannot read the replacement status");
                    return Ok(());
                }
            },
            // The request's outcome was unknown, or the process stopped
            // before the ticket was stored: the provider's log tells.
            None => match provider
                .replaced(replacement.requested_at - chrono::Duration::minutes(1))
                .await
            {
                Ok(replaced) if replaced.iter().any(|entry| replaced_this(entry, proxy)) => {
                    ReplaceState::Done {
                        removed: 1,
                        added: 1,
                    }
                }
                Ok(_) if age >= UNTRACED_REPLACEMENT_GRACE => ReplaceState::Failed {
                    code: None,
                    message: "the request left no trace at the provider".to_owned(),
                },
                Ok(_) => ReplaceState::Pending,
                Err(error) => {
                    tracing::warn!(proxy = %proxy.id, %error, "cannot read the provider's log of replaced proxies");
                    return Ok(());
                }
            },
        };

        match state {
            ReplaceState::Pending => {
                if age >= SLOW_REPLACEMENT {
                    self.raise_once(
                        self.alert(
                            AlertKind::ReplacementFailed,
                            AlertSeverity::Warning,
                            "a proxy replacement has been pending at the provider for over an hour",
                            json!({ "requested_at": replacement.requested_at, "ticket": replacement.ticket }),
                        )
                        .about(proxy.id),
                    );
                }
            }
            ReplaceState::Done { removed, added } => {
                let done = self
                    .store
                    .proxies()
                    .modify_noting(proxy.id, |stored| {
                        if !machine::finish_replacement(stored, now) {
                            return (false, Vec::new());
                        }
                        let event = self.health_event(
                            Some(proxy.id),
                            "proxy_replaced",
                            AlertSeverity::Warning,
                            json!({
                                "reason": replacement.reason, "host": proxy.host,
                                "removed": removed, "added": added, "ticket": replacement.ticket,
                            }),
                        );
                        (true, vec![event])
                    })
                    .await?;
                if done == Some(true) {
                    tracing::warn!(proxy = %proxy.id, host = %proxy.host, "proxy replaced by the provider");
                    self.publish(PoolEvent::StateChanged {
                        proxy: proxy.id,
                        from: ProxyState::Replacing,
                        to: ProxyState::Retired,
                    });
                    self.memory()
                        .standing
                        .retain(|(_, subject)| *subject != Some(proxy.id));
                    report.replaced.push(proxy.id);
                }
                self.memory().polled.remove(&proxy.id);
            }
            ReplaceState::Failed { code, message } => {
                let why = code.map_or_else(|| message.clone(), |code| format!("{code}: {message}"));
                self.give_up(proxy, &why, now).await?;
                report.failed.push(proxy.id);
            }
        }
        Ok(())
    }

    // --- Maintenance --------------------------------------------------------

    /// One maintenance pass: ends backoffs that ran out, follows
    /// replacements in flight, requests the replacements that are due (the
    /// longest-failing proxy first, one at a time), and checks the pool's
    /// capacity. When a replacement finished, the pool is reconciled so the
    /// new address is adopted at once.
    ///
    /// # Errors
    ///
    /// Fails when the store fails. Provider failures are logged and tried
    /// again on the next pass.
    pub async fn tick(&self) -> Result<TickReport, PoolError> {
        let now = self.clock.now();
        let mut report = TickReport::default();
        let mut live = self.store.proxies().list_live(None).await?;

        for proxy in &live {
            if proxy.state == ProxyState::Backoff
                && proxy.backoff_until.is_none_or(|until| until <= now)
                && self
                    .expire(proxy.id, false)
                    .await?
                    .is_some_and(|after| after.state == ProxyState::Degraded)
            {
                report.expired.push(proxy.id);
            }
        }
        // A proxy whose backoff just ran out is owed a probe before
        // anything else is decided about it.
        if !report.expired.is_empty() {
            live = self.store.proxies().list_live(None).await?;
        }

        let mut in_flight = false;
        if self.provider.is_some() {
            for proxy in live
                .iter()
                .filter(|proxy| proxy.state == ProxyState::Replacing)
            {
                self.follow_replacement(proxy, now, &mut report).await?;
            }
            in_flight = live.iter().any(|proxy| {
                proxy.state == ProxyState::Replacing
                    && !report.replaced.contains(&proxy.id)
                    && !report.failed.contains(&proxy.id)
            });
        }

        // The provider takes one replacement at a time.
        if self.provider.is_some() && !in_flight {
            let mut due: Vec<(&Proxy, ReplaceReason)> = live
                .iter()
                .filter(|proxy| !report.failed.contains(&proxy.id))
                .filter_map(|proxy| {
                    machine::replacement_due(proxy, now, &self.rules).map(|reason| (proxy, reason))
                })
                .collect();
            // Unreachable before bad reputation; the longest failing first;
            // the worst score first.
            due.sort_by_key(|(proxy, reason)| {
                (
                    *reason != ReplaceReason::Unreachable,
                    proxy.failing_since,
                    std::cmp::Reverse(
                        proxy
                            .last_health
                            .as_ref()
                            .and_then(|health| health.reputation_score),
                    ),
                )
            });
            for (proxy, reason) in due {
                match self.request_replacement(proxy.id, reason).await {
                    Ok(ReplacementOutcome::Requested { .. }) => {
                        report.requested.push(proxy.id);
                        break;
                    }
                    // Neither helps the next candidate either.
                    Ok(
                        ReplacementOutcome::BudgetExhausted { .. }
                        | ReplacementOutcome::QuotaExhausted
                        | ReplacementOutcome::Unsupported,
                    ) => break,
                    Ok(_) => {}
                    Err(PoolError::Store(error)) => return Err(error.into()),
                    Err(error) => {
                        tracing::warn!(proxy = %proxy.id, %error, "cannot request a replacement");
                        break;
                    }
                }
            }
        }

        if !report.replaced.is_empty()
            && let Err(error) = self.reconcile().await
        {
            tracing::warn!(%error, "cannot reconcile after a replacement");
        }

        self.check_capacity(&live);
        Ok(report)
    }

    /// Runs the pool's upkeep until `shutdown` resolves: a sync with the
    /// provider and a read of its account at the start and then every
    /// `sync_interval` (spec §7.2), and a maintenance pass every `tick`.
    /// Failures are logged and tried again at the next turn; the pool never
    /// stops by itself.
    pub async fn run(
        self: Arc<Self>,
        sync_interval: std::time::Duration,
        tick: std::time::Duration,
        shutdown: impl Future<Output = ()> + Send,
    ) {
        tokio::pin!(shutdown);
        let mut next_sync = tokio::time::Instant::now();
        loop {
            if self.provider.is_some() && tokio::time::Instant::now() >= next_sync {
                next_sync = tokio::time::Instant::now() + sync_interval;
                match self.reconcile().await {
                    Ok(report) => tracing::debug!(
                        listed = report.listed,
                        added = report.added.len(),
                        retired = report.retired.len(),
                        missing = report.missing.len(),
                        "proxy pool synced with the provider"
                    ),
                    Err(error) => tracing::warn!(%error, "cannot sync the proxy pool"),
                }
                if let Err(error) = self.sync_account().await {
                    tracing::warn!(%error, "cannot read the provider account");
                }
            }
            if let Err(error) = self.tick().await {
                tracing::error!(%error, "proxy pool maintenance failed");
            }
            tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(tick) => {}
            }
        }
    }

    /// Alerts when a quarter or more of the pool cannot be used (spec risk
    /// R4), and publishes the pool's state as metrics.
    fn check_capacity(&self, live: &[Proxy]) {
        for state in ProxyState::ALL {
            if *state == ProxyState::Retired {
                continue;
            }
            let count = live.iter().filter(|proxy| proxy.state == *state).count();
            // A pool is far smaller than 2^52 proxies.
            #[allow(clippy::cast_precision_loss)]
            metrics::gauge!("kage_proxy_state", "state" => state.as_str()).set(count as f64);
        }
        // A proxy that was never checked is not counted as lost: at first
        // start every proxy is new for a moment.
        let out = live
            .iter()
            .filter(|proxy| matches!(proxy.state, ProxyState::Backoff | ProxyState::Replacing))
            .count();
        if live.len() >= MIN_POOL_FOR_SHARES && out * 4 >= live.len() {
            self.raise_once(self.alert(
                AlertKind::PoolCapacityLow,
                AlertSeverity::Warning,
                "a quarter or more of the proxy pool is unusable",
                json!({ "unusable": out, "live": live.len() }),
            ));
        } else {
            self.settle(AlertKind::PoolCapacityLow, None);
        }
    }
}

/// Whether an entry of the provider's log is about this proxy: the same
/// address, and not from before the pool knew it.
fn replaced_this(entry: &ReplacedProxy, proxy: &Proxy) -> bool {
    entry.old_host == proxy.host
        && entry.old_port.is_none_or(|port| port == proxy.port)
        && entry.at >= proxy.created_at
}
