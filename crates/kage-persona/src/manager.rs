//! The persona manager: generation, and a persona's life as the pool's
//! events drive it (spec §6.1, §6.3).
//!
//! **Generation.** A proxy that is usable and has no persona gets one. Its
//! id is new and its seed derives from the id; everything else derives from
//! the seed and from the observed geo record of the proxy: the entry of
//! `personas.mix` (a deficit-weighted draw over the personas that already
//! exist, the country first and then the pool), the locale and the stock
//! language list of that browser in that country, the time zone, a
//! position near the lookup's coordinates and a screen class. The engine's
//! sidecar then generates the fingerprint from the seed and those
//! constraints, and the whole document is stored and never changed. One
//! generation runs at a time, so that each draw sees the personas before
//! it.
//!
//! **Life.** The manager subscribes to [`Pool::subscribe`]. A proxy that
//! becomes usable gets its persona; one in backoff or being replaced cools
//! its persona; a new geo record moves the persona's time zone and
//! position and, across a border, its locale and languages; a retired
//! proxy retires its persona, ends its sessions and deletes its http
//! cookies, and the replacement proxy gets a new persona that inherits
//! nothing. That is the same path whoever replaced the proxy: Kage within
//! its budget, or the provider by itself. Events can be lost (the channel
//! is bounded, the process restarts), so every sweep also reconciles the
//! personas with the pool from the store.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::{DateTime, Utc};
use kage_browser::contract::{GeneratePersonaRequest, PersonaSummary};
use kage_core::alert::{Alert, AlertKind, AlertSeverity, AlertSink};
use kage_core::config::{Config, EgressMode};
use kage_core::domain::{
    ClaimedOs, CoolingReason, Engine, HealthEvent, Persona, PersonaState, Proxy, ProxyEndpoint,
    ProxyProviderKind, ProxyState, Render, SubjectKind,
};
use kage_core::ids::{HealthEventId, PersonaId, ProxyId};
use kage_core::persona::locale;
use kage_core::persona::mix::{self, MixCounts, MixEntry};
use kage_core::persona::place::ZoneSource;
use kage_core::persona::screen::{self, ScreenClass};
use kage_core::persona::warmup::{self, PlannedVisit, WarmupSubject};
use kage_core::persona::{self};
use kage_core::seed::Seed;
use kage_http::{ClientFactory, HttpSettings};
use kage_proxy::{Clock, Pool, PoolEvent};
use kage_store::{Store, StoreError};
use serde_json::json;
use tokio::sync::broadcast;

use crate::contexts::{BrowserGate, Contexts, DirectEgress, Egress, Via};
use crate::domain_state::DomainStates;
use crate::identity::{Directory, profile_for};
use crate::lifecycle::{self, GeoChange, Place, Transition};
use crate::sessions::Sessions;
use crate::sidecar::{
    Sidecars, contract_os, domain_engine, domain_os, domain_screen, exact_bounds,
};
use crate::{PersonaError, PersonaEvent};

/// Failed generations in a row for one proxy before the alert is raised,
/// when the failure is one that time may cure.
const TRANSIENT_FAILURES_BEFORE_ALERT: u32 = 3;

/// Why a persona was retired with its proxy.
pub const REASON_PROXY_RETIRED: &str = "proxy_retired";
/// Why a persona was retired by `kage persona regenerate`.
pub const REASON_REGENERATED: &str = "regenerated";
/// Why a persona was retired by `kage persona retire`.
pub const REASON_OPERATOR: &str = "operator";

/// What the manager is built from.
pub struct PersonaParts {
    /// The store.
    pub store: Store,
    /// The proxy pool whose events drive the personas.
    pub pool: Arc<Pool>,
    /// The two sidecars.
    pub sidecars: Sidecars,
    /// The http path's clients: a retired persona's are forgotten.
    pub http: Arc<ClientFactory>,
    /// Where alerts go.
    pub alerts: Arc<dyn AlertSink>,
    /// The configuration.
    pub config: Config,
    /// The certificate authority of the operator's own echo
    /// (`selftest.http.echo_ca_file`), as PEM.
    pub echo_trust_anchor: Option<Vec<u8>>,
}

impl std::fmt::Debug for PersonaParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonaParts")
            .field("sidecars", &self.sidecars)
            .finish_non_exhaustive()
    }
}

/// What one reconciliation with the pool did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcileReport {
    /// Personas generated for proxies that had none.
    pub generated: Vec<PersonaId>,
    /// Personas retired because their proxy is gone.
    pub retired: Vec<PersonaId>,
    /// Usable proxies that still have no persona: generation failed.
    pub failed: Vec<ProxyId>,
}

/// What one sweep did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// The reconciliation with the pool.
    pub reconcile: ReconcileReport,
    /// Sessions that had ended and were removed.
    pub sessions_ended: u64,
    /// Profile directories archived.
    pub archived: u32,
    /// Archives deleted.
    pub deleted: u32,
    /// Copies of profiles from before a browser upgrade deleted.
    pub upgrade_copies_deleted: u32,
    /// Warm-up visits made or skipped.
    pub warmup_visits: u32,
    /// Self-tests run.
    pub selftests: u32,
}

#[derive(Debug, Default)]
struct Memory {
    /// Alerts that were raised and whose cause still stands.
    standing: HashSet<String>,
    /// Failed generations in a row, per proxy.
    failures: HashMap<ProxyId, u32>,
    /// Whether the missing personas volume was already reported.
    volume_reported: bool,
}

