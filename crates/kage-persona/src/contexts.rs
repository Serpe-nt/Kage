//! The one owner of every persona's browser context (follow-up F27).
//!
//! A persona has one context in its engine's sidecar, and three things
//! want it: real work, warm-up visits and self-tests. They all take it
//! here, as a [`Lease`]: while a lease is held nobody else touches the
//! persona's context, and nobody closes it under its holder.
//!
//! Kage owns closing (spec §10.6). A context stays open when its lease is
//! given back, so that a persona's next page finds its session cookies and
//! its connections; it is closed
//!
//! - by its holder, when the persona has no reason to keep its browser
//!   open (after a block, after a warm-up visit or a self-test);
//! - when it was not used for the idle limit ([`Contexts::close_idle`]);
//! - when another persona needs the engine's slot and none is free: the
//!   least recently used context that is not held goes, one whose persona
//!   is not in the middle of a visit before one whose persona is;
//! - all at once, at the hard stop and at shutdown ([`Contexts::close_all`]).
//!
//! The sidecar's own idle limit is the backstop, not the rule: a context it
//! closed by itself is simply opened again by the next lease.
//!
//! # The gate above it
//!
//! Who may take a persona *now* is decided one level up: the scheduler
//! knows what is in flight, what waits and whether anything may be fetched
//! at all. The manager's own work asks it through [`BrowserGate`] before
//! it takes a lease; in a process without a scheduler (a CLI command) there
//! is no gate and the lease is all there is.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_browser::{BrowserError, Navigation};
use kage_core::domain::{Engine, Persona, ProxyEndpoint};
use kage_core::ids::PersonaId;
use kage_core::outcome::Outcome;
use kage_proxy::Clock;
use tokio::sync::{Notify, OwnedMutexGuard};
use uuid::Uuid;

use crate::PersonaError;
use crate::sidecar::{Sidecars, open_request};

/// Who is given a persona's proxy: the two reach the forwarder from
/// different places.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The persona's browser, in a sidecar container.
    Browser,
    /// The persona's http client, in this process.
    Http,
}

/// Where a persona's traffic leaves the process: the proxy its browser and
/// its http client are given. With the forwarder that is the forwarder,
/// under a credential that names the persona; without one it is the
/// provider's proxy itself.
pub trait Egress: Send + Sync + fmt::Debug {
    /// The endpoint to hand out for `persona`, whose own proxy is
    /// `upstream`.
    fn endpoint(&self, persona: &Persona, upstream: &ProxyEndpoint, via: Via) -> ProxyEndpoint;
}

/// Straight to the provider's proxy: for a process without a forwarder.
#[derive(Debug, Clone, Copy, Default)]
pub struct DirectEgress;

impl Egress for DirectEgress {
    fn endpoint(&self, _persona: &Persona, upstream: &ProxyEndpoint, _via: Via) -> ProxyEndpoint {
        upstream.clone()
    }
}

/// What the manager wants a persona's browser for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose<'a> {
    /// A warm-up visit to a site.
    Warmup {
        /// The host that is visited.
        domain: &'a str,
    },
    /// A self-test.
    Selftest {
        /// Whether an operator asked for it; a scheduled one waits where
        /// an operator's goes ahead (the hard stop).
        operator: bool,
    },
}

/// Why the manager's own work cannot have a persona's browser now. It
/// waits and asks again; nothing here is an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Held {
    /// The persona is making an attempt.
    PersonaBusy,
    /// No browser session is free, or the last one is wanted by real work.
    NoSession,
    /// Nothing may be fetched: a leak is on record, or the bandwidth
    /// budget is used up.
    Stopped(&'static str),
    /// The site's circuit breaker is open.
    DomainPaused {
        /// When to ask again, when that is known.
        until: Option<DateTime<Utc>>,
    },
}

impl fmt::Display for Held {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PersonaBusy => f.write_str("the persona is making an attempt"),
            Self::NoSession => f.write_str("no browser session is free"),
            Self::Stopped(reason) => write!(f, "nothing may be fetched ({reason})"),
            Self::DomainPaused { until: Some(until) } => {
                write!(f, "the site is paused until {until}")
            }
            Self::DomainPaused { until: None } => f.write_str("the site is paused"),
        }
    }
}

/// A persona and a browser session, held for the manager's own work.
/// Dropping it gives both back.
pub trait Permit: Send + fmt::Debug {}

/// What a warm-up visit found, after everything that follows from it was
/// applied to the persona's state on the site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Judged {
    /// The outcome.
    pub outcome: Outcome,
    /// The status the site answered with, when it answered.
    pub status: Option<u16>,
}

