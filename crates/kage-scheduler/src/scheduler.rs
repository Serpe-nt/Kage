//! The scheduler: the planner with a clock, a store and waiting callers
//! (spec §11).
//!
//! Whoever wants to make an attempt asks for a [`Grant`] with
//! [`Scheduler::acquire`] and waits; it never busy-waits. One loop
//! ([`Scheduler::run`]) plans: when work arrives, when an attempt ends,
//! and at the moment the planner asked to be woken. Each pass shows the
//! planner what is stored about the personas and about the domains that
//! have work waiting, lets it decide, and answers the callers whose work
//! leaves or is refused.
//!
//! Before every dispatch the leak flag of the pool is read: while egress
//! is unsafe every waiting request is refused (spec §7.3), and so it is
//! while the bandwidth budget is used up.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_browser::{BrowserError, Navigation};
use kage_core::domain::{Persona, Render};
use kage_core::schedule::breaker::{Admission, BreakerRules};
use kage_core::schedule::planner::{
    Completion, Decision, Dispatch, GrantId, Held as PlannerHeld, PlainDispatch, Planner,
    PlannerRules, Refusal, Work,
};
use kage_core::schedule::politeness::Courtesy;
use kage_core::schedule::queue::{Priority, Ticket};
use kage_core::seed::Seed;
use kage_persona::{BrowserGate, Held, Judged, Permit, PersonaError, Purpose};
use kage_proxy::Clock;
use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;

use crate::SchedulerError;
use crate::backend::Backend;
use crate::forwarder::{Label, Meter};
use crate::metrics;
use crate::verdicts::Verdicts;

/// How often the personas are read again when nothing else happens.
const REFRESH: Duration = Duration::from_secs(5);

/// How long the loop waits after a pass that failed.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(1);

/// Why a request is answered without an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    /// Why.
    pub reason: Refusal,
    /// When it would be worth asking again, when that is known.
    pub retry_at: Option<DateTime<Utc>>,
}

enum Answer {
    Go(Grant),
    GoPlain(PlainGrant),
    Refused(Refused),
}

struct Inner {
    planner: Planner,
    waiting: HashMap<Ticket, oneshot::Sender<Answer>>,
    refreshed: Option<Instant>,
}

/// See the module documentation.
pub struct Scheduler {
    inner: Mutex<Inner>,
    poke: Notify,
    backend: Arc<dyn Backend>,
    clock: Arc<dyn Clock>,
    /// The bandwidth budget is used up and the hard stop is on.
    bandwidth_stop: AtomicBool,
    shutting_down: AtomicBool,
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

/// One attempt that may be made now. Report its end with
/// [`Grant::complete`]; a grant that is only dropped (its attempt was
/// cancelled) gives the persona and its slots back and says nothing about
/// the site.
pub struct Grant {
    dispatch: Dispatch,
    scheduler: Arc<Scheduler>,
    reported: bool,
}

impl fmt::Debug for Grant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grant")
            .field("dispatch", &self.dispatch)
            .finish_non_exhaustive()
    }
}

impl Grant {
    /// What the planner decided: the persona, the path, the page of the
    /// visit.
    pub const fn dispatch(&self) -> &Dispatch {
        &self.dispatch
    }

    /// Reports how the attempt ended.
    pub fn complete(mut self, completion: Completion) {
        self.reported = true;
        self.scheduler.complete(self.dispatch.grant, completion);
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        if !self.reported {
            self.scheduler.complete(
                self.dispatch.grant,
                Completion {
                    courtesy: Courtesy::NotTheSite,
                    blocked: false,
                },
            );
        }
    }
}

/// A plain client's attempt (level 0) that may be made now: no persona.
/// Report its end with [`PlainGrant::complete`]; a grant that is only
/// dropped gives its slots back and says nothing about the site.
pub struct PlainGrant {
    dispatch: PlainDispatch,
    scheduler: Arc<Scheduler>,
    reported: bool,
}

impl fmt::Debug for PlainGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlainGrant")
            .field("dispatch", &self.dispatch)
            .finish_non_exhaustive()
    }
}

impl PlainGrant {
    /// What the planner decided: the domain, and how long the work waited.
    pub const fn dispatch(&self) -> &PlainDispatch {
        &self.dispatch
    }

    /// Reports how the attempt ended: only what it says about the site's
    /// health counts (the pause after server errors).
    pub fn complete(mut self, courtesy: Courtesy) {
        self.reported = true;
        self.scheduler.complete(
            self.dispatch.grant,
            Completion {
                courtesy,
                blocked: false,
            },
        );
    }
}

impl Drop for PlainGrant {
    fn drop(&mut self) {
        if !self.reported {
            self.scheduler.complete(
                self.dispatch.grant,
                Completion {
                    courtesy: Courtesy::NotTheSite,
                    blocked: false,
                },
            );
        }
    }
}