/// The persona manager. See the module documentation.
pub struct PersonaManager {
    pub(crate) store: Store,
    pub(crate) pool: Arc<Pool>,
    pub(crate) sidecars: Sidecars,
    pub(crate) http: Arc<ClientFactory>,
    /// Clients for the self-test of the http path: without cookies, and
    /// trusting the operator's own echo.
    pub(crate) selftest_http: ClientFactory,
    pub(crate) alerts: Arc<dyn AlertSink>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: Config,
    directory: Arc<Directory>,
    events: broadcast::Sender<PersonaEvent>,
    /// One generation at a time: each draw sees the personas before it.
    generation: tokio::sync::Mutex<()>,
    /// One context of the manager's own per engine, in a process without
    /// a scheduler: a warm-up visit or a self-test, never two at once.
    browser_work: [tokio::sync::Mutex<()>; 2],
    /// The one owner of every persona's context.
    contexts: Arc<Contexts>,
    /// Where a persona's traffic leaves the process.
    egress: Arc<dyn Egress>,
    /// The scheduler, when one runs in this process.
    gate: OnceLock<Arc<dyn BrowserGate>>,
    memory: Mutex<Memory>,
}

impl std::fmt::Debug for PersonaManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonaManager")
            .field("sidecars", &self.sidecars)
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

impl PersonaManager {
    /// A manager from its parts.
    ///
    /// # Errors
    ///
    /// Fails when `personas.mix` is one no persona can be drawn from: no
    /// entry, no weight, an entry twice, or a chromium entry for another
    /// system than Linux while `engines.chromium.allow_cross_os_claims` is
    /// off.
    pub fn new(parts: PersonaParts) -> Result<Self, PersonaError> {
        mix::validate(
            &parts.config.personas.mix,
            parts.config.engines.chromium.allow_cross_os_claims,
        )
        .map_err(|error| PersonaError::Mix(error.to_string()))?;
        let mut selftest_http =
            ClientFactory::in_memory(HttpSettings::from_config(&parts.config.http));
        if let Some(pem) = parts.echo_trust_anchor {
            selftest_http = selftest_http.with_trust_anchor(pem);
        }
        let (events, _) = broadcast::channel(256);
        let clock = Arc::clone(parts.pool.clock());
        Ok(Self {
            contexts: Arc::new(Contexts::new(
                parts.sidecars.clone(),
                parts.config.scheduler.max_contexts_per_engine,
                Arc::clone(&clock),
            )),
            egress: Arc::new(DirectEgress),
            gate: OnceLock::new(),
            store: parts.store,
            clock,
            pool: parts.pool,
            sidecars: parts.sidecars,
            http: parts.http,
            selftest_http,
            alerts: parts.alerts,
            config: parts.config,
            directory: Arc::new(Directory::new()),
            events,
            generation: tokio::sync::Mutex::new(()),
            browser_work: [tokio::sync::Mutex::new(()), tokio::sync::Mutex::new(())],
            memory: Mutex::new(Memory::default()),
        })
    }

    /// The same manager with every persona's traffic leaving through
    /// `egress`: the forwarder. Without this call it goes straight to the
    /// provider's proxy.
    #[must_use]
    pub fn with_egress(mut self, egress: Arc<dyn Egress>) -> Self {
        self.egress = egress;
        self
    }

    /// Makes the manager's own browser work (warm-up visits, scheduled
    /// self-tests) ask `gate` before it takes a persona: the scheduler of
    /// this process. The first gate stays; `false` when there was one.
    pub fn set_gate(&self, gate: Arc<dyn BrowserGate>) -> bool {
        self.gate.set(gate).is_ok()
    }

    /// The scheduler's gate, when one was set.
    pub fn gate(&self) -> Option<&Arc<dyn BrowserGate>> {
        self.gate.get()
    }

    /// The one owner of every persona's browser context.
    pub const fn contexts(&self) -> &Arc<Contexts> {
        &self.contexts
    }

    /// The two sidecars.
    pub const fn sidecars(&self) -> &Sidecars {
        &self.sidecars
    }

    /// The clients of the http path.
    pub const fn http(&self) -> &Arc<ClientFactory> {
        &self.http
    }

    /// The proxy pool.
    pub const fn pool(&self) -> &Arc<Pool> {
        &self.pool
    }

    /// Which persona lives behind which proxy on the http path. The health
    /// checker's probe clients read it
    /// ([`crate::identity::PersonaProbeClients`]).
    pub fn directory(&self) -> Arc<Directory> {
        Arc::clone(&self.directory)
    }

    /// Persona events as they happen.
    pub fn subscribe(&self) -> broadcast::Receiver<PersonaEvent> {
        self.events.subscribe()
    }

    /// The sessions service.
    pub fn sessions(&self) -> Sessions {
        Sessions::new(
            self.store.clone(),
            Arc::clone(&self.clock),
            self.config.sessions.limits(),
        )
    }

    /// The per-domain state service.
    pub fn domains(&self) -> DomainStates {
        DomainStates::new(self.store.clone(), Arc::clone(&self.clock))
    }

    /// The manager's clock: the pool's.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    fn memory(&self) -> MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The lock of the manager's own browser work on an engine.
    pub(crate) fn browser_work(&self, engine: Engine) -> &tokio::sync::Mutex<()> {
        match engine {
            Engine::Chromium => &self.browser_work[0],
            Engine::Firefox => &self.browser_work[1],
        }
    }

    // --- Events and alerts -------------------------------------------------

    pub(crate) fn publish(&self, event: PersonaEvent) {
        // Nobody listening is not an error.
        let _ = self.events.send(event);
    }

