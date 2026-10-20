//! The bandwidth budget of the proxy plan, kept (decision D5).
//!
//! The forwarder counts every byte per persona and per domain being
//! fetched. This service writes those counts to the store, by the hour,
//! and judges the budget of the billing term: on the larger of what Kage
//! counted and what the provider itself reports for the term, because the
//! provider also bills what Kage does not see (health probes, a CLI
//! self-test).
//!
//! - At the warning share (80 %) `bandwidth_budget_warning` is raised,
//!   once per term, with the day the cap would be reached at the rate so
//!   far. Nothing else changes.
//! - At the cap, with the hard stop on, everything stops:
//!   `bandwidth_budget_exhausted` is raised once per term, the scheduler
//!   refuses every dispatch with `bandwidth_exhausted`, the forwarder
//!   refuses new connections, and every open browser is closed. Health
//!   probes go on: they do not pass the forwarder. The stop ends with the
//!   term, or when the operator raises the cap.
//! - With the hard stop off Kage only alerts.

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use kage_core::alert::{Alert, AlertKind, AlertSeverity, AlertSink};
use kage_core::domain::{BandwidthUsage, Exit, hour_of};
use kage_core::schedule::budget::{Budget, BudgetLevel, BudgetRules, assess};
use kage_proxy::{Clock, Pool};
use kage_store::Store;
use serde_json::json;

use crate::SchedulerError;
use crate::forwarder::{Counted, Meter};
use crate::metrics;

/// How often the provider is asked for its own total.
const PROVIDER_EVERY: Duration = Duration::from_mins(10);

/// The billing term the budget is counted over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Term {
    /// Its start.
    pub start: DateTime<Utc>,
    /// Its end.
    pub end: DateTime<Utc>,
}

/// Where the counts are kept and where the provider's own total comes
/// from.
#[async_trait]
pub trait Ledger: Send + Sync + fmt::Debug {
    /// The term that contains `now`.
    fn term(&self, now: DateTime<Utc>) -> Term;

    /// Adds counted bytes to what is stored.
    async fn add(&self, usage: &[BandwidthUsage]) -> Result<(), SchedulerError>;

    /// Everything Kage counted through the pool from `since` on: what the
    /// host's own connection carries costs nothing of the budget.
    async fn counted_since(&self, since: DateTime<Utc>) -> Result<u64, SchedulerError>;

    /// What the provider counted for the whole account from `since` on;
    /// `None` when it reports nothing.
    async fn provider_since(&self, since: DateTime<Utc>) -> Result<Option<u64>, SchedulerError>;

    /// Whether an alert of this kind was already raised from `since` on.
    async fn raised_since(&self, kind: &str, since: DateTime<Utc>) -> Result<bool, SchedulerError>;
}

/// The ledger over the store and the pool.
#[derive(Debug, Clone)]
pub struct StoreLedger {
    store: Store,
    pool: Arc<Pool>,
}

impl StoreLedger {
    /// A ledger over the store and the pool.
    pub const fn new(store: Store, pool: Arc<Pool>) -> Self {
        Self { store, pool }
    }
}

#[async_trait]
impl Ledger for StoreLedger {
    fn term(&self, now: DateTime<Utc>) -> Term {
        let (start, _) = self.pool.budget_window(now);
        Term {
            start,
            end: self.pool.budget_window_end(now),
        }
    }

    async fn add(&self, usage: &[BandwidthUsage]) -> Result<(), SchedulerError> {
        Ok(self.store.bandwidth().add(usage).await?)
    }

    async fn counted_since(&self, since: DateTime<Utc>) -> Result<u64, SchedulerError> {
        Ok(self
            .store
            .bandwidth()
            .total_since(since, Exit::Pool)
            .await?
            .total())
    }

    async fn provider_since(&self, since: DateTime<Utc>) -> Result<Option<u64>, SchedulerError> {
        Ok(self.pool.provider_traffic_since(since).await?)
    }

    async fn raised_since(&self, kind: &str, since: DateTime<Utc>) -> Result<bool, SchedulerError> {
        Ok(self.store.health_events().exists_since(kind, since).await?)
    }
}

