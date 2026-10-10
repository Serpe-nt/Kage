//! The bandwidth budget (decision D5): the forwarder's counts written by
//! the hour, the warning at 80 % once per term, and the hard stop.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration as Span, TimeZone, Utc};
use common::{id, noon};
use kage_core::alert::{AlertKind, AlertSeverity, AlertSink, MemoryAlertSink};
use kage_core::domain::{BandwidthUsage, IDLE_DOMAIN};
use kage_core::schedule::budget::{Budget, BudgetLevel, BudgetRules};
use kage_proxy::{Clock, TokioClock};
use kage_scheduler::{Bandwidth, Ledger, Meter, SchedulerError, Term};

#[derive(Debug, Default)]
struct Kept {
    rows: Vec<BandwidthUsage>,
    /// What the provider says it counted; `Err` when it cannot be asked.
    provider: Option<Result<u64, ()>>,
    provider_asked: u32,
    store_down: bool,
}

/// A ledger in memory, over calendar months, that finds raised alerts in
/// the sink the budget raises them to, as the store does.
#[derive(Debug)]
struct MonthLedger {
    kept: Mutex<Kept>,
    alerts: Arc<MemoryAlertSink>,
}

impl MonthLedger {
    fn kept(&self) -> std::sync::MutexGuard<'_, Kept> {
        self.kept.lock().unwrap()
    }

    fn total(&self) -> u64 {
        self.kept().rows.iter().map(BandwidthUsage::total).sum()
    }
}

fn month_start(at: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0)
        .unwrap()
}

#[async_trait]
impl Ledger for MonthLedger {
    fn term(&self, now: DateTime<Utc>) -> Term {
        let start = month_start(now);
        Term {
            start,
            end: month_start(start + Span::days(32)),
        }
    }

    async fn add(&self, usage: &[BandwidthUsage]) -> Result<(), SchedulerError> {
        let mut kept = self.kept();
        if kept.store_down {
            return Err(SchedulerError::ShuttingDown);
        }
        kept.rows.extend_from_slice(usage);
        Ok(())
    }

    async fn counted_since(&self, since: DateTime<Utc>) -> Result<u64, SchedulerError> {
        let kept = self.kept();
        if kept.store_down {
            return Err(SchedulerError::ShuttingDown);
        }
        Ok(kept
            .rows
            .iter()
            .filter(|row| row.hour >= since)
            .map(BandwidthUsage::total)
            .sum())
    }

    async fn provider_since(&self, _since: DateTime<Utc>) -> Result<Option<u64>, SchedulerError> {
        let mut kept = self.kept();
        kept.provider_asked += 1;
        match kept.provider {
            None => Ok(None),
            Some(Ok(bytes)) => Ok(Some(bytes)),
            Some(Err(())) => Err(SchedulerError::ShuttingDown),
        }
    }

    async fn raised_since(&self, kind: &str, since: DateTime<Utc>) -> Result<bool, SchedulerError> {
        Ok(self
            .alerts
            .alerts()
            .iter()
            .any(|alert| alert.kind.as_str() == kind && alert.at >= since))
    }
}

struct Books {
    meter: Arc<Meter>,
    ledger: Arc<MonthLedger>,
    alerts: Arc<MemoryAlertSink>,
    clock: Arc<dyn Clock>,
}

impl Books {
    fn new() -> Self {
        let alerts = Arc::new(MemoryAlertSink::new());
        Self {
            meter: Arc::new(Meter::default()),
            ledger: Arc::new(MonthLedger {
                kept: Mutex::new(Kept::default()),
                alerts: Arc::clone(&alerts),
            }),
            alerts,
            clock: Arc::new(TokioClock::starting_at(noon())),
        }
    }

    /// The budget service over these books; a second one is what a restart
    /// leaves: the same store, nothing remembered.
    fn bandwidth(&self, cap_bytes: u64, hard_stop: bool) -> Arc<Bandwidth> {
        let ledger: Arc<dyn Ledger> = self.ledger.clone();
        let alerts: Arc<dyn AlertSink> = self.alerts.clone();
        Arc::new(Bandwidth::new(
            Arc::clone(&self.meter),
            ledger,
            BudgetRules {
                cap_bytes,
                warn_at: 0.8,
                hard_stop,
            },
            alerts,
            Arc::clone(&self.clock),
        ))
    }
}