    /// Stores a health event about a persona. A failure to store it is
    /// logged and nothing else: the event is a record, not a step.
    pub(crate) async fn event(
        &self,
        persona: PersonaId,
        kind: &str,
        severity: AlertSeverity,
        details: serde_json::Value,
    ) {
        let event = HealthEvent {
            id: HealthEventId::new(),
            at: self.now(),
            subject_kind: SubjectKind::Persona,
            subject_id: Some(persona.as_uuid()),
            kind: kind.to_owned(),
            severity,
            details,
        };
        tracing::info!(persona = %persona, event = kind, details = %event.details, "persona event");
        if let Err(error) = self.store.health_events().insert(&event).await {
            tracing::warn!(%error, event = kind, "cannot store a persona health event");
        }
    }

    /// Raises an alert unless the same one is already standing.
    pub(crate) fn raise_once(&self, key: String, alert: Alert) {
        if self.memory().standing.insert(key) {
            self.alerts.raise(alert.at(self.now()));
        }
    }

    /// Notes that the cause of a standing alert is gone. Returns whether
    /// one was standing.
    pub(crate) fn settle(&self, key: &str) -> bool {
        self.memory().standing.remove(key)
    }

    /// Whether the missing personas volume is reported now for the first
    /// time.
    pub(crate) fn report_volume_once(&self) -> bool {
        !std::mem::replace(&mut self.memory().volume_reported, true)
    }

    fn transition(&self, persona: PersonaId, transition: Option<Transition>) {
        if let Some((from, to)) = transition {
            self.publish(PersonaEvent::StateChanged { persona, from, to });
        }
    }

    // --- Reading ----------------------------------------------------------

    /// One persona.
    ///
    /// # Errors
    ///
    /// [`PersonaError::NotFound`] when there is none; fails when the store
    /// fails.
    pub async fn get(&self, id: PersonaId) -> Result<Persona, PersonaError> {
        self.store
            .personas()
            .get(id)
            .await?
            .ok_or(PersonaError::NotFound(id))
    }

    /// A live persona's proxy with its credentials opened, for work that
    /// sends traffic as the persona.
    ///
    /// # Errors
    ///
    /// [`PersonaError::Retired`] for a retired persona,
    /// [`PersonaError::ProxyNotUsable`] while its proxy cannot carry
    /// traffic, [`PersonaError::EgressUnsafe`] while a leak is on record.
    pub async fn route(&self, persona: &Persona) -> Result<(Proxy, ProxyEndpoint), PersonaError> {
        if !persona.is_live() {
            return Err(PersonaError::Retired(persona.id));
        }
        let proxy = self
            .pool
            .get(persona.proxy_id)
            .await?
            .ok_or(PersonaError::ProxyNotFound(persona.proxy_id))?;
        // A row this installation does not leave through carries nothing:
        // the pool's under `egress.mode: direct`, the host's under
        // `proxies`.
        if !proxy.state.is_usable() || !self.serves(&proxy) {
            return Err(PersonaError::ProxyNotUsable {
                proxy: proxy.id,
                state: proxy.state,
            });
        }
        if self.egress_unsafe().await? {
            return Err(PersonaError::EgressUnsafe);
        }
        let endpoint = self.pool.endpoint(&proxy)?;
        Ok((proxy, endpoint))
    }

    /// Whether this installation keeps personas for a row and leaves
    /// through it (spec §25): under `egress.mode: direct` the host's own
    /// rows only, under `proxies` the pool's only. Never both: a site that
    /// sees a visitor move from a residential address to the host's has
    /// seen a bot.
    pub fn serves(&self, proxy: &Proxy) -> bool {
        (proxy.provider == ProxyProviderKind::Host)
            == (self.config.egress.mode == EgressMode::Direct)
    }