/// The scheduler, as the manager's own browser work sees it.
#[async_trait]
pub trait BrowserGate: Send + Sync + fmt::Debug {
    /// Asks for a persona and a browser session. Never waits: what cannot
    /// be had now is [`Held`].
    async fn reserve(
        &self,
        persona: &Persona,
        purpose: Purpose<'_>,
    ) -> Result<Box<dyn Permit>, Held>;

    /// Judges what a warm-up visit found with the block signatures in
    /// force, and applies what follows as for any attempt: the outcome on
    /// the persona's state, a cooldown from the site's policy scaled by
    /// the signature's confidence, a count on the site's breaker.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    async fn judge(
        &self,
        persona: &Persona,
        domain: &str,
        navigation: Result<&Navigation, &BrowserError>,
    ) -> Result<Judged, PersonaError>;
}

#[derive(Debug)]
struct Open {
    context: Uuid,
    engine: Engine,
    last_used: DateTime<Utc>,
    /// Whether the persona is in the middle of a visit.
    visiting: bool,
    held: bool,
}

#[derive(Debug, Default)]
struct State {
    open: HashMap<PersonaId, Open>,
    locks: HashMap<PersonaId, Arc<tokio::sync::Mutex<()>>>,
}

/// See the module documentation.
#[derive(Debug)]
pub struct Contexts {
    sidecars: Sidecars,
    /// Contexts one engine's sidecar holds at once.
    capacity: usize,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    /// Signalled whenever a lease is given back or a context is closed.
    freed: Notify,
}

/// A persona's open context, held by one piece of work.
///
/// Give it back with [`Lease::keep`] or [`Lease::close`]. A lease that is
/// only dropped keeps the context open, as an attempt that was cancelled
/// would leave a window open.
#[derive(Debug)]
pub struct Lease {
    contexts: Arc<Contexts>,
    persona: PersonaId,
    engine: Engine,
    context: Uuid,
    reused: bool,
    returned: bool,
    _alone: OwnedMutexGuard<()>,
}

impl Lease {
    /// The context in the sidecar.
    pub const fn context(&self) -> Uuid {
        self.context
    }

    /// The engine whose sidecar holds it.
    pub const fn engine(&self) -> Engine {
        self.engine
    }

    /// Whether the context was open already: the persona's session is the
    /// one it had.
    pub const fn reused(&self) -> bool {
        self.reused
    }

    /// Gives the context back and leaves it open. `visiting` says whether
    /// the persona is in the middle of a visit: such a context is the last
    /// to be closed for another persona's sake.
    pub fn keep(mut self, visiting: bool) {
        self.returned = true;
        self.contexts.give_back(self.persona, visiting);
    }

    /// Closes the context: the browser exits and its profile is flushed.
    /// A context that cannot be closed is logged and forgotten; the
    /// sidecar's own idle limit takes it.
    pub async fn close(mut self) {
        self.returned = true;
        self.contexts
            .shut(self.persona, self.engine, self.context)
            .await;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.returned {
            self.contexts.give_back(self.persona, false);
        }
    }
}