#[tokio::test(start_paused = true)]
async fn counts_are_written_by_the_hour_per_persona_and_domain() {
    let books = Books::new();
    let bandwidth = books.bandwidth(1_000_000, true);
    assert_eq!(bandwidth.last(), None);

    // Nothing counted: nothing written.
    bandwidth.flush().await.unwrap();
    assert_eq!(books.ledger.kept().rows, []);

    {
        let _fetching = books.meter.label(id(0), "shop.example");
        books.meter.add(id(0), 1_000, 9_000);
    }
    books.meter.add(id(0), 10, 20);
    books.meter.add(id(1), 300, 700);
    bandwidth.flush().await.unwrap();

    let hour = Utc.with_ymd_and_hms(2026, 7, 14, 10, 0, 0).unwrap();
    let row = |persona, domain: &str, bytes_up, bytes_down| BandwidthUsage {
        hour,
        persona_id: id(persona),
        domain: domain.to_owned(),
        bytes_up,
        bytes_down,
    };
    assert_eq!(
        books.ledger.kept().rows,
        [
            row(0, IDLE_DOMAIN, 10, 20),
            row(0, "shop.example", 1_000, 9_000),
            row(1, IDLE_DOMAIN, 300, 700),
        ]
    );
    // What was written is not written again.
    bandwidth.flush().await.unwrap();
    assert_eq!(books.ledger.kept().rows.len(), 3);
    assert_eq!(books.meter.totals(), (1_310, 9_720));

    let budget = bandwidth.assess().await.unwrap();
    assert_eq!(budget.counted_bytes, 11_030);
    assert_eq!(budget.provider_bytes, None);
    assert_eq!(budget.used_bytes, 11_030);
    assert_eq!(budget.level, BudgetLevel::Ok);
    assert!(!budget.stop);
    assert_eq!(bandwidth.last(), Some(budget));
    assert_eq!(books.alerts.alerts().len(), 0);
}

#[tokio::test(start_paused = true)]
async fn counts_that_cannot_be_written_are_kept_for_the_next_time() {
    let books = Books::new();
    let bandwidth = books.bandwidth(1_000_000, true);
    books.ledger.kept().store_down = true;
    books.meter.add(id(0), 100, 900);

    assert!(bandwidth.flush().await.is_err());
    assert!(bandwidth.assess().await.is_err());
    books.meter.add(id(1), 1, 2);
    assert!(bandwidth.flush().await.is_err());
    assert_eq!(books.ledger.total(), 0);

    books.ledger.kept().store_down = false;
    bandwidth.flush().await.unwrap();

    // Everything arrived, once.
    assert_eq!(books.ledger.total(), 1_003);
    bandwidth.flush().await.unwrap();
    assert_eq!(books.ledger.total(), 1_003);
}

#[tokio::test(start_paused = true)]
async fn the_warning_is_raised_once_per_term_with_the_day_the_cap_would_be_reached() {
    let books = Books::new();
    let bandwidth = books.bandwidth(1_000, true);
    books.meter.add(id(0), 50, 740);
    bandwidth.flush().await.unwrap();
    assert_eq!(bandwidth.assess().await.unwrap().level, BudgetLevel::Ok);
    assert_eq!(books.alerts.alerts().len(), 0);

    books.meter.add(id(0), 0, 60);
    bandwidth.flush().await.unwrap();
    let budget = bandwidth.assess().await.unwrap();

    // 850 of 1000 bytes after thirteen and a half days of July.
    assert_eq!(budget.level, BudgetLevel::Warning);
    assert!(!budget.stop);
    assert_eq!(books.alerts.kinds(), [AlertKind::BandwidthBudgetWarning]);
    let alert = &books.alerts.alerts()[0];
    assert_eq!(alert.severity, AlertSeverity::Warning);
    assert_eq!(alert.at, books.clock.now());
    assert!(alert.message.starts_with("85 % of"), "{}", alert.message);
    assert!(
        alert
            .message
            .ends_with("at this rate the cap is reached on 2026-07-16"),
        "{}",
        alert.message
    );
    assert_eq!(alert.details["used_bytes"], 850);
    assert_eq!(alert.details["cap_bytes"], 1_000);
    assert_eq!(alert.details["term_start"], "2026-07-01T00:00:00Z");
    assert_eq!(alert.details["term_end"], "2026-08-01T00:00:00Z");

    // Judged again, and again after a restart: no second warning.
    bandwidth.assess().await.unwrap();
    let restarted = books.bandwidth(1_000, true);
    assert_eq!(
        restarted.assess().await.unwrap().level,
        BudgetLevel::Warning
    );
    assert_eq!(books.alerts.alerts().len(), 1);

    // A new term starts at nothing, and warns again when it gets there.
    tokio::time::sleep(Duration::from_hours(24 * 20)).await;
    let budget = bandwidth.assess().await.unwrap();
    assert_eq!(budget.counted_bytes, 0);
    assert_eq!(budget.level, BudgetLevel::Ok);
    books.meter.add(id(0), 0, 900);
    bandwidth.flush().await.unwrap();
    assert_eq!(
        bandwidth.assess().await.unwrap().level,
        BudgetLevel::Warning
    );
    assert_eq!(books.alerts.kinds(), [AlertKind::BandwidthBudgetWarning; 2]);
    assert!(
        books.alerts.alerts()[1]
            .message
            .contains("the cap is reached on 2026-08-0"),
        "{}",
        books.alerts.alerts()[1].message
    );
}