    /// Whether a leak of the pool stops everything: not when nothing
    /// leaves through the pool.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub(crate) async fn egress_unsafe(&self) -> Result<bool, PersonaError> {
        if self.config.egress.mode == EgressMode::Direct {
            return Ok(false);
        }
        Ok(self.pool.egress_unsafe().await?)
    }

    /// The place of a persona of `engine` with `seed` behind `proxy`:
    /// what the proxy's geo record says (spec §6.1). For a row of the
    /// host's own connection what the operator declared wins (spec §25):
    /// its country, which a lookup that disagrees only counts against
    /// (`geo_mismatch`), and its locale.
    fn place_for(&self, proxy: &Proxy, engine: Engine, seed: &Seed) -> Place {
        let direct = &self.config.egress.direct;
        if proxy.provider != ProxyProviderKind::Host || direct.country.is_none() {
            return lifecycle::place_of(proxy, engine, seed);
        }
        let mut declared = proxy.clone();
        declared.country = direct.country;
        // A lookup that names another country says nothing of the declared
        // place: neither its time zone nor its position is taken.
        if declared
            .observed_geo()
            .is_some_and(|geo| Some(geo.country) != direct.country)
            && let Some(health) = declared.last_health.as_mut()
        {
            health.geo = None;
        }
        let mut place = lifecycle::place_of(&declared, engine, seed);
        if let Some(locale) = direct
            .locale
            .as_deref()
            .and_then(|tag| locale::declared(tag, engine))
        {
            place.locale = locale;
        }
        place
    }

    /// A live persona's proxy, and the endpoint its browser or its http
    /// client is given for it: the forwarder under the persona's own
    /// credential when there is one, else the proxy itself.
    ///
    /// # Errors
    ///
    /// As [`Self::route`].
    pub async fn egress(
        &self,
        persona: &Persona,
        via: Via,
    ) -> Result<(Proxy, ProxyEndpoint), PersonaError> {
        let (proxy, upstream) = self.route(persona).await?;
        let endpoint = self.egress.endpoint(persona, &upstream, via);
        Ok((proxy, endpoint))
    }

    /// The warm-up plan of a persona: the same every time it is asked.
    pub fn warmup_plan(&self, persona: &Persona) -> Vec<PlannedVisit> {
        warmup::plan(
            &WarmupSubject {
                seed: &persona.seed,
                created_at: persona.created_at,
                timezone: &persona.timezone,
                country: persona.country,
                locale: &persona.locale,
            },
            &self.config.personas.warmup,
        )
    }

    // --- Generation -------------------------------------------------------

    /// Fills the directory from the stored personas. Called once at start,
    /// before the health checker probes anything.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn load(&self) -> Result<usize, PersonaError> {
        let live = self.store.personas().list_live().await?;
        for persona in &live {
            self.directory.record(persona);
        }
        Ok(live.len())
    }

    /// The persona of a proxy: the one it has, brought in step with the
    /// proxy's state, or a new one when the proxy is usable and has none.
    /// `None` when the proxy does not exist, is retired, or is not usable
    /// yet and has no persona.
    ///
    /// # Errors
    ///
    /// Fails when the store fails or the persona cannot be generated; a
    /// failed generation raises `persona_generation_failed`, at once or,
    /// for a failure that time may cure, on the third in a row.
    pub async fn ensure(&self, proxy: ProxyId) -> Result<Option<Persona>, PersonaError> {
        let Some(proxy) = self.pool.get(proxy).await? else {
            return Ok(None);
        };
        // A row this installation does not leave through keeps what it has
        // and gets nothing new: its personas wait for the mode they were
        // made under.
        if !proxy.state.is_live() || !self.serves(&proxy) {
            return Ok(None);
        }
        if let Some(existing) = self.store.personas().get_by_proxy(proxy.id).await? {
            return Ok(Some(self.follow(existing, &proxy).await?));
        }
        if !proxy.state.is_usable() {
            return Ok(None);
        }
        match self.generate(&proxy).await {
            Ok(persona) => {
                self.memory().failures.remove(&proxy.id);
                self.settle(&generation_key(proxy.id));
                Ok(Some(persona))
            }
            Err(error) => {
                self.generation_failed(&proxy, &error);
                Err(error)
            }
        }
    }

    fn generation_failed(&self, proxy: &Proxy, error: &PersonaError) {
        let failures = {
            let mut memory = self.memory();
            let failures = memory.failures.entry(proxy.id).or_insert(0);
            *failures += 1;
            *failures
        };
        tracing::warn!(proxy = %proxy.id, %error, failures, "cannot generate a persona");
        if error.is_transient() && failures < TRANSIENT_FAILURES_BEFORE_ALERT {
            return;
        }
        self.raise_once(
            generation_key(proxy.id),
            Alert::new(
                AlertKind::PersonaGenerationFailed,
                AlertSeverity::Critical,
                format!(
                    "no persona could be generated for proxy {}: {error}; nothing is fetched \
                     through it until one exists",
                    proxy.id
                ),
                json!({ "error": error.to_string(), "failures": failures }),
            )
            .about(proxy.id.as_uuid()),
        );
    }

    /// Generates the persona of a usable proxy that has none (spec §6.1).
    /// When the proxy got one meanwhile, that one is returned.
    ///
    /// # Errors
    ///
    /// [`PersonaError::ProxyNotUsable`] for a proxy that cannot carry
    /// traffic; a sidecar error when the fingerprint cannot be generated;
    /// [`PersonaError::Generation`] when the sidecar generated something
    /// else than it was asked for.
    pub async fn generate(&self, proxy: &Proxy) -> Result<Persona, PersonaError> {
        let _one_at_a_time = self.generation.lock().await;
        if let Some(existing) = self.store.personas().get_by_proxy(proxy.id).await? {
            return Ok(existing);
        }
        if !proxy.state.is_usable() {
            return Err(PersonaError::ProxyNotUsable {
                proxy: proxy.id,
                state: proxy.state,
            });
        }
        let (mut persona, place, generator) = self.draft(proxy).await?;
        // A persona that has nothing to warm up with is ready at once.
        let planned = self.warmup_plan(&persona).len();
        if planned == 0 {
            persona.warmed_at = Some(persona.created_at);
        }

        match self.store.personas().insert(&persona).await {
            Ok(()) => {}
            // Another process (the CLI next to the server) was faster.
            Err(StoreError::Conflict { .. }) => {
                if let Some(existing) = self.store.personas().get_by_proxy(proxy.id).await? {
                    return Ok(existing);
                }
                return Err(PersonaError::ProxyNotFound(proxy.id));
            }
            Err(error) => return Err(error.into()),
        }
        self.directory.record(&persona);
        self.announce(&persona, &place, &generator, planned).await;
        Ok(persona)
    }

    /// Draws a new persona for a proxy and has its fingerprint generated.
    /// Nothing is stored.
    async fn draft(&self, proxy: &Proxy) -> Result<(Persona, Place, String), PersonaError> {
        let now = self.now();
        let id = PersonaId::new();
        let seed = persona::seed_for(id);

        // Engine and system: the entry the pool is shortest of, the
        // country of the new persona first.
        let country = self.place_for(proxy, Engine::Chromium, &seed).country;
        // The host's personas run the engines the operator names, if any.
        let named: Vec<MixEntry>;
        let entries: &[MixEntry] = match &self.config.egress.direct.engines {
            Some(engines) if proxy.provider == ProxyProviderKind::Host => {
                named = self
                    .config
                    .personas
                    .mix
                    .iter()
                    .filter(|entry| engines.contains(&entry.engine))
                    .copied()
                    .collect();
                if named.is_empty() {
                    return Err(PersonaError::Mix(
                        "egress.direct.engines names no engine of personas.mix".to_owned(),
                    ));
                }
                &named
            }
            _ => &self.config.personas.mix,
        };
        let live = self.store.personas().list_live().await?;
        let (counts, totals) = MixCounts::of(
            entries,
            live.iter()
                .map(|other| (other.engine, other.claimed_os, other.country == country)),
        );
        let entry = mix::draw(entries, &counts, totals, &seed)
            .and_then(|index| entries.get(index))
            .ok_or_else(|| PersonaError::Mix("no entry has a weight".to_owned()))?;
        let (engine, os) = (entry.engine, entry.os);
        if engine == Engine::Chromium
            && os != ClaimedOs::Linux
            && !self.config.engines.chromium.allow_cross_os_claims
        {
            return Err(PersonaError::CrossOsClaim { engine, os });
        }

        let place = self.place_for(proxy, engine, &seed);
        let class = screen::draw(engine, os, &seed);
        let configured_major = match engine {
            Engine::Chromium => self.config.engines.chromium.browser_major,
            Engine::Firefox => self.config.engines.firefox.browser_major,
        };
        let request = GeneratePersonaRequest {
            browser_major: configured_major.map(u64::from),
            claimed_os: contract_os(os),
            languages: place.locale.languages.clone(),
            screen: class.map(exact_bounds),
            seed: seed.to_string(),
        };
        let generated = self.sidecars.of(engine).generate_persona(&request).await?;
        let summary = generated.summary;
        let (browser_major, hardware_concurrency, screen) =
            verify(engine, os, &request, class, configured_major, &summary)?;

        let persona = Persona {
            id,
            proxy_id: proxy.id,
            engine,
            claimed_os: os,
            browser_major,
            seed,
            fingerprint: serde_json::Value::Object(generated.fingerprint),
            user_agent: summary.user_agent,
            hardware_concurrency,
            wreq_profile: profile_for(engine, browser_major, os),
            country: place.country,
            locale: place.locale.locale.clone(),
            languages: place.locale.languages.clone(),
            timezone: place.timezone.clone(),
            geolocation: place.geolocation,
            screen,
            tables_version: persona::TABLES_VERSION.to_owned(),
            profile_dir: persona::profile_dir(engine, id),
            state: PersonaState::Active,
            cooling_reason: None,
            warmup_visits: 0,
            warmed_at: None,
            created_at: now,
            retired_at: None,
            retire_reason: None,
            profile_archived_at: None,
            profile_archive: None,
            profile_deleted_at: None,
        };
        Ok((persona, place, summary.generator))
    }

    /// Records a new persona: the health event, what an operator should
    /// know about it, and the events for subscribers.
    async fn announce(&self, persona: &Persona, place: &Place, generator: &str, planned: usize) {
        let (engine, browser_major) = (persona.engine, persona.browser_major);
        let country = place.country.map(|country| country.to_string());
        self.event(
            persona.id,
            "persona_generated",
            AlertSeverity::Info,
            json!({
                "proxy": persona.proxy_id,
                "engine": engine.as_str(),
                "claimed_os": persona.claimed_os.as_str(),
                "browser_major": browser_major,
                "generator": generator,
                "country": country,
                "timezone": place.timezone,
                "timezone_source": match place.zone_source {
                    ZoneSource::Observed => "observed",
                    ZoneSource::Country => "country",
                    ZoneSource::Unknown => "unknown",
                },
                "locale": place.locale.locale,
                "languages": place.locale.languages,
                "position": place.geolocation.is_some(),
                "screen": format!("{}x{}", persona.screen.width, persona.screen.height),
                "hardware_concurrency": persona.hardware_concurrency,
                "wreq_profile": persona.wreq_profile,
                "tables_version": persona.tables_version,
                "warmup_visits_planned": planned,
            }),
        )
        .await;
        if !place.locale.known {
            self.event(
                persona.id,
                "persona_locale_unknown",
                AlertSeverity::Warning,
                json!({
                    "country": country,
                    "locale": place.locale.locale,
                    "languages": place.locale.languages,
                }),
            )
            .await;
        }
        if persona.wreq_profile.is_none() {
            self.raise_once(
                format!("http_profile_missing:{engine}:{browser_major}"),
                Alert::new(
                    AlertKind::HttpProfileMissing,
                    AlertSeverity::Warning,
                    format!(
                        "no emulation profile presents {engine} {browser_major}: its personas \
                         are browser-only"
                    ),
                    json!({ "engine": engine.as_str(), "browser_major": browser_major }),
                ),
            );
        }
        self.publish(PersonaEvent::Generated {
            persona: persona.id,
            proxy: persona.proxy_id,
        });
        if persona.warmed_at.is_some() {
            self.publish(PersonaEvent::Warmed {
                persona: persona.id,
            });
        }
    }

    // --- Life -------------------------------------------------------------

    /// Brings a persona in step with its proxy: its state, and the place
    /// the proxy's geo record gives.
    async fn follow(&self, persona: Persona, proxy: &Proxy) -> Result<Persona, PersonaError> {
        let place = self.place_for(proxy, persona.engine, &persona.seed);
        let proxy_state = proxy.state;
        let Some((change, moved, cooled, updated)) = self
            .store
            .personas()
            .modify(persona.id, |persona| {
                let change = lifecycle::follow_geo(persona, &place);
                let cooled = if change == GeoChange::Identity {
                    lifecycle::cool_until_selftest(persona, CoolingReason::GeoChanged)
                } else {
                    None
                };
                let moved = lifecycle::follow_proxy(persona, proxy_state);
                (change, moved, cooled, persona.clone())
            })
            .await?
        else {
            return Err(PersonaError::NotFound(persona.id));
        };
        self.directory.record(&updated);

        if change != GeoChange::None {
            let severity = match change {
                GeoChange::Identity => AlertSeverity::Warning,
                GeoChange::Place | GeoChange::None => AlertSeverity::Info,
            };
            self.event(
                updated.id,
                "persona_geo_updated",
                severity,
                json!({
                    "proxy": proxy.id,
                    "change": match change {
                        GeoChange::Identity => "identity",
                        GeoChange::Place | GeoChange::None => "place",
                    },
                    "from": {
                        "country": persona.country.map(|country| country.to_string()),
                        "timezone": persona.timezone,
                        "locale": persona.locale,
                        "languages": persona.languages,
                    },
                    "to": {
                        "country": updated.country.map(|country| country.to_string()),
                        "timezone": updated.timezone,
                        "locale": updated.locale,
                        "languages": updated.languages,
                    },
                    "cooling": updated.cooling_reason.map(CoolingReason::as_str),
                }),
            )
            .await;
            self.publish(PersonaEvent::GeoUpdated {
                persona: updated.id,
                change,
            });
        }
        for transition in [cooled, moved].into_iter().flatten() {
            self.event(
                updated.id,
                "persona_state_changed",
                AlertSeverity::Info,
                json!({
                    "from": transition.0.as_str(),
                    "to": transition.1.as_str(),
                    "reason": updated.cooling_reason.map(CoolingReason::as_str),
                    "proxy_state": proxy_state.as_str(),
                }),
            )
            .await;
            self.transition(updated.id, Some(transition));
        }
        Ok(updated)
    }

    /// Applies one pool event.
    ///
    /// # Errors
    ///
    /// Fails when the store or a sidecar fails; the next sweep's
    /// reconciliation does what this event could not.
    pub async fn handle(&self, event: &PoolEvent) -> Result<(), PersonaError> {
        match *event {
            PoolEvent::StateChanged {
                proxy,
                to: ProxyState::Retired,
                ..
            } => self.proxy_retired(proxy).await?,
            PoolEvent::Added { proxy }
            | PoolEvent::GeoChanged { proxy }
            | PoolEvent::StateChanged { proxy, .. } => {
                self.ensure(proxy).await?;
            }
            PoolEvent::EgressUnsafe(_) => {}
        }
        Ok(())
    }

    /// A proxy was retired: its persona goes with it, and so does whatever
    /// the http path kept for the proxy itself.
    async fn proxy_retired(&self, proxy: ProxyId) -> Result<(), PersonaError> {
        if let Some(persona) = self.store.personas().get_by_proxy(proxy).await? {
            self.retire(persona.id, REASON_PROXY_RETIRED).await?;
        }
        self.http.forget(proxy).await;
        self.store.http_cookies().delete_for_proxy(proxy).await?;
        self.directory.forget(proxy);
        self.memory().failures.remove(&proxy);
        self.settle(&generation_key(proxy));
        Ok(())
    }

    /// Retires a persona (spec §6.3): its sessions end and its http
    /// cookies are deleted at once, and its profile directory is archived
    /// and later deleted by the sweep. Retiring a retired persona changes
    /// nothing.
    ///
    /// # Errors
    ///
    /// [`PersonaError::NotFound`] when there is no such persona; fails
    /// when the store fails.
    pub async fn retire(&self, id: PersonaId, reason: &str) -> Result<Persona, PersonaError> {
        let now = self.now();
        let Some((transition, persona)) = self
            .store
            .personas()
            .modify(id, |persona| {
                (lifecycle::retire(persona, now, reason), persona.clone())
            })
            .await?
        else {
            return Err(PersonaError::NotFound(id));
        };
        if transition.is_none() {
            return Ok(persona);
        }
        // No probe presents as a persona that is gone, from this moment.
        self.directory.record(&persona);
        // Its browser has nothing left to do. One that is held by an
        // attempt is closed by whoever holds it.
        self.contexts.close_of(id).await;
        let sessions = self.store.sessions().delete_for_persona(id).await?;
        self.http.forget_persona(id).await;
        let cookies = self.store.http_cookies().delete_for_persona(id).await?;
        for render in [Render::Http, Render::Browser] {
            self.settle(&crate::selftest::alert_key(id, render));
        }
        self.event(
            id,
            "persona_retired",
            AlertSeverity::Info,
            json!({
                "proxy": persona.proxy_id,
                "reason": reason,
                "sessions_ended": sessions,
                "http_cookies_deleted": cookies,
            }),
        )
        .await;
        self.transition(id, transition);
        Ok(persona)
    }

    /// Replaces the persona behind a proxy with a new one: the old one is
    /// retired and the proxy gets a persona that inherits nothing.
    ///
    /// # Errors
    ///
    /// [`PersonaError::NotFound`] and [`PersonaError::Retired`] for a
    /// persona that cannot be replaced, [`PersonaError::ProxyNotUsable`]
    /// while its proxy cannot carry traffic (nothing is retired then), and
    /// whatever generation fails with (the old persona is retired then, and
    /// the sweep generates the new one).
    pub async fn regenerate(&self, id: PersonaId) -> Result<Persona, PersonaError> {
        let old = self.get(id).await?;
        if !old.is_live() {
            return Err(PersonaError::Retired(id));
        }
        let proxy = self
            .pool
            .get(old.proxy_id)
            .await?
            .ok_or(PersonaError::ProxyNotFound(old.proxy_id))?;
        if !proxy.state.is_usable() {
            return Err(PersonaError::ProxyNotUsable {
                proxy: proxy.id,
                state: proxy.state,
            });
        }
        self.retire(id, REASON_REGENERATED).await?;
        self.generate(&proxy).await
    }

    /// Brings the personas in step with the pool as the store has it: a
    /// persona whose proxy is gone is retired, every other one follows its
    /// proxy's state and place, and a usable proxy without a persona gets
    /// one.
    ///
    /// # Errors
    ///
    /// Fails when the store fails. A persona that cannot be generated is
    /// reported in [`ReconcileReport::failed`], not as an error.
    pub async fn reconcile(&self) -> Result<ReconcileReport, PersonaError> {
        let mut report = ReconcileReport::default();
        let proxies: HashMap<ProxyId, Proxy> = self
            .pool
            .live()
            .await?
            .into_iter()
            .map(|proxy| (proxy.id, proxy))
            .collect();
        let mut covered = HashSet::new();
        for persona in self.store.personas().list_live().await? {
            if let Some(proxy) = proxies.get(&persona.proxy_id) {
                covered.insert(proxy.id);
                // A persona of a row this installation does not leave
                // through waits as it is.
                if self.serves(proxy) {
                    self.follow(persona, proxy).await?;
                }
            } else {
                report.retired.push(persona.id);
                self.retire(persona.id, REASON_PROXY_RETIRED).await?;
            }
        }
        let mut missing: Vec<&Proxy> = proxies
            .values()
            .filter(|proxy| {
                proxy.state.is_usable() && self.serves(proxy) && !covered.contains(&proxy.id)
            })
            .collect();
        // Oldest first, so that a restart draws the mix in the same order.
        missing.sort_by_key(|proxy| proxy.id);
        for proxy in missing {
            match self.ensure(proxy.id).await {
                Ok(Some(persona)) => report.generated.push(persona.id),
                Ok(None) => {}
                Err(PersonaError::Store(error)) => return Err(error.into()),
                Err(_) => report.failed.push(proxy.id),
            }
        }
        Ok(report)
    }

    /// One pass over everything no event announces: the reconciliation
    /// with the pool, ended sessions, profiles to archive and delete,
    /// warm-up visits and self-tests that are due. A part that fails is
    /// logged and the others still run.
    pub async fn sweep(&self) -> SweepReport {
        let mut report = SweepReport::default();
        match self.reconcile().await {
            Ok(reconcile) => report.reconcile = reconcile,
            Err(error) => tracing::warn!(%error, "cannot reconcile the personas with the pool"),
        }
        match self.sessions().sweep().await {
            Ok(ended) => report.sessions_ended = ended,
            Err(error) => tracing::warn!(%error, "cannot remove ended sessions"),
        }
        match self.archive_due().await {
            Ok((archived, deleted)) => {
                report.archived = archived;
                report.deleted = deleted;
            }
            Err(error) => tracing::warn!(%error, "cannot archive retired profiles"),
        }
        match self.sweep_upgrade_copies().await {
            Ok(deleted) => report.upgrade_copies_deleted = deleted,
            Err(error) => {
                tracing::warn!(%error, "cannot delete the profile copies from before an upgrade");
            }
        }
        match self.warm_up_due().await {
            Ok(visits) => report.warmup_visits = visits,
            Err(error) => tracing::warn!(%error, "cannot run the warm-up"),
        }
        match self.selftests_due().await {
            Ok(runs) => report.selftests = runs,
            Err(error) => tracing::warn!(%error, "cannot run the scheduled self-tests"),
        }
        report
    }

    /// Runs the manager until `shutdown` resolves: pool events as they
    /// arrive, and a sweep at the start and then every
    /// `personas.sweep_interval`. The two run side by side, so that a
    /// warm-up visit or a self-test never delays the retirement of a
    /// persona whose proxy is gone.
    pub async fn run(self: Arc<Self>, shutdown: impl Future<Output = ()> + Send) {
        let mut events = self.pool.subscribe();
        if let Err(error) = self.load().await {
            tracing::warn!(%error, "cannot read the stored personas");
        }
        let interval = self.config.personas.sweep_interval.get();
        let (stop, stopped) = tokio::sync::watch::channel(false);

        let follow = {
            let manager = Arc::clone(&self);
            let mut stopped = stopped.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = stopped.changed() => return,
                        event = events.recv() => match event {
                            Ok(event) => {
                                if let Err(error) = manager.handle(&event).await {
                                    tracing::warn!(%error, ?event, "cannot apply a pool event");
                                }
                            }
                            // Events were lost: read the pool itself.
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                if let Err(error) = manager.reconcile().await {
                                    tracing::warn!(%error, "cannot reconcile the personas");
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => return,
                        },
                    }
                }
            }
        };
        let sweep = {
            let manager = Arc::clone(&self);
            let mut stopped = stopped;
            async move {
                loop {
                    tokio::select! {
                        _ = stopped.changed() => return,
                        report = manager.sweep() => {
                            tracing::debug!(?report, "persona sweep");
                        }
                    }
                    tokio::select! {
                        _ = stopped.changed() => return,
                        () = tokio::time::sleep(interval) => {}
                    }
                }
            }
        };
        let stop = async move {
            shutdown.await;
            // Both loops hold a receiver until they return.
            let _ = stop.send(true);
        };
        tokio::join!(follow, sweep, stop);
    }
}