impl Contexts {
    /// The contexts of the two sidecars, at most `capacity` open per
    /// engine.
    pub fn new(sidecars: Sidecars, capacity: u32, clock: Arc<dyn Clock>) -> Self {
        Self {
            sidecars,
            capacity: usize::try_from(capacity.max(1)).unwrap_or(usize::MAX),
            clock,
            state: Mutex::new(State::default()),
            freed: Notify::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The personas whose context is open, as far as Kage knows.
    pub fn open(&self) -> Vec<PersonaId> {
        let mut open: Vec<PersonaId> = self.state().open.keys().copied().collect();
        open.sort_unstable();
        open
    }

    /// How many contexts of an engine are open.
    pub fn open_on(&self, engine: Engine) -> usize {
        self.state()
            .open
            .values()
            .filter(|open| open.engine == engine)
            .count()
    }

    fn give_back(&self, persona: PersonaId, visiting: bool) {
        let now = self.clock.now();
        if let Some(open) = self.state().open.get_mut(&persona) {
            open.held = false;
            open.visiting = visiting;
            open.last_used = now;
        }
        self.freed.notify_waiters();
    }

    async fn shut(&self, persona: PersonaId, engine: Engine, context: Uuid) {
        if let Err(error) = self.sidecars.of(engine).close_context(context).await {
            tracing::warn!(%persona, %context, %error, "a context could not be closed");
        }
        let mut state = self.state();
        if state
            .open
            .get(&persona)
            .is_some_and(|open| open.context == context)
        {
            state.open.remove(&persona);
        }
        drop(state);
        self.freed.notify_waiters();
    }

    /// The context to close for `engine` to have room: the least recently
    /// used one nobody holds, one whose persona is not visiting first.
    fn victim(state: &State, engine: Engine) -> Option<(PersonaId, Uuid)> {
        state
            .open
            .iter()
            .filter(|(_, open)| open.engine == engine && !open.held)
            .min_by_key(|(persona, open)| (open.visiting, open.last_used, **persona))
            .map(|(persona, open)| (*persona, open.context))
    }

    /// Takes the persona's context for one piece of work, opening it when
    /// it is not open, through `endpoint`. Waits while another lease holds
    /// the persona, and while every context of the engine is held.
    ///
    /// # Errors
    ///
    /// A sidecar error when the context cannot be opened:
    /// `context_conflict` when it is open with other parameters than these
    /// (another process opened it), `context_limit` when the sidecar is
    /// full of contexts Kage does not know.
    pub async fn acquire(
        self: &Arc<Self>,
        persona: &Persona,
        endpoint: &ProxyEndpoint,
    ) -> Result<Lease, PersonaError> {
        let request = open_request(persona, endpoint)?;
        let lock = Arc::clone(self.state().locks.entry(persona.id).or_default());
        let alone = lock.lock_owned().await;
        let sidecar = self.sidecars.of(persona.engine);

        loop {
            // Wait for room before asking the sidecar, which would refuse.
            let freed = self.freed.notified();
            let evict = {
                let state = self.state();
                let known = state.open.contains_key(&persona.id);
                let open = state
                    .open
                    .values()
                    .filter(|open| open.engine == persona.engine)
                    .count();
                if known || open < self.capacity {
                    None
                } else {
                    Some(Self::victim(&state, persona.engine))
                }
            };
            match evict {
                None => break,
                Some(Some((other, context))) => {
                    tracing::debug!(persona = %other, %context, "closing a context to make room");
                    self.shut(other, persona.engine, context).await;
                }
                // Every context of the engine is held: wait for one.
                Some(None) => freed.await,
            }
        }

        // Opening is idempotent: an open context with these parameters is
        // returned as it is, one the sidecar closed by itself is opened
        // again.
        let opened = match sidecar.open_context(&request).await {
            Ok(opened) => opened,
            Err(error) => {
                // What Kage believed about this persona's context is void.
                let stale = self.state().open.remove(&persona.id);
                if let Some(stale) = stale.filter(|_| error.is_context_conflict()) {
                    // Open with the parameters of an earlier lease (the
                    // persona's place or proxy changed since): close that
                    // one and open with these.
                    self.shut(persona.id, persona.engine, stale.context).await;
                    match sidecar.open_context(&request).await {
                        Ok(opened) => opened,
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    self.freed.notify_waiters();
                    return Err(error.into());
                }
            }
        };
        let now = self.clock.now();
        self.state().open.insert(
            persona.id,
            Open {
                context: opened.context_id,
                engine: persona.engine,
                last_used: now,
                visiting: false,
                held: true,
            },
        );
        Ok(Lease {
            contexts: Arc::clone(self),
            persona: persona.id,
            engine: persona.engine,
            context: opened.context_id,
            reused: opened.reused,
            returned: false,
            _alone: alone,
        })
    }

    /// Closes the context of a persona when it is open and nobody holds
    /// it: for a persona that was retired or blocked. Returns whether one
    /// was closed.
    pub async fn close_of(&self, persona: PersonaId) -> bool {
        let found = {
            let state = self.state();
            state
                .open
                .get(&persona)
                .filter(|open| !open.held)
                .map(|open| (open.engine, open.context))
        };
        match found {
            Some((engine, context)) => {
                self.shut(persona, engine, context).await;
                true
            }
            None => false,
        }
    }

    /// Closes every context nobody holds that was not used since `before`.
    /// Returns how many.
    pub async fn close_idle(&self, before: DateTime<Utc>) -> usize {
        let idle: Vec<(PersonaId, Engine, Uuid)> = self
            .state()
            .open
            .iter()
            .filter(|(_, open)| !open.held && open.last_used <= before)
            .map(|(persona, open)| (*persona, open.engine, open.context))
            .collect();
        for (persona, engine, context) in &idle {
            self.shut(*persona, *engine, *context).await;
        }
        idle.len()
    }

    /// Closes every context nobody holds: at the hard stop and at
    /// shutdown. Returns how many.
    pub async fn close_all(&self) -> usize {
        self.close_idle(DateTime::<Utc>::MAX_UTC).await
    }
}
