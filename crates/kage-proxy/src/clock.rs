//! The pool's source of wall-clock time, replaceable in tests.
//!
//! Timestamps the pool stores and compares come from a [`Clock`]; waits use
//! tokio's timer. In production the clock is the system clock. Tests that
//! pause tokio's timer use [`TokioClock`], which advances with it, so a test
//! can step through hours of pool life in milliseconds and still see
//! consistent timestamps.

use std::fmt;
use std::sync::{Mutex, PoisonError};

use chrono::{DateTime, Utc};

/// A source of the current time.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The current time.
    fn now(&self) -> DateTime<Utc>;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock that starts at a given time and then follows tokio's timer,
/// including a paused and manually advanced one.
#[derive(Debug, Clone, Copy)]
pub struct TokioClock {
    start: DateTime<Utc>,
    started: tokio::time::Instant,
}

impl TokioClock {
    /// A clock that reads `start` now.
    pub fn starting_at(start: DateTime<Utc>) -> Self {
        Self {
            start,
            started: tokio::time::Instant::now(),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> DateTime<Utc> {
        let elapsed =
            chrono::Duration::from_std(self.started.elapsed()).unwrap_or(chrono::Duration::MAX);
        self.start
            .checked_add_signed(elapsed)
            .unwrap_or(DateTime::<Utc>::MAX_UTC)
    }
}

/// A clock that only moves when told to.
#[derive(Debug)]
pub struct ManualClock {
    now: Mutex<DateTime<Utc>>,
}

impl ManualClock {
    /// A clock that reads `start` until it is moved.
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(start),
        }
    }

    /// Moves the clock forward.
    pub fn advance(&self, by: chrono::Duration) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now += by;
    }

    /// Sets the clock.
    pub fn set(&self, to: DateTime<Utc>) {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner) = to;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{TimeZone, Utc};

    use super::{Clock, ManualClock, SystemClock, TokioClock};

    #[test]
    fn the_system_clock_reads_the_current_time() {
        let before = Utc::now();
        let read = SystemClock.now();
        let after = Utc::now();

        assert!(before <= read && read <= after);
    }

    #[test]
    fn a_manual_clock_moves_only_when_told() {
        let start = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
        let clock = ManualClock::new(start);
        assert_eq!(clock.now(), start);
        assert_eq!(clock.now(), start);

        clock.advance(chrono::Duration::minutes(5));
        assert_eq!(clock.now(), start + chrono::Duration::minutes(5));

        clock.set(start);
        assert_eq!(clock.now(), start);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tokio_clock_follows_the_paused_timer() {
        let start = Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap();
        let clock = TokioClock::starting_at(start);
        assert_eq!(clock.now(), start);

        tokio::time::advance(Duration::from_hours(3)).await;
        assert_eq!(clock.now(), start + chrono::Duration::hours(3));

        tokio::time::sleep(Duration::from_secs(90)).await;
        assert_eq!(
            clock.now(),
            start + chrono::Duration::hours(3) + chrono::Duration::seconds(90)
        );
    }
}