fn generation_key(proxy: ProxyId) -> String {
    format!("persona_generation_failed:{proxy}")
}

/// Checks that the sidecar generated what it was asked for, and reads the
/// numbers Kage stores: the browser major, the processor count and the
/// screen.
fn verify(
    engine: Engine,
    os: ClaimedOs,
    request: &GeneratePersonaRequest,
    class: Option<ScreenClass>,
    configured_major: Option<u32>,
    summary: &PersonaSummary,
) -> Result<(u32, u32, kage_core::domain::Screen), PersonaError> {
    let wrong = |reason: String| PersonaError::Generation { engine, reason };
    if domain_engine(summary.engine) != engine {
        return Err(wrong(format!(
            "it is the {} sidecar",
            domain_engine(summary.engine)
        )));
    }
    if domain_os(summary.claimed_os) != os {
        return Err(wrong(format!(
            "it claims {}, not {os}",
            domain_os(summary.claimed_os)
        )));
    }
    if !summary.seed.eq_ignore_ascii_case(&request.seed) {
        return Err(wrong("it was generated from another seed".to_owned()));
    }
    let browser_major = u32::try_from(summary.browser_major)
        .map_err(|_| wrong(format!("browser major {}", summary.browser_major)))?;
    if configured_major.is_some_and(|configured| configured != browser_major) {
        return Err(wrong(format!(
            "the engine bundles major {browser_major}, the configuration names {}",
            configured_major.unwrap_or_default()
        )));
    }
    let hardware_concurrency = u32::try_from(summary.hardware_concurrency)
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| wrong(format!("{} processors", summary.hardware_concurrency)))?;
    let screen = domain_screen(&summary.screen)
        .ok_or_else(|| wrong("a screen that is no screen".to_owned()))?;
    if let Some(class) = class
        && (screen.width != class.width || screen.height != class.height)
    {
        return Err(wrong(format!(
            "its screen is {}x{}, not the {}x{} that was asked for",
            screen.width, screen.height, class.width, class.height
        )));
    }
    Ok((browser_major, hardware_concurrency, screen))
}