#[tokio::test(start_paused = true)]
async fn at_the_cap_everything_stops_when_the_hard_stop_is_on() {
    let books = Books::new();
    let bandwidth = books.bandwidth(1_000, true);
    books.meter.add(id(0), 100, 900);
    bandwidth.flush().await.unwrap();

    let budget = bandwidth.assess().await.unwrap();

    assert_eq!(budget.level, BudgetLevel::Exhausted);
    assert!(budget.stop);
    // The cap was passed without the warning having been seen: both are
    // raised, each once.
    assert_eq!(
        books.alerts.kinds(),
        [
            AlertKind::BandwidthBudgetWarning,
            AlertKind::BandwidthBudgetExhausted
        ]
    );
    let alert = &books.alerts.alerts()[1];
    assert_eq!(alert.severity, AlertSeverity::Critical);
    assert!(
        alert.message.contains("nothing is fetched until"),
        "{}",
        alert.message
    );
    assert!(bandwidth.assess().await.unwrap().stop);
    assert!(books.bandwidth(1_000, true).assess().await.unwrap().stop);
    assert_eq!(books.alerts.alerts().len(), 2);

    // The operator raises the cap: the stop ends.
    let raised = books.bandwidth(5_000, true);
    let budget = raised.assess().await.unwrap();
    assert_eq!(budget.level, BudgetLevel::Ok);
    assert!(!budget.stop);
}

#[tokio::test(start_paused = true)]
async fn without_the_hard_stop_the_cap_only_alerts() {
    let books = Books::new();
    let bandwidth = books.bandwidth(1_000, false);
    books.meter.add(id(0), 100, 1_900);
    bandwidth.flush().await.unwrap();

    let budget = bandwidth.assess().await.unwrap();

    assert_eq!(budget.level, BudgetLevel::Exhausted);
    assert!(!budget.stop);
    let alert = &books.alerts.alerts()[1];
    assert_eq!(alert.kind, AlertKind::BandwidthBudgetExhausted);
    assert!(
        alert.message.contains("fetching continues"),
        "{}",
        alert.message
    );
}

#[tokio::test(start_paused = true)]
async fn the_budget_is_judged_on_the_larger_of_kages_count_and_the_providers() {
    let books = Books::new();
    let bandwidth = books.bandwidth(1_000, true);
    books.meter.add(id(0), 10, 90);
    bandwidth.flush().await.unwrap();
    // The provider also bills what does not pass the forwarder.
    books.ledger.kept().provider = Some(Ok(990));

    let budget = bandwidth.assess().await.unwrap();

    assert_eq!(budget.counted_bytes, 100);
    assert_eq!(budget.provider_bytes, Some(990));
    assert_eq!(budget.used_bytes, 990);
    assert_eq!(budget.level, BudgetLevel::Warning);

    // The provider is asked at most every ten minutes.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_mins(3)).await;
        bandwidth.assess().await.unwrap();
    }
    assert_eq!(books.ledger.kept().provider_asked, 1);
    tokio::time::sleep(Duration::from_mins(2)).await;
    bandwidth.assess().await.unwrap();
    assert_eq!(books.ledger.kept().provider_asked, 2);

    // When it cannot be asked, what it said last for the term stands.
    books.ledger.kept().provider = Some(Err(()));
    tokio::time::sleep(Duration::from_mins(11)).await;
    let budget = bandwidth.assess().await.unwrap();
    assert_eq!(books.ledger.kept().provider_asked, 3);
    assert_eq!(budget.provider_bytes, Some(990));

    // A service that never heard from it judges on its own count.
    let fresh = books.bandwidth(1_000, true);
    let budget = fresh.assess().await.unwrap();
    assert_eq!(budget.provider_bytes, None);
    assert_eq!(budget.used_bytes, 100);
    assert_eq!(budget.level, BudgetLevel::Ok);
}

#[tokio::test(start_paused = true)]
async fn the_service_flushes_and_judges_until_it_is_stopped() {
    let books = Books::new();
    let bandwidth = books.bandwidth(1_000, true);
    let seen: Arc<Mutex<Vec<Budget>>> = Arc::new(Mutex::new(Vec::new()));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = {
        let seen = Arc::clone(&seen);
        tokio::spawn(Arc::clone(&bandwidth).run(
            Duration::from_secs(30),
            move |budget| seen.lock().unwrap().push(budget),
            async move {
                let _ = stopped.await;
            },
        ))
    };

    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    books.meter.add(id(0), 100, 900);
    assert_eq!(books.ledger.total(), 0);
    tokio::time::sleep(Duration::from_secs(30)).await;

    // The next round wrote the count and found the budget used up.
    assert_eq!(books.ledger.total(), 1_000);
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(!seen[0].stop);
        assert!(seen[1].stop);
    }

    // A store that is down is tried again the next round.
    books.ledger.kept().store_down = true;
    books.meter.add(id(1), 1, 1);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(seen.lock().unwrap().len(), 2);
    books.ledger.kept().store_down = false;

    // What was counted before the stop is written at the stop.
    books.meter.add(id(1), 5, 5);
    stop.send(()).unwrap();
    running.await.unwrap();
    assert_eq!(books.ledger.total(), 1_012);
}