/// Takes waiting work out of the queue when its caller stops waiting.
struct Waiting<'a> {
    scheduler: &'a Scheduler,
    ticket: Ticket,
    answered: bool,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if !self.answered {
            let mut inner = self.scheduler.inner();
            inner.planner.cancel(self.ticket);
            inner.waiting.remove(&self.ticket);
        }
    }
}

impl Scheduler {
    /// A scheduler with nothing waiting. `seed` draws the planner's ties
    /// and pauses.
    pub fn new(
        rules: PlannerRules,
        seed: Seed,
        backend: Arc<dyn Backend>,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        tracing::info!(%seed, "scheduler starting");
        Arc::new(Self {
            inner: Mutex::new(Inner {
                planner: Planner::new(rules, seed),
                waiting: HashMap::new(),
                refreshed: None,
            }),
            poke: Notify::new(),
            backend,
            clock,
            bandwidth_stop: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
        })
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The scheduler's clock.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// What the scheduler reads and writes outside itself.
    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// Stops every dispatch because the bandwidth budget is used up, or
    /// lets them go again.
    pub fn set_bandwidth_stop(&self, stop: bool) {
        if self.bandwidth_stop.swap(stop, Ordering::Relaxed) != stop {
            self.poke.notify_one();
        }
    }

    /// How many attempts of a priority wait.
    pub fn depth(&self, priority: Priority) -> usize {
        self.inner().planner.depth(priority)
    }

    /// Attempts in flight on a path.
    pub fn in_flight(&self, render: Render) -> u32 {
        self.inner().planner.in_flight(render)
    }

    /// Waits for a persona and its slot for one attempt.
    ///
    /// Dropping the future takes the work out of the queue.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the request is answered without an attempt.
    pub async fn acquire(self: &Arc<Self>, work: Work) -> Result<Grant, Refused> {
        match self.wait_for(work).await? {
            Answer::Go(grant) => Ok(grant),
            Answer::GoPlain(_) => Err(Refused {
                // Persona work is never dispatched as a plain client's.
                reason: Refusal::NoPersona,
                retry_at: None,
            }),
            Answer::Refused(refused) => Err(refused),
        }
    }

    /// Waits for the site's gates for one attempt of a plain client (level
    /// 0, spec §25): `work.plain` is set.
    ///
    /// Dropping the future takes the work out of the queue.
    ///
    /// # Errors
    ///
    /// [`Refused`] when the request is answered without an attempt.
    pub async fn acquire_plain(self: &Arc<Self>, work: Work) -> Result<PlainGrant, Refused> {
        let work = Work {
            plain: true,
            ..work
        };
        match self.wait_for(work).await? {
            Answer::GoPlain(grant) => Ok(grant),
            Answer::Go(_) => Err(Refused {
                reason: Refusal::NoPersona,
                retry_at: None,
            }),
            Answer::Refused(refused) => Err(refused),
        }
    }

    async fn wait_for(self: &Arc<Self>, work: Work) -> Result<Answer, Refused> {
        if self.shutting_down.load(Ordering::Relaxed) {
            return Err(Refused {
                reason: Refusal::ShuttingDown,
                retry_at: None,
            });
        }
        let priority = work.priority;
        let asked = Instant::now();
        let (sender, receiver) = oneshot::channel();
        let ticket = {
            let now = self.clock.now();
            let mut inner = self.inner();
            let ticket = inner.planner.submit(work, now);
            inner.waiting.insert(ticket, sender);
            ticket
        };
        self.poke.notify_one();
        let mut waiting = Waiting {
            scheduler: self,
            ticket,
            answered: false,
        };
        let answer = receiver.await;
        waiting.answered = true;
        metrics::waited(priority, asked.elapsed());
        answer.map_err(|_| Refused {
            reason: Refusal::ShuttingDown,
            retry_at: None,
        })
    }

    fn complete(&self, grant: GrantId, completion: Completion) {
        let now = self.clock.now();
        self.inner().planner.complete(grant, now, completion);
        self.poke.notify_one();
    }

    /// One pass: shows the planner what is stored, lets it decide, and
    /// answers the callers whose work leaves or is refused.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails; nothing was decided then.
    pub async fn pass(self: &Arc<Self>) -> Result<(), SchedulerError> {
        let stale = self
            .inner()
            .refreshed
            .is_none_or(|at| at.elapsed() >= REFRESH);
        let waiting_domains = self.inner().planner.domains_waiting();
        if stale || !waiting_domains.is_empty() {
            let personas = self.backend.personas().await?;
            let mut inner = self.inner();
            inner.planner.set_personas(personas);
            inner.refreshed = Some(Instant::now());
        }
        // Before every dispatch: is egress safe, is there budget left?
        let stop = if self.shutting_down.load(Ordering::Relaxed) {
            Some(Refusal::ShuttingDown)
        } else if !waiting_domains.is_empty() && self.backend.egress_unsafe().await? {
            Some(Refusal::EgressUnsafe)
        } else if self.bandwidth_stop.load(Ordering::Relaxed) {
            Some(Refusal::BandwidthExhausted)
        } else {
            None
        };
        let mut facts = HashMap::new();
        for domain in waiting_domains {
            let known = self.backend.facts(&domain).await?;
            facts.insert(domain, known);
        }

        let now = self.clock.now();
        let decisions = {
            let mut inner = self.inner();
            inner.planner.set_stop(stop);
            inner.planner.plan(now, &facts)
        };
        for decision in decisions {
            match decision {
                Decision::Dispatch(dispatch) => self.leave(dispatch, now).await,
                Decision::Plain(dispatch) => {
                    // Nothing of a persona's is counted for it.
                    let ticket = dispatch.work;
                    let grant = PlainGrant {
                        dispatch,
                        scheduler: Arc::clone(self),
                        reported: false,
                    };
                    let sender = self.inner().waiting.remove(&ticket);
                    if let Some(sender) = sender {
                        // A caller that stopped waiting drops the grant.
                        let _ = sender.send(Answer::GoPlain(grant));
                    }
                }
                Decision::Refuse {
                    work,
                    reason,
                    retry_at,
                } => {
                    metrics::refused(reason);
                    if let Some(sender) = self.inner().waiting.remove(&work) {
                        // A caller that stopped waiting needs no answer.
                        let _ = sender.send(Answer::Refused(Refused { reason, retry_at }));
                    }
                }
            }
        }
        let inner = self.inner();
        metrics::queue(
            inner.planner.depth(Priority::Sync),
            inner.planner.depth(Priority::Job),
        );
        metrics::in_flight(
            inner.planner.in_flight(Render::Browser),
            inner.planner.in_flight(Render::Http),
        );
        drop(inner);
        Ok(())
    }

    /// Books an attempt that leaves and hands it to its caller.
    async fn leave(self: &Arc<Self>, dispatch: Dispatch, now: DateTime<Utc>) {
        let ticket = dispatch.work;
        let grant = Grant {
            dispatch,
            scheduler: Arc::clone(self),
            reported: false,
        };
        let dispatch = grant.dispatch();
        // Counted before the next pass plans: the daily cap and the trial
        // of a half-open breaker hold for whatever is decided next.
        let trial = dispatch.trial.then_some(dispatch.breaker);
        let counted = self
            .backend
            .dispatched(dispatch.persona, &dispatch.domain, trial, now)
            .await;
        let sender = self.inner().waiting.remove(&ticket);
        match (counted, sender) {
            (Ok(()), Some(sender)) => {
                // A caller that stopped waiting drops the grant, which
                // gives the persona back.
                let _ = sender.send(Answer::Go(grant));
            }
            (Err(error), sender) => {
                tracing::warn!(%error, "an attempt could not be counted and does not leave");
                drop(grant);
                if let Some(sender) = sender {
                    let _ = sender.send(Answer::Refused(Refused {
                        reason: Refusal::NoPersona,
                        retry_at: None,
                    }));
                }
            }
            (Ok(()), None) => drop(grant),
        }
    }

    /// Runs the scheduler until `shutdown` resolves: a pass whenever work
    /// arrives or an attempt ends, and at the moment the planner asked to
    /// be woken. Whatever still waits then is refused.
    pub async fn run(self: Arc<Self>, shutdown: impl Future<Output = ()> + Send) {
        tokio::pin!(shutdown);
        loop {
            let pause = match self.pass().await {
                Ok(()) => {
                    let wake = self.inner().planner.wake_at();
                    wake.map_or(REFRESH, |at| {
                        (at - self.clock.now()).to_std().unwrap_or(Duration::ZERO)
                    })
                    .min(REFRESH)
                }
                Err(error) => {
                    tracing::warn!(%error, "a scheduler pass failed");
                    RETRY_AFTER_FAILURE
                }
            };
            tokio::select! {
                () = &mut shutdown => break,
                () = self.poke.notified() => {}
                () = tokio::time::sleep(pause) => {}
            }
        }
        self.shutting_down.store(true, Ordering::Relaxed);
        let waiting: Vec<_> = self.inner().waiting.drain().collect();
        for (ticket, sender) in waiting {
            self.inner().planner.cancel(ticket);
            let _ = sender.send(Answer::Refused(Refused {
                reason: Refusal::ShuttingDown,
                retry_at: None,
            }));
        }
    }
}

/// A persona and a browser session held for the manager's own work.
struct Reserved {
    scheduler: Arc<Scheduler>,
    grant: GrantId,
    /// What the persona's bytes count toward meanwhile.
    _label: Option<Label>,
}

impl fmt::Debug for Reserved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reserved")
            .field("grant", &self.grant)
            .finish_non_exhaustive()
    }
}