#[cfg(test)]
mod tests {
    use kage_browser::contract::{
        self, EngineName, GeneratePersonaRequest, InjectMode, PersonaSummary,
    };
    use kage_core::domain::{ClaimedOs, Engine};
    use kage_core::persona::screen::ScreenClass;

    use super::verify;
    use crate::PersonaError;
    use crate::sidecar::exact_bounds;

    const CLASS: ScreenClass = ScreenClass {
        width: 2560,
        height: 1440,
        weight: 18,
    };

    fn request() -> GeneratePersonaRequest {
        GeneratePersonaRequest {
            browser_major: Some(152),
            claimed_os: contract::ClaimedOs::Windows,
            languages: vec!["de".to_owned(), "en-US".to_owned(), "en".to_owned()],
            screen: Some(exact_bounds(CLASS)),
            seed: "ab".repeat(32),
        }
    }

    fn summary() -> PersonaSummary {
        PersonaSummary {
            browser_major: 152,
            browser_version: "152.0.4".to_owned(),
            claimed_os: contract::ClaimedOs::Windows,
            engine: EngineName::Firefox,
            generator: "camoufox".to_owned(),
            hardware_concurrency: 12,
            inject: InjectMode::None,
            languages: vec!["de".to_owned(), "en-US".to_owned(), "en".to_owned()],
            platform: "Win32".to_owned(),
            screen: contract::Screen {
                color_depth: 24,
                dpr: 1.0,
                height: 1440,
                width: 2560,
            },
            seed: "AB".repeat(32),
            user_agent: "Mozilla/5.0".to_owned(),
        }
    }

