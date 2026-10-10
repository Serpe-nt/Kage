//! The circuit breaker of a domain (spec §8.3).
//!
//! Blocks are counted per domain, over all personas, in the hour that
//! began with the first of them. At the policy's threshold the breaker
//! opens: the domain is paused and every request for it is answered at
//! once with `provider_error`, reason `domain_paused`, so that a site that
//! is turning the pool away is left alone instead of being fed the next
//! persona.
//!
//! When the pause is over the breaker is half open. One request is let
//! through as a trial, on one persona; everything else is still answered
//! `domain_paused`. The trial succeeds: the breaker closes and its counters
//! start again. The trial is blocked: the breaker opens again at once, for
//! twice as long as before, up to the scheduler's longest pause. A trial
//! that never reports back stops holding the breaker after its timeout,
//! and the next request is the trial.
//!
//! These are transitions on [`DomainBreaker`], the stored row; the
//! scheduler applies them inside a row lock.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::domain::{BreakerState, DomainBreaker};

/// The window blocks are counted in.
const WINDOW: chrono::Duration = chrono::Duration::hours(1);

/// When a breaker opens and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BreakerRules {
    /// Blocks in the window that open the breaker.
    pub threshold: u32,
    /// The first pause.
    pub pause: Duration,
    /// The longest pause.
    pub max_pause: Duration,
    /// How long a trial request holds the breaker without reporting back.
    pub trial_timeout: Duration,
}

/// What a breaker says to a request for its domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// The request may go.
    Flow,
    /// The pause is over and no trial is under way: this request may go as
    /// the trial.
    Trial,
    /// The domain is paused.
    Paused {
        /// When to ask again: the end of the pause, or of the wait for the
        /// trial that is under way. `None` for a pause without an end.
        until: Option<DateTime<Utc>>,
    },
}

/// What recording an outcome did to a breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerChange {
    /// Nothing a caller needs to know.
    Unchanged,
    /// The breaker opened: the domain is paused until then.
    Opened {
        /// End of the pause.
        until: DateTime<Utc>,
    },
    /// The breaker closed: the domain answers again.
    Closed,
}

fn chrono_of(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::MAX)
}

impl DomainBreaker {
    /// Whether the pause is over while the breaker is still stored open.
    fn half_open(&self, now: DateTime<Utc>) -> bool {
        self.state == BreakerState::Open && !self.is_open(now)
    }

    /// What the breaker says to a request at `now`.
    pub fn admission(&self, now: DateTime<Utc>, rules: &BreakerRules) -> Admission {
        if self.state == BreakerState::Closed {
            return Admission::Flow;
        }
        if self.is_open(now) {
            return Admission::Paused {
                until: self.paused_until,
            };
        }
        let trial_ends = self
            .trial_started_at
            .and_then(|started| started.checked_add_signed(chrono_of(rules.trial_timeout)));
        match trial_ends {
            Some(ends) if ends > now => Admission::Paused { until: Some(ends) },
            _ => Admission::Trial,
        }
    }

    /// Notes that the trial request left at `now`.
    pub fn begin_trial(&mut self, now: DateTime<Utc>) {
        self.trial_started_at = Some(now);
        self.updated_at = now;
    }

    /// The pause the breaker would open with now: the first pause, doubled
    /// for every time it opened since the domain last answered well, at
    /// most the longest pause.
    pub fn next_pause(&self, rules: &BreakerRules) -> Duration {
        let doubled = 1_u32
            .checked_shl(self.trips)
            .and_then(|factor| rules.pause.checked_mul(factor))
            .unwrap_or(rules.max_pause);
        doubled.min(rules.max_pause)
    }

    fn open(&mut self, now: DateTime<Utc>, rules: &BreakerRules) -> BreakerChange {
        let until = now
            .checked_add_signed(chrono_of(self.next_pause(rules)))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        self.state = BreakerState::Open;
        self.paused_until = Some(until);
        self.trips = self.trips.saturating_add(1);
        self.trial_started_at = None;
        self.updated_at = now;
        BreakerChange::Opened { until }
    }

