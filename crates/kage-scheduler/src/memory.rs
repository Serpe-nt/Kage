//! A [`Backend`] that keeps everything in memory.
//!
//! It is what the scheduler's own tests run on: under a paused clock a
//! real database would let time run ahead of its answers, and a day of
//! scheduling would take a day. It applies the same rules as the store
//! (the domain types' own), so what a test sees here is what Postgres
//! would hold.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_core::domain::{
    Attempt, DomainBreaker, FetchRecord, Persona, PersonaDomainState, RequestRecord,
};
use kage_core::ids::PersonaId;
use kage_core::outcome::Outcome;
use kage_core::persona::place;
use kage_core::schedule::breaker::{Admission, BreakerChange, BreakerRules};
use kage_core::schedule::planner::{DomainFacts, PersonaView};
use kage_core::schedule::selection::Network;
use kage_persona::identity::http_spec;

use crate::SchedulerError;
use crate::backend::Backend;

#[derive(Debug, Default)]
struct State {
    personas: BTreeMap<PersonaId, (Persona, bool, Network)>,
    facts: HashMap<String, DomainFacts>,
    egress_unsafe: bool,
    requests: Vec<RequestRecord>,
    attempts: Vec<Attempt>,
    records: Vec<FetchRecord>,
}

/// See the module documentation.
#[derive(Debug, Default)]
pub struct MemoryBackend {
    state: Mutex<State>,
}

impl MemoryBackend {
    /// An empty backend.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds a persona, or replaces it. `usable` says whether its proxy can
    /// carry traffic; `network` where its address sits.
    pub fn put_persona(&self, persona: Persona, usable: bool, network: Network) {
        self.state()
            .personas
            .insert(persona.id, (persona, usable, network));
    }

    /// Takes a persona away.
    pub fn remove_persona(&self, id: PersonaId) {
        self.state().personas.remove(&id);
    }

    /// Sets or clears the leak flag.
    pub fn set_egress_unsafe(&self, unsafe_now: bool) {
        self.state().egress_unsafe = unsafe_now;
    }

    /// What one persona has done on one domain.
    pub fn domain_state(&self, persona: PersonaId, domain: &str) -> Option<PersonaDomainState> {
        self.state()
            .facts
            .get(domain)
            .and_then(|facts| facts.states.get(&persona))
            .cloned()
    }

    /// Changes what one persona has done on one domain.
    pub fn with_domain_state<T>(
        &self,
        persona: PersonaId,
        domain: &str,
        change: impl FnOnce(&mut PersonaDomainState) -> T,
    ) -> T {
        let mut state = self.state();
        let entry = state
            .facts
            .entry(domain.to_owned())
            .or_default()
            .states
            .entry(persona)
            .or_insert_with(|| PersonaDomainState::new(persona, domain));
        change(entry)
    }

    /// The breaker of a domain, when it ever counted a block.
    pub fn breaker(&self, domain: &str) -> Option<DomainBreaker> {
        self.state()
            .facts
            .get(domain)
            .and_then(|facts| facts.breaker.clone())
    }

    /// Puts a breaker in place.
    pub fn put_breaker(&self, breaker: DomainBreaker) {
        let domain = breaker.domain.clone();
        self.state().facts.entry(domain).or_default().breaker = Some(breaker);
    }

    /// Every request that was stored, in order.
    pub fn requests(&self) -> Vec<RequestRecord> {
        self.state().requests.clone()
    }

    /// Every attempt that was stored, in order.
    pub fn attempts(&self) -> Vec<Attempt> {
        self.state().attempts.clone()
    }

    /// Every result that was stored, in order.
    pub fn records(&self) -> Vec<FetchRecord> {
        self.state().records.clone()
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn personas(&self) -> Result<Vec<PersonaView>, SchedulerError> {
        Ok(self
            .state()
            .personas
            .values()
            .map(|(persona, usable, network)| PersonaView {
                id: persona.id,
                seed: persona.seed,
                engine: persona.engine,
                country: persona.country,
                timezone: persona.timezone.clone(),
                ready: persona.is_ready() && *usable,
                http: http_spec(persona).is_ok(),
                network: network.clone(),
            })
            .collect())
    }

    async fn persona(&self, id: PersonaId) -> Result<Option<Persona>, SchedulerError> {
        Ok(self
            .state()
            .personas
            .get(&id)
            .map(|(persona, ..)| persona.clone()))
    }

    async fn facts(&self, domain: &str) -> Result<DomainFacts, SchedulerError> {
        Ok(self.state().facts.get(domain).cloned().unwrap_or_default())
    }

    async fn egress_unsafe(&self) -> Result<bool, SchedulerError> {
        Ok(self.state().egress_unsafe)
    }

    async fn dispatched(
        &self,
        persona: PersonaId,
        domain: &str,
        trial: Option<BreakerRules>,
        at: DateTime<Utc>,
    ) -> Result<(), SchedulerError> {
        let mut state = self.state();
        let zone = state.personas.get(&persona).map_or_else(
            || "UTC".to_owned(),
            |(persona, ..)| persona.timezone.clone(),
        );
        let today = place::local_day(at, &zone);
        let facts = state.facts.entry(domain.to_owned()).or_default();
        facts
            .states
            .entry(persona)
            .or_insert_with(|| PersonaDomainState::new(persona, domain))
            .record_request(at, today);
        if let (Some(rules), Some(breaker)) = (trial, facts.breaker.as_mut())
            && breaker.admission(at, &rules) == Admission::Trial
        {
            breaker.begin_trial(at);
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
        self.with_domain_state(persona, domain, |state| {
            state.record_outcome(outcome, at, cooldown);
        });
        Ok(())
    }

    async fn breaker_block(
        &self,
        domain: &str,
        rules: &BreakerRules,
        at: DateTime<Utc>,
    ) -> Result<BreakerChange, SchedulerError> {
        Ok(self
            .state()
            .facts
            .entry(domain.to_owned())
            .or_default()
            .breaker
            .get_or_insert_with(|| DomainBreaker::closed(domain, at))
            .record_block(at, rules))
    }

    async fn breaker_success(
        &self,
        domain: &str,
        at: DateTime<Utc>,
    ) -> Result<BreakerChange, SchedulerError> {
        Ok(self
            .state()
            .facts
            .get_mut(domain)
            .and_then(|facts| facts.breaker.as_mut())
            .map_or(BreakerChange::Unchanged, |breaker| {
                breaker.record_success(at)
            }))
    }

    async fn save_request(&self, request: &RequestRecord) -> Result<(), SchedulerError> {
        self.state().requests.push(request.clone());
        Ok(())
    }

    async fn save_attempt(&self, attempt: &Attempt) -> Result<(), SchedulerError> {
        self.state().attempts.push(attempt.clone());
        Ok(())
    }

    async fn finish(&self, record: &FetchRecord) -> Result<(), SchedulerError> {
        let mut state = self.state();
        if let Some(request) = state
            .requests
            .iter_mut()
            .find(|request| request.id == record.request_id)
        {
            request.outcome = Some(record.outcome);
            request.finished_at = Some(record.created_at);
        }
        state.records.push(record.clone());
        Ok(())
    }
}