    fn check(summary: &PersonaSummary) -> Result<(u32, u32, (u32, u32)), String> {
        verify(
            Engine::Firefox,
            ClaimedOs::Windows,
            &request(),
            Some(CLASS),
            Some(152),
            summary,
        )
        .map(|(major, processors, screen)| (major, processors, (screen.width, screen.height)))
        .map_err(|error| {
            assert!(matches!(
                error,
                PersonaError::Generation {
                    engine: Engine::Firefox,
                    ..
                }
            ));
            error.to_string()
        })
    }

    #[test]
    fn what_the_sidecar_generated_is_what_was_asked_for() {
        assert_eq!(check(&summary()), Ok((152, 12, (2560, 1440))));
        // Without a class or a configured major there is nothing to hold
        // the screen or the major against.
        let mut free = summary();
        free.browser_major = 153;
        free.screen.width = 1600;
        assert!(
            verify(
                Engine::Firefox,
                ClaimedOs::Windows,
                &request(),
                None,
                None,
                &free
            )
            .is_ok()
        );
    }

    #[test]
    fn a_persona_that_is_not_the_one_asked_for_is_refused() {
        type Change = fn(&mut PersonaSummary);
        let cases: [(&str, Change); 7] = [
            ("chromium sidecar", |s| s.engine = EngineName::Chromium),
            ("claims macos", |s| {
                s.claimed_os = contract::ClaimedOs::Macos;
            }),
            ("another seed", |s| s.seed = "cd".repeat(32)),
            ("bundles major 153", |s| s.browser_major = 153),
            ("0 processors", |s| s.hardware_concurrency = 0),
            ("is no screen", |s| s.screen.width = u64::MAX),
            ("1920x1080, not the 2560x1440", |s| {
                s.screen.width = 1920;
                s.screen.height = 1080;
            }),
        ];
        for (expected, change) in cases {
            let mut summary = summary();
            change(&mut summary);
            let error = check(&summary).unwrap_err();
            assert!(error.contains(expected), "{expected}: {error}");
        }
        let mut huge = summary();
        huge.browser_major = u64::MAX;
        assert!(check(&huge).unwrap_err().contains("browser major"));
    }
}