impl Permit for Reserved {}

impl Drop for Reserved {
    fn drop(&mut self) {
        self.scheduler.complete(
            self.grant,
            Completion {
                courtesy: Courtesy::NotTheSite,
                blocked: false,
            },
        );
    }
}

/// The scheduler as the persona manager's own browser work sees it
/// (follow-up F27): warm-up visits and self-tests ask here before they
/// take a persona's browser.
#[derive(Debug)]
pub struct SchedulerGate {
    scheduler: Arc<Scheduler>,
    verdicts: Arc<Verdicts>,
    meter: Option<Arc<Meter>>,
}

impl SchedulerGate {
    /// The gate of a scheduler. With a `meter` a warm-up visit's bytes
    /// count toward the site it visits.
    pub const fn new(
        scheduler: Arc<Scheduler>,
        verdicts: Arc<Verdicts>,
        meter: Option<Arc<Meter>>,
    ) -> Self {
        Self {
            scheduler,
            verdicts,
            meter,
        }
    }

    /// The breaker rules of a site a warm-up visit goes to.
    fn breaker_rules(&self, domain: &str) -> BreakerRules {
        self.verdicts.breaker_rules(domain)
    }
}

#[async_trait]
impl BrowserGate for SchedulerGate {
    async fn reserve(
        &self,
        persona: &Persona,
        purpose: Purpose<'_>,
    ) -> Result<Box<dyn Permit>, Held> {
        let scheduler = &self.scheduler;
        let (honour_stop, domain) = match purpose {
            Purpose::Warmup { domain } => (true, Some(domain)),
            Purpose::Selftest { operator } => (!operator, None),
        };
        if honour_stop {
            // The manager's work reads the leak flag like every dispatch.
            let unsafe_now = scheduler.backend.egress_unsafe().await.unwrap_or(true);
            if unsafe_now {
                return Err(Held::Stopped(Refusal::EgressUnsafe.as_str()));
            }
            if scheduler.bandwidth_stop.load(Ordering::Relaxed) {
                return Err(Held::Stopped(Refusal::BandwidthExhausted.as_str()));
            }
        }
        if let Some(domain) = domain {
            // A warm-up visit waits while the site is paused, and is never
            // the trial that decides whether it stays paused.
            let breaker = scheduler
                .backend
                .facts(domain)
                .await
                .ok()
                .and_then(|facts| facts.breaker);
            let now = scheduler.clock.now();
            match breaker.map(|breaker| breaker.admission(now, &self.breaker_rules(domain))) {
                None | Some(Admission::Flow) => {}
                Some(Admission::Trial) => return Err(Held::DomainPaused { until: None }),
                Some(Admission::Paused { until }) => return Err(Held::DomainPaused { until }),
            }
        }
        // An operator's self-test counts as a synchronous request; the
        // manager's own work comes after everything that waits.
        let asked_by_operator = matches!(purpose, Purpose::Selftest { operator: true });
        let reserved = {
            let mut inner = scheduler.inner();
            let known = if asked_by_operator {
                inner.planner.reserve_for_operator(persona.id)
            } else {
                inner.planner.reserve(persona.id, false)
            };
            if known == Err(PlannerHeld::UnknownPersona) {
                // A persona the planner has not been shown yet: at the
                // next pass it is.
                inner.refreshed = None;
            }
            known
        };
        match reserved {
            Ok(grant) => Ok(Box::new(Reserved {
                scheduler: Arc::clone(scheduler),
                grant,
                _label: self
                    .meter
                    .as_ref()
                    .zip(domain)
                    .map(|(meter, domain)| meter.label(persona.id, domain)),
            })),
            Err(PlannerHeld::PersonaBusy) => Err(Held::PersonaBusy),
            Err(PlannerHeld::Stopped(reason)) => Err(Held::Stopped(reason.as_str())),
            Err(PlannerHeld::NoSlot | PlannerHeld::UnknownPersona) => {
                scheduler.poke.notify_one();
                Err(Held::NoSession)
            }
        }
    }

    async fn judge(
        &self,
        persona: &Persona,
        domain: &str,
        navigation: Result<&Navigation, &BrowserError>,
    ) -> Result<Judged, PersonaError> {
        self.verdicts
            .judge_warmup(persona, domain, navigation)
            .await
            .map_err(|error| match error {
                SchedulerError::Store(error) => PersonaError::Store(error),
                SchedulerError::Pool(error) => PersonaError::Pool(error),
                SchedulerError::Persona(error) => error,
                other => PersonaError::Gate(other.to_string()),
            })
    }
}