    /// Counts one blocked attempt at `now`.
    pub fn record_block(&mut self, now: DateTime<Utc>, rules: &BreakerRules) -> BreakerChange {
        if self.half_open(now) {
            // The trial, or an attempt that left before the pause and came
            // back after it: the site still turns the pool away.
            self.block_count_1h = self.block_count_1h.saturating_add(1);
            return self.open(now, rules);
        }
        if self.state == BreakerState::Open {
            // An attempt that was under way when the breaker opened.
            self.block_count_1h = self.block_count_1h.saturating_add(1);
            self.updated_at = now;
            return BreakerChange::Unchanged;
        }
        let window_over = self
            .window_started_at
            .is_none_or(|started| now.signed_duration_since(started) >= WINDOW);
        if window_over {
            self.window_started_at = Some(now);
            self.block_count_1h = 0;
        }
        self.block_count_1h = self.block_count_1h.saturating_add(1);
        self.updated_at = now;
        if self.block_count_1h >= rules.threshold {
            return self.open(now, rules);
        }
        BreakerChange::Unchanged
    }

    /// Notes one attempt that the site answered well at `now`.
    pub fn record_success(&mut self, now: DateTime<Utc>) -> BreakerChange {
        if self.half_open(now) {
            *self = Self::closed(std::mem::take(&mut self.domain), now);
            return BreakerChange::Closed;
        }
        // While the pause runs, an attempt that left before it changes
        // nothing; a closed breaker has nothing to reset.
        BreakerChange::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeZone, Utc};

    use super::{Admission, BreakerChange, BreakerRules};
    use crate::domain::{BreakerState, DomainBreaker};

