//! The pause after server errors in a row (spec §8.3).
//!
//! A server error or a timeout says nothing about a persona: it cools no
//! one and the breaker does not count it. But a site that is simply down
//! must not be hammered by retries on persona after persona. So the
//! answers of a domain are watched over all personas: after a number of
//! server errors or timeouts in a row the domain is left alone for a short
//! while, twice as long with every further one, and any other answer ends
//! the row.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// What an attempt's end says about the site's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Courtesy {
    /// The site answered with a server error, or did not answer in time.
    ServerError,
    /// The site answered with anything else, a block included.
    Answered,
    /// The attempt failed before the site had a say: the proxy, the
    /// sidecar, the guard. It neither adds to the row nor ends it.
    NotTheSite,
}

/// When a domain is paused for server errors and for how long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerErrorRules {
    /// Errors in a row that start the pause.
    pub after: u32,
    /// The first pause.
    pub pause: Duration,
    /// The longest pause.
    pub max_pause: Duration,
}

/// The server errors of one domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerErrors {
    streak: u32,
    paused_until: Option<DateTime<Utc>>,
}

impl ServerErrors {
    /// Server errors in a row so far.
    pub const fn streak(&self) -> u32 {
        self.streak
    }

    /// When the domain may be asked again; `None` when it may be now.
    pub fn paused_until(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.paused_until.filter(|until| *until > now)
    }

    /// Records how an attempt ended at `now`. Returns the end of the pause
    /// this one started or extended.
    pub fn record(
        &mut self,
        courtesy: Courtesy,
        now: DateTime<Utc>,
        rules: &ServerErrorRules,
    ) -> Option<DateTime<Utc>> {
        match courtesy {
            Courtesy::NotTheSite => None,
            Courtesy::Answered => {
                *self = Self::default();
                None
            }
            Courtesy::ServerError => {
                self.streak = self.streak.saturating_add(1);
                if self.streak < rules.after.max(1) {
                    return None;
                }
                let doublings = self.streak - rules.after.max(1);
                let pause = 1_u32
                    .checked_shl(doublings)
                    .and_then(|factor| rules.pause.checked_mul(factor))
                    .unwrap_or(rules.max_pause)
                    .min(rules.max_pause);
                let until = chrono::Duration::from_std(pause)
                    .ok()
                    .and_then(|pause| now.checked_add_signed(pause))
                    .unwrap_or(DateTime::<Utc>::MAX_UTC);
                // An answer that arrives late never shortens a pause.
                let until = self
                    .paused_until
                    .map_or(until, |current| current.max(until));
                self.paused_until = Some(until);
                Some(until)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{DateTime, TimeZone, Utc};

    use super::{Courtesy, ServerErrorRules, ServerErrors};

    fn rules() -> ServerErrorRules {
        ServerErrorRules {
            after: 3,
            pause: Duration::from_secs(30),
            max_pause: Duration::from_mins(10),
        }
    }

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 10, 9, 0, 0).unwrap()
    }

    fn seconds(count: i64) -> chrono::Duration {
        chrono::Duration::seconds(count)
    }

    #[test]
    fn the_third_error_in_a_row_pauses_the_domain() {
        let rules = rules();
        let mut errors = ServerErrors::default();

        assert_eq!(errors.record(Courtesy::ServerError, start(), &rules), None);
        assert_eq!(errors.record(Courtesy::ServerError, start(), &rules), None);
        assert_eq!(errors.streak(), 2);
        assert_eq!(errors.paused_until(start()), None);

        let third = start() + seconds(5);
        assert_eq!(
            errors.record(Courtesy::ServerError, third, &rules),
            Some(third + seconds(30))
        );
        assert_eq!(errors.paused_until(third), Some(third + seconds(30)));
        assert_eq!(
            errors.paused_until(third + seconds(29)),
            Some(third + seconds(30))
        );
        // The pause is over at its end, not a second later.
        assert_eq!(errors.paused_until(third + seconds(30)), None);
    }

    #[test]
    fn every_further_error_doubles_the_pause_up_to_the_cap() {
        let rules = rules();
        let mut errors = ServerErrors::default();
        errors.record(Courtesy::ServerError, start(), &rules);
        errors.record(Courtesy::ServerError, start(), &rules);

        let mut at = start();
        for pause in [30, 60, 120, 240, 480, 600, 600] {
            let until = errors.record(Courtesy::ServerError, at, &rules).unwrap();
            assert_eq!(until, at + seconds(pause));
            at = until;
        }
        assert_eq!(errors.streak(), 9);
        // A row no shift holds is the cap too.
        for _ in 0..40 {
            at = errors.record(Courtesy::ServerError, at, &rules).unwrap();
        }
        assert_eq!(
            errors.record(Courtesy::ServerError, at, &rules),
            Some(at + seconds(600))
        );
    }

    #[test]
    fn any_other_answer_ends_the_row_and_the_pause() {
        let rules = rules();
        let mut errors = ServerErrors::default();
        for _ in 0..3 {
            errors.record(Courtesy::ServerError, start(), &rules);
        }
        assert!(errors.paused_until(start()).is_some());

        assert_eq!(errors.record(Courtesy::Answered, start(), &rules), None);

        assert_eq!(errors, ServerErrors::default());
        assert_eq!(errors.paused_until(start()), None);
        // The row starts again from nothing.
        assert_eq!(errors.record(Courtesy::ServerError, start(), &rules), None);
        assert_eq!(errors.streak(), 1);
    }

    #[test]
    fn a_failure_that_was_not_the_sites_changes_nothing() {
        let rules = rules();
        let mut errors = ServerErrors::default();
        errors.record(Courtesy::ServerError, start(), &rules);
        errors.record(Courtesy::ServerError, start(), &rules);
        let before = errors;

        assert_eq!(errors.record(Courtesy::NotTheSite, start(), &rules), None);

        assert_eq!(errors, before);
        // And it does not end the row either: the next error is the third.
        assert!(
            errors
                .record(Courtesy::ServerError, start(), &rules)
                .is_some()
        );
    }

    #[test]
    fn a_late_answer_never_shortens_a_pause() {
        let rules = rules();
        let mut errors = ServerErrors::default();
        for _ in 0..4 {
            errors.record(Courtesy::ServerError, start(), &rules);
        }
        // Paused for a minute from the fourth error.
        assert_eq!(errors.paused_until(start()), Some(start() + seconds(60)));

        // The fifth error is from an attempt that started long before and
        // is reported with an old time: the pause it would give ends
        // earlier than the one that runs.
        let old = start() - seconds(600);
        assert_eq!(
            errors.record(Courtesy::ServerError, old, &rules),
            Some(start() + seconds(60))
        );
    }

    #[test]
    fn a_threshold_of_one_pauses_on_the_first_error() {
        let rules = ServerErrorRules {
            after: 1,
            ..rules()
        };
        let mut errors = ServerErrors::default();

        assert_eq!(
            errors.record(Courtesy::ServerError, start(), &rules),
            Some(start() + seconds(30))
        );
        // A threshold of zero is read as one.
        let none = ServerErrorRules { after: 0, ..rules };
        let mut errors = ServerErrors::default();
        assert_eq!(
            errors.record(Courtesy::ServerError, start(), &none),
            Some(start() + seconds(30))
        );
    }
}
