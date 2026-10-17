//! What the scheduler reads and writes outside itself: personas, their
//! state per domain, the breakers, and the record of requests.
//!
//! The scheduler talks to a [`Backend`], not to Postgres. [`StoreBackend`]
//! is the real one, over the store and the pool; [`crate::MemoryBackend`]
//! keeps everything in memory, for tests that run days of scheduling under
//! a paused clock, where a real database would let time run ahead of its
//! answers.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_core::domain::{
    Attempt, FetchRecord, Persona, PersonaState, Proxy, ProxyEndpoint, RequestRecord,
};
use kage_core::ids::{ApiKeyId, PersonaId};
use kage_core::outcome::Outcome;
use kage_core::persona::place;
use kage_core::schedule::breaker::{Admission, BreakerChange, BreakerRules};
use kage_core::schedule::planner::{DomainFacts, PersonaView};
use kage_core::schedule::selection::Network;
use kage_persona::identity::http_spec;
use kage_proxy::Pool;
use kage_store::Store;
use kage_store::repo::{ResponseCacheEntry, RobotsCacheEntry};
use tokio::time::Instant;

use crate::SchedulerError;
use crate::forwarder::{RouteRefusal, Routes};

/// How long the forwarder trusts what it found out about a persona's
/// proxy before it asks again.
const ROUTE_TTL: Duration = Duration::from_secs(5);

/// A result the response cache holds, with what it takes to serve it.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheHit {
    /// The entry.
    pub entry: ResponseCacheEntry,
    /// The result it points at.
    pub record: FetchRecord,
    /// The key whose request fetched the page; `None` for a request of
    /// Kage's own.
    pub filled_by: Option<ApiKeyId>,
}

/// See the module documentation.
#[async_trait]
pub trait Backend: Send + Sync + fmt::Debug {
    /// Every live persona as the planner needs it.
    async fn personas(&self) -> Result<Vec<PersonaView>, SchedulerError>;

    /// One persona, whole.
    async fn persona(&self, id: PersonaId) -> Result<Option<Persona>, SchedulerError>;

    /// What is stored about a domain.
    async fn facts(&self, domain: &str) -> Result<DomainFacts, SchedulerError>;

    /// Whether a proxy leaked the host's address and nothing may be
    /// fetched (spec §7.3).
    async fn egress_unsafe(&self) -> Result<bool, SchedulerError>;

    /// Counts a request of a persona to a domain that leaves at `at`, on
    /// the day of the persona's own zone, and notes the trial of a
    /// half-open breaker.
    async fn dispatched(
        &self,
        persona: PersonaId,
        domain: &str,
        trial: Option<BreakerRules>,
        at: DateTime<Utc>,
    ) -> Result<(), SchedulerError>;

    /// Records how an attempt of a persona on a domain ended, with the
    /// cooldown a block gives.
    async fn record_outcome(
        &self,
        persona: PersonaId,
        domain: &str,
        outcome: Outcome,
        cooldown: Option<Duration>,
        at: DateTime<Utc>,
    ) -> Result<(), SchedulerError>;

    /// Counts a block on the domain's breaker.
    async fn breaker_block(
        &self,
        domain: &str,
        rules: &BreakerRules,
        at: DateTime<Utc>,
    ) -> Result<BreakerChange, SchedulerError>;

    /// Tells the domain's breaker that its trial succeeded.
    async fn breaker_success(
        &self,
        domain: &str,
        at: DateTime<Utc>,
    ) -> Result<BreakerChange, SchedulerError>;

    /// Stores a request as it was accepted.
    async fn save_request(&self, request: &RequestRecord) -> Result<(), SchedulerError>;

    /// Stores a finished attempt.
    async fn save_attempt(&self, attempt: &Attempt) -> Result<(), SchedulerError>;

    /// Stores the result of a request and marks the request finished.
    async fn finish(&self, record: &FetchRecord) -> Result<(), SchedulerError>;

    /// What is kept of the `robots.txt` of an origin, however old.
    async fn robots(&self, origin: &str) -> Result<Option<RobotsCacheEntry>, SchedulerError>;

    /// Keeps what a scout brought back, in place of what was kept.
    async fn save_robots(&self, entry: &RobotsCacheEntry) -> Result<(), SchedulerError>;

    /// The persona that asks a site for what no reader asks for, when the
    /// site has one that lives.
    async fn scout(&self, site: &str) -> Result<Option<PersonaId>, SchedulerError>;

    /// Makes a persona the scout of a site.
    async fn set_scout(
        &self,
        site: &str,
        persona: PersonaId,
        at: DateTime<Utc>,
    ) -> Result<(), SchedulerError>;