    fn rules() -> BreakerRules {
        BreakerRules {
            threshold: 5,
            pause: Duration::from_mins(30),
            max_pause: Duration::from_hours(6),
            trial_timeout: Duration::from_mins(10),
        }
    }

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 10, 9, 0, 0).unwrap()
    }

    fn minutes(count: i64) -> chrono::Duration {
        chrono::Duration::minutes(count)
    }

    /// A breaker that just opened for the first time at `start()`.
    fn opened() -> DomainBreaker {
        let mut breaker = DomainBreaker::closed("example.org", start());
        for _ in 0..4 {
            assert_eq!(
                breaker.record_block(start(), &rules()),
                BreakerChange::Unchanged
            );
        }
        assert_eq!(
            breaker.record_block(start(), &rules()),
            BreakerChange::Opened {
                until: start() + minutes(30)
            }
        );
        breaker
    }

    #[test]
    fn the_fifth_block_in_an_hour_opens_the_breaker() {
        let rules = rules();
        let mut breaker = DomainBreaker::closed("example.org", start());
        assert_eq!(breaker.admission(start(), &rules), Admission::Flow);

        for minute in 0..4 {
            let at = start() + minutes(minute * 10);
            assert_eq!(breaker.record_block(at, &rules), BreakerChange::Unchanged);
            assert_eq!(breaker.admission(at, &rules), Admission::Flow);
        }
        assert_eq!(breaker.block_count_1h, 4);
        assert_eq!(breaker.window_started_at, Some(start()));

        let fifth = start() + minutes(59);
        assert_eq!(
            breaker.record_block(fifth, &rules),
            BreakerChange::Opened {
                until: fifth + minutes(30)
            }
        );
        assert_eq!(breaker.state, BreakerState::Open);
        assert_eq!(breaker.block_count_1h, 5);
        assert_eq!(breaker.trips, 1);
        assert_eq!(breaker.updated_at, fifth);
        assert_eq!(
            breaker.admission(fifth + minutes(29), &rules),
            Admission::Paused {
                until: Some(fifth + minutes(30))
            }
        );
    }

    #[test]
    fn blocks_an_hour_apart_do_not_add_up() {
        let rules = rules();
        let mut breaker = DomainBreaker::closed("example.org", start());
        for _ in 0..4 {
            breaker.record_block(start(), &rules);
        }

        // The hour that began with the first block is over: a new window.
        let later = start() + minutes(60);
        assert_eq!(
            breaker.record_block(later, &rules),
            BreakerChange::Unchanged
        );
        assert_eq!(breaker.block_count_1h, 1);
        assert_eq!(breaker.window_started_at, Some(later));
        assert_eq!(breaker.state, BreakerState::Closed);

        // One second earlier it would have been the fifth.
        let mut breaker = DomainBreaker::closed("example.org", start());
        for _ in 0..4 {
            breaker.record_block(start(), &rules);
        }
        let just_inside = start() + minutes(60) - chrono::Duration::seconds(1);
        assert!(matches!(
            breaker.record_block(just_inside, &rules),
            BreakerChange::Opened { .. }
        ));
    }

    #[test]
    fn a_threshold_of_one_opens_on_the_first_block() {
        let rules = BreakerRules {
            threshold: 1,
            ..rules()
        };
        let mut breaker = DomainBreaker::closed("example.org", start());

        assert!(matches!(
            breaker.record_block(start(), &rules),
            BreakerChange::Opened { .. }
        ));
    }

    #[test]
    fn after_the_pause_one_trial_goes_and_everything_else_waits() {
        let rules = rules();
        let mut breaker = opened();
        let over = start() + minutes(30);

        assert_eq!(
            breaker.admission(over - chrono::Duration::seconds(1), &rules),
            Admission::Paused { until: Some(over) }
        );
        assert_eq!(breaker.admission(over, &rules), Admission::Trial);

        breaker.begin_trial(over);
        assert_eq!(breaker.trial_started_at, Some(over));
        assert_eq!(
            breaker.admission(over + minutes(1), &rules),
            Admission::Paused {
                until: Some(over + minutes(10))
            }
        );
        // A trial that never reports back holds the breaker for its
        // timeout and no longer.
        assert_eq!(
            breaker.admission(over + minutes(10), &rules),
            Admission::Trial
        );
    }

    #[test]
    fn a_trial_that_succeeds_closes_the_breaker_and_resets_it() {
        let rules = rules();
        let mut breaker = opened();
        let over = start() + minutes(31);
        breaker.begin_trial(over);

        assert_eq!(breaker.record_success(over), BreakerChange::Closed);

        assert_eq!(breaker, DomainBreaker::closed("example.org", over));
        assert_eq!(breaker.admission(over, &rules), Admission::Flow);
        // The next time it opens, the pause is the first one again.
        for _ in 0..4 {
            breaker.record_block(over, &rules);
        }
        assert_eq!(
            breaker.record_block(over, &rules),
            BreakerChange::Opened {
                until: over + minutes(30)
            }
        );
    }

    #[test]
    fn a_trial_that_is_blocked_reopens_for_twice_as_long_up_to_the_cap() {
        let rules = rules();
        let mut breaker = opened();
        let mut at = start() + minutes(30);
        // 30 minutes were the first pause; then 1 h, 2 h, 4 h, and 6 h
        // from there on.
        for (trips, pause) in [(2, 60), (3, 120), (4, 240), (5, 360), (6, 360)] {
            assert_eq!(breaker.admission(at, &rules), Admission::Trial);
            breaker.begin_trial(at);
            assert_eq!(
                breaker.record_block(at, &rules),
                BreakerChange::Opened {
                    until: at + minutes(pause)
                }
            );
            assert_eq!(breaker.trips, trips);
            assert_eq!(breaker.trial_started_at, None);
            assert_eq!(breaker.state, BreakerState::Open);
            at += minutes(pause);
        }
        assert_eq!(breaker.next_pause(&rules), Duration::from_hours(6));
        // A count that no shift holds is the cap as well.
        breaker.trips = 40;
        assert_eq!(breaker.next_pause(&rules), Duration::from_hours(6));
        breaker.trips = 0;
        assert_eq!(breaker.next_pause(&rules), Duration::from_mins(30));
        breaker.trips = 3;
        assert_eq!(breaker.next_pause(&rules), Duration::from_hours(4));
    }

    #[test]
    fn attempts_that_were_under_way_do_not_move_an_open_breaker() {
        let rules = rules();
        let mut breaker = opened();
        let during = start() + minutes(5);

        // A block that comes back during the pause is counted, no more.
        assert_eq!(
            breaker.record_block(during, &rules),
            BreakerChange::Unchanged
        );
        assert_eq!(breaker.block_count_1h, 6);
        assert_eq!(breaker.paused_until, Some(start() + minutes(30)));
        assert_eq!(breaker.trips, 1);
        // A success that comes back during the pause does not close it.
        assert_eq!(breaker.record_success(during), BreakerChange::Unchanged);
        assert_eq!(breaker.state, BreakerState::Open);
        assert!(breaker.is_open(during));
    }

    #[test]
    fn a_success_on_a_closed_breaker_changes_nothing() {
        let rules = rules();
        let mut breaker = DomainBreaker::closed("example.org", start());
        breaker.record_block(start(), &rules);
        let before = breaker.clone();

        assert_eq!(
            breaker.record_success(start() + minutes(1)),
            BreakerChange::Unchanged
        );
        // The blocks of the hour still count: one good answer among them
        // does not make the site friendly.
        assert_eq!(breaker, before);
    }

    #[test]
    fn a_pause_without_an_end_stays_until_someone_closes_it() {
        let rules = rules();
        let mut breaker = opened();
        breaker.paused_until = None;

        assert_eq!(
            breaker.admission(start() + minutes(100_000), &rules),
            Admission::Paused { until: None }
        );
    }
}