#[derive(Debug, Default)]
struct Memory {
    /// The provider's total, the term it is for, and when it was asked.
    provider: Option<(DateTime<Utc>, Option<u64>, DateTime<Utc>)>,
    /// Counts that could not be written yet.
    unwritten: Vec<Counted>,
    /// The terms whose alerts were raised, by kind.
    warned: Option<DateTime<Utc>>,
    exhausted: Option<DateTime<Utc>>,
    last: Option<Budget>,
}

/// See the module documentation.
pub struct Bandwidth {
    meter: Arc<Meter>,
    ledger: Arc<dyn Ledger>,
    rules: BudgetRules,
    alerts: Arc<dyn AlertSink>,
    clock: Arc<dyn Clock>,
    memory: Mutex<Memory>,
}

impl fmt::Debug for Bandwidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bandwidth")
            .field("rules", &self.rules)
            .field("ledger", &self.ledger)
            .finish_non_exhaustive()
    }
}

impl Bandwidth {
    /// The budget over a forwarder's meter.
    pub fn new(
        meter: Arc<Meter>,
        ledger: Arc<dyn Ledger>,
        rules: BudgetRules,
        alerts: Arc<dyn AlertSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            meter,
            ledger,
            rules,
            alerts,
            clock,
            memory: Mutex::new(Memory::default()),
        }
    }

    fn memory(&self) -> std::sync::MutexGuard<'_, Memory> {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The budget as it was last judged.
    pub fn last(&self) -> Option<Budget> {
        self.memory().last
    }

    /// Writes what the forwarder counted since the last call to the
    /// store, under the hour that is now. Counts that cannot be written
    /// are kept and written with the next ones.
    ///
    /// # Errors
    ///
    /// Fails when the store fails; nothing is lost then.
    pub async fn flush(&self) -> Result<(), SchedulerError> {
        let drained = self.meter.drain();
        for egress in [Exit::Pool, Exit::Host] {
            let (up, down) = drained
                .iter()
                .filter(|counted| counted.egress == egress)
                .fold((0_u64, 0_u64), |(up, down), counted| {
                    (up + counted.bytes_up, down + counted.bytes_down)
                });
            // The pool's series is there from the start, as before 2.0; the
            // host's appears with its first byte.
            if egress == Exit::Pool || up + down > 0 {
                metrics::bandwidth(egress, up, down);
            }
        }
        let pending: Vec<Counted> = {
            let mut memory = self.memory();
            memory.unwritten.extend(drained);
            memory.unwritten.clone()
        };
        if pending.is_empty() {
            return Ok(());
        }
        let hour = hour_of(self.clock.now());
        let usage: Vec<BandwidthUsage> = pending
            .iter()
            .map(|counted| BandwidthUsage {
                hour,
                persona_id: counted.persona,
                egress: counted.egress,
                domain: counted.domain.clone(),
                bytes_up: counted.bytes_up,
                bytes_down: counted.bytes_down,
            })
            .collect();
        self.ledger.add(&usage).await?;
        // Only what was written is forgotten: more may have been counted
        // meanwhile.
        self.memory().unwritten.drain(..pending.len());
        Ok(())
    }

    /// Judges the budget of the term that is now, raises what is due, and
    /// says where it stands. [`Budget::stop`] is the hard stop.
    ///
    /// # Errors
    ///
    /// Fails when the store fails. A provider that cannot be asked is
    /// logged, and the budget is judged on Kage's own count.
    pub async fn assess(&self) -> Result<Budget, SchedulerError> {
        let now = self.clock.now();
        let term = self.ledger.term(now);
        let counted = self.ledger.counted_since(term.start).await?;
        let provider = self.provider(term, now).await;
        let budget = assess(
            counted,
            provider,
            term.start,
            Some(term.end),
            now,
            &self.rules,
        );
        metrics::budget(budget.share);
        self.memory().last = Some(budget);

        if budget.level != BudgetLevel::Ok {
            self.warn_once(&budget, term).await?;
        }
        if budget.level == BudgetLevel::Exhausted {
            self.exhausted_once(&budget, term).await?;
        }
        Ok(budget)
    }

    /// The provider's own total for the term, asked at most every ten
    /// minutes.
    async fn provider(&self, term: Term, now: DateTime<Utc>) -> Option<u64> {
        let cached = self.memory().provider;
        if let Some((for_term, bytes, asked)) = cached
            && for_term == term.start
            && (now - asked).to_std().is_ok_and(|age| age < PROVIDER_EVERY)
        {
            return bytes;
        }
        match self.ledger.provider_since(term.start).await {
            Ok(bytes) => {
                self.memory().provider = Some((term.start, bytes, now));
                bytes
            }
            Err(error) => {
                tracing::warn!(%error, "the provider's traffic figures cannot be read");
                // What it said last still counts for this term.
                cached
                    .filter(|(for_term, ..)| *for_term == term.start)
                    .and_then(|(_, bytes, _)| bytes)
            }
        }
    }

    fn details(budget: &Budget, term: Term) -> serde_json::Value {
        json!({
            "used_bytes": budget.used_bytes,
            "counted_bytes": budget.counted_bytes,
            "provider_bytes": budget.provider_bytes,
            "cap_bytes": budget.cap_bytes,
            "share": budget.share,
            "term_start": term.start,
            "term_end": term.end,
            "exhausted_at": budget.exhausted_at,
        })
    }

    async fn warn_once(&self, budget: &Budget, term: Term) -> Result<(), SchedulerError> {
        if self.memory().warned == Some(term.start) {
            return Ok(());
        }
        let kind = AlertKind::BandwidthBudgetWarning;
        if !self.ledger.raised_since(kind.as_str(), term.start).await? {
            let reached = budget.exhausted_at.map_or_else(
                || "at this rate the cap is not reached before the term ends".to_owned(),
                |at| {
                    format!(
                        "at this rate the cap is reached on {}",
                        at.format("%Y-%m-%d")
                    )
                },
            );
            self.alerts.raise(
                Alert::new(
                    kind,
                    AlertSeverity::Warning,
                    format!(
                        "{:.0} % of the proxy plan's bandwidth for this term is used ({:.1} of \
                         {:.0} GB); {reached}",
                        budget.share * 100.0,
                        gigabytes(budget.used_bytes),
                        gigabytes(budget.cap_bytes),
                    ),
                    Self::details(budget, term),
                )
                .at(self.clock.now()),
            );
        }
        self.memory().warned = Some(term.start);
        Ok(())
    }

    async fn exhausted_once(&self, budget: &Budget, term: Term) -> Result<(), SchedulerError> {
        if self.memory().exhausted == Some(term.start) {
            return Ok(());
        }
        let kind = AlertKind::BandwidthBudgetExhausted;
        if !self.ledger.raised_since(kind.as_str(), term.start).await? {
            let consequence = if budget.stop {
                "nothing is fetched until the term ends or bandwidth.monthly_cap_gb is raised"
            } else {
                "bandwidth.hard_stop is off: fetching continues"
            };
            self.alerts.raise(
                Alert::new(
                    kind,
                    AlertSeverity::Critical,
                    format!(
                        "the proxy plan's bandwidth for this term is used up ({:.1} of {:.0} GB); \
                         {consequence}",
                        gigabytes(budget.used_bytes),
                        gigabytes(budget.cap_bytes),
                    ),
                    Self::details(budget, term),
                )
                .at(self.clock.now()),
            );
        }
        self.memory().exhausted = Some(term.start);
        Ok(())
    }

    /// Flushes and judges every `interval` until `shutdown` resolves, and
    /// tells `on_budget` where the budget stands each time: that is where
    /// the hard stop is applied. A last flush is made at the end.
    pub async fn run(
        self: Arc<Self>,
        interval: Duration,
        mut on_budget: impl FnMut(Budget) + Send,
        shutdown: impl Future<Output = ()> + Send,
    ) {
        tokio::pin!(shutdown);
        loop {
            if let Err(error) = self.flush().await {
                tracing::warn!(%error, "the bandwidth counts could not be written");
            }
            match self.assess().await {
                Ok(budget) => on_budget(budget),
                Err(error) => tracing::warn!(%error, "the bandwidth budget could not be judged"),
            }
            tokio::select! {
                () = &mut shutdown => break,
                () = tokio::time::sleep(interval) => {}
            }
        }
        if let Err(error) = self.flush().await {
            tracing::warn!(%error, "the last bandwidth counts could not be written");
        }
    }
}

fn gigabytes(bytes: u64) -> f64 {
    // Byte counts of a proxy plan are far below 2^53.
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    bytes / 1e9
}