    /// The personas that asked a host of a site for a page after `since`
    /// and are not its scout.
    async fn readers(
        &self,
        site: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<PersonaId>, SchedulerError>;

    /// The entry of the response cache under a key that has not expired
    /// by `now`, with its result.
    async fn cached(
        &self,
        cache_key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CacheHit>, SchedulerError>;

    /// Puts a result into the response cache, in place of what its key
    /// held.
    async fn cache(&self, entry: &ResponseCacheEntry) -> Result<(), SchedulerError>;
}

/// The address a persona's requests are seen from, when it is known.
fn exit_address(proxy: &Proxy) -> Option<IpAddr> {
    proxy
        .last_health
        .as_ref()
        .and_then(|health| health.exit_ip)
        .or_else(|| proxy.host.parse().ok())
}

/// A persona as the planner needs it, from its row and its proxy's.
pub fn view_of(persona: &Persona, proxy: Option<&Proxy>) -> PersonaView {
    let usable = proxy.is_some_and(|proxy| proxy.state.is_usable());
    let asn = proxy.and_then(|proxy| {
        proxy
            .observed_geo()
            .and_then(|geo| geo.asn.as_deref())
            .or(proxy.asn.as_deref())
    });
    PersonaView {
        id: persona.id,
        seed: persona.seed,
        engine: persona.engine,
        country: persona.country,
        timezone: persona.timezone.clone(),
        ready: persona.is_ready() && usable,
        http: http_spec(persona).is_ok(),
        network: Network::of(proxy.and_then(exit_address), asn),
    }
}

/// The backend over Postgres and the proxy pool.
#[derive(Debug, Clone)]
pub struct StoreBackend {
    store: Store,
    pool: Arc<Pool>,
}

impl StoreBackend {
    /// A backend over the store and the pool.
    pub const fn new(store: Store, pool: Arc<Pool>) -> Self {
        Self { store, pool }
    }
}

#[async_trait]
impl Backend for StoreBackend {
    async fn personas(&self) -> Result<Vec<PersonaView>, SchedulerError> {
        let proxies: HashMap<_, _> = self
            .pool
            .live()
            .await?
            .into_iter()
            .map(|proxy| (proxy.id, proxy))
            .collect();
        Ok(self
            .store
            .personas()
            .list_live()
            .await?
            .iter()
            .map(|persona| view_of(persona, proxies.get(&persona.proxy_id)))
            .collect())
    }

    async fn persona(&self, id: PersonaId) -> Result<Option<Persona>, SchedulerError> {
        Ok(self.store.personas().get(id).await?)
    }

    async fn facts(&self, domain: &str) -> Result<DomainFacts, SchedulerError> {
        let states = self
            .store
            .persona_domain_state()
            .list_for_domain(domain)
            .await?
            .into_iter()
            .map(|state| (state.persona_id, state))
            .collect();
        let breaker = self.store.domain_breakers().get(domain).await?;
        Ok(DomainFacts { states, breaker })
    }

    async fn egress_unsafe(&self) -> Result<bool, SchedulerError> {
        Ok(self.pool.egress_unsafe().await?)
    }

    async fn dispatched(
        &self,
        persona: PersonaId,
        domain: &str,
        trial: Option<BreakerRules>,
        at: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        // The day is the persona's own: its daily cap turns at its
        // midnight.
        let zone = self
            .store
            .personas()
            .get(persona)
            .await?
            .map_or_else(|| "UTC".to_owned(), |persona| persona.timezone);
        let today = place::local_day(at, &zone);
        self.store
            .persona_domain_state()
            .modify(persona, domain, |state| state.record_request(at, today))
            .await?;
        if let Some(rules) = trial {
            self.store
                .domain_breakers()
                .modify(domain, at, |breaker| {
                    if breaker.admission(at, &rules) == Admission::Trial {
                        breaker.begin_trial(at);
                    }
                })
                .await?;
        }
        Ok(())
    }

    async fn record_outcome(
        &self,
        persona: PersonaId,
        domain: &str,
        outcome: Outcome,
        cooldown: Option<Duration>,
        at: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        let cooldown = cooldown.and_then(|cooldown| chrono::Duration::from_std(cooldown).ok());
        self.store
            .persona_domain_state()
            .modify(persona, domain, |state| {
                state.record_outcome(outcome, at, cooldown);
            })
            .await?;
        Ok(())
    }

    async fn breaker_block(
        &self,
        domain: &str,
        rules: &BreakerRules,
        at: DateTime<Utc>,
    ) -> Result<BreakerChange, SchedulerError> {
        Ok(self
            .store
            .domain_breakers()
            .modify(domain, at, |breaker| breaker.record_block(at, rules))
            .await?)
    }

    async fn breaker_success(
        &self,
        domain: &str,
        at: DateTime<Utc>,
    ) -> Result<BreakerChange, SchedulerError> {
        // A domain that never counted a block has no breaker to tell.
        if self.store.domain_breakers().get(domain).await?.is_none() {
            return Ok(BreakerChange::Unchanged);
        }
        Ok(self
            .store
            .domain_breakers()
            .modify(domain, at, |breaker| breaker.record_success(at))
            .await?)
    }

    async fn save_request(&self, request: &RequestRecord) -> Result<(), SchedulerError> {
        Ok(self.store.requests().insert(request).await?)
    }

    async fn save_attempt(&self, attempt: &Attempt) -> Result<(), SchedulerError> {
        Ok(self.store.attempts().insert(attempt).await?)
    }

    async fn finish(&self, record: &FetchRecord) -> Result<(), SchedulerError> {
        self.store.results().insert(record).await?;
        self.store
            .requests()
            .finish(record.request_id, record.outcome, record.created_at)
            .await?;
        Ok(())
    }

    async fn robots(&self, origin: &str) -> Result<Option<RobotsCacheEntry>, SchedulerError> {
        Ok(self.store.robots_cache().get(origin).await?)
    }

    async fn save_robots(&self, entry: &RobotsCacheEntry) -> Result<(), SchedulerError> {
        Ok(self.store.robots_cache().put(entry).await?)
    }

    async fn scout(&self, site: &str) -> Result<Option<PersonaId>, SchedulerError> {
        Ok(self.store.site_scouts().get(site).await?)
    }

    async fn set_scout(
        &self,
        site: &str,
        persona: PersonaId,
        at: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        Ok(self.store.site_scouts().set(site, persona, at).await?)
    }

    async fn readers(
        &self,
        site: &str,
        since: DateTime<Utc>,
    ) -> Result<Vec<PersonaId>, SchedulerError> {
        Ok(self.store.site_scouts().readers(site, since).await?)
    }

    async fn cached(
        &self,
        cache_key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<CacheHit>, SchedulerError> {
        let Some(entry) = self.store.response_cache().get(cache_key, now).await? else {
            return Ok(None);
        };
        let Some(record) = self.store.results().get(entry.result_id).await? else {
            return Ok(None);
        };
        let filled_by = self
            .store
            .requests()
            .get(record.request_id)
            .await?
            .and_then(|request| request.api_key_id);
        Ok(Some(CacheHit {
            entry,
            record,
            filled_by,
        }))
    }

    async fn cache(&self, entry: &ResponseCacheEntry) -> Result<(), SchedulerError> {
        Ok(self.store.response_cache().put(entry).await?)
    }
}

/// A route as it was last looked up, and when.
type KnownRoute = (Instant, Result<ProxyEndpoint, RouteRefusal>);

/// The forwarder's routes over the store and the pool: a persona's own
/// proxy, when the persona is active and the proxy can carry traffic.
#[derive(Debug)]
pub struct PoolRoutes {
    store: Store,
    pool: Arc<Pool>,
    known: Mutex<HashMap<PersonaId, KnownRoute>>,
}

impl PoolRoutes {
    /// Routes over the store and the pool.
    pub fn new(store: Store, pool: Arc<Pool>) -> Self {
        Self {
            store,
            pool,
            known: Mutex::new(HashMap::new()),
        }
    }

    async fn look_up(&self, id: PersonaId) -> Result<ProxyEndpoint, RouteRefusal> {
        let unavailable = |error: &dyn fmt::Display| {
            tracing::warn!(persona = %id, %error, "the forwarder cannot look up a persona's proxy");
            RouteRefusal::Unavailable
        };
        let persona = self
            .store
            .personas()
            .get(id)
            .await
            .map_err(|error| unavailable(&error))?
            .ok_or(RouteRefusal::UnknownPersona)?;
        // A cooling or retired persona sends nothing; one that is still
        // warming up does: its warm-up visits.
        if persona.state != PersonaState::Active {
            return Err(RouteRefusal::PersonaNotActive);
        }
        let proxy = self
            .pool
            .get(persona.proxy_id)
            .await
            .map_err(|error| unavailable(&error))?
            .filter(|proxy| proxy.state.is_usable())
            .ok_or(RouteRefusal::ProxyNotUsable)?;
        self.pool
            .endpoint(&proxy)
            .map_err(|error| unavailable(&error))
    }
}

#[async_trait]
impl Routes for PoolRoutes {
    async fn upstream(&self, persona: PersonaId) -> Result<ProxyEndpoint, RouteRefusal> {
        let cached = self
            .known
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&persona)
            .filter(|(at, _)| at.elapsed() < ROUTE_TTL)
            .map(|(_, answer)| answer.clone());
        if let Some(answer) = cached {
            return answer;
        }
        let answer = self.look_up(persona).await;
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        // What could not be found out is asked again at once.
        if !matches!(answer, Err(RouteRefusal::Unavailable)) {
            known.insert(persona, (Instant::now(), answer.clone()));
        }
        known.retain(|_, (at, _)| at.elapsed() < ROUTE_TTL);
        drop(known);
        answer
    }
}
