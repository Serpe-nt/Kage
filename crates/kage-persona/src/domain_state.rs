//! What one persona has done on one domain (spec §5, §6.3): cooldowns
//! after a block, the daily cap, and the block counters.
//!
//! The rules are the domain type's
//! ([`kage_core::domain::PersonaDomainState`]); this service gives them the
//! persona's own calendar and makes every change one transaction, so that
//! two requests counted at the same moment are both counted.
//!
//! - A block sets `cooldown_until`; the persona is not eligible for the
//!   domain until then. A running cooldown is never shortened.
//! - The daily cap counts requests per calendar day of the persona's own
//!   time zone. A persona that reached it is eligible again at its local
//!   midnight.
//! - `block_count_24h` counts blocks in the 24 hours that began with the
//!   first of them.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use kage_core::domain::{Persona, PersonaDomainState};
use kage_core::outcome::Outcome;
use kage_core::persona::place;
use kage_proxy::Clock;
use kage_store::{Store, StoreError};

/// Whether a persona may be used for a domain now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    /// It may.
    Eligible,
    /// It was blocked there and cools down.
    CoolingDown {
        /// When the cooldown ends.
        until: DateTime<Utc>,
    },
    /// It has used up its daily cap.
    CapReached {
        /// The persona's next local midnight.
        until: DateTime<Utc>,
    },
}

impl Eligibility {
    /// Whether the persona may be used.
    pub const fn is_eligible(self) -> bool {
        matches!(self, Self::Eligible)
    }

    /// When the persona may be used again; `None` when it may now.
    pub const fn until(self) -> Option<DateTime<Utc>> {
        match self {
            Self::Eligible => None,
            Self::CoolingDown { until } | Self::CapReached { until } => Some(until),
        }
    }
}

/// The per-domain state service.
#[derive(Debug, Clone)]
pub struct DomainStates {
    store: Store,
    clock: Arc<dyn Clock>,
}

impl DomainStates {
    /// A service over `store`.
    pub fn new(store: Store, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    /// Whether `state` lets `persona` be used at `now`, with `daily_cap`
    /// requests a day when the domain has one.
    pub fn judge(
        persona: &Persona,
        state: Option<&PersonaDomainState>,
        daily_cap: Option<u32>,
        now: DateTime<Utc>,
    ) -> Eligibility {
        let Some(state) = state else {
            return Eligibility::Eligible;
        };
        if let Some(until) = state.cooldown_until.filter(|until| *until > now) {
            return Eligibility::CoolingDown { until };
        }
        let today = place::local_day(now, &persona.timezone);
        if daily_cap.is_some_and(|cap| state.cap_reached(today, cap)) {
            return Eligibility::CapReached {
                until: place::next_local_midnight(now, &persona.timezone),
            };
        }
        Eligibility::Eligible
    }

    /// Whether a persona may be used for a domain now.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn eligibility(
        &self,
        persona: &Persona,
        domain: &str,
        daily_cap: Option<u32>,
    ) -> Result<Eligibility, StoreError> {
        let state = self
            .store
            .persona_domain_state()
            .get(persona.id, domain)
            .await?;
        Ok(Self::judge(
            persona,
            state.as_ref(),
            daily_cap,
            self.clock.now(),
        ))
    }

    /// Counts one request of a persona to a domain, on the calendar day of
    /// the persona's own time zone.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn record_request(
        &self,
        persona: &Persona,
        domain: &str,
    ) -> Result<PersonaDomainState, StoreError> {
        let now = self.clock.now();
        let today = place::local_day(now, &persona.timezone);
        self.store
            .persona_domain_state()
            .modify(persona.id, domain, |state| {
                state.record_request(now, today);
                state.clone()
            })
            .await
    }

    /// Records how an attempt of a persona on a domain ended. A block
    /// counts and, with a `cooldown`, keeps the persona away from the
    /// domain for that long.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn record_outcome(
        &self,
        persona: &Persona,
        domain: &str,
        outcome: Outcome,
        cooldown: Option<std::time::Duration>,
    ) -> Result<PersonaDomainState, StoreError> {
        let now = self.clock.now();
        let cooldown = cooldown.and_then(|cooldown| Duration::from_std(cooldown).ok());
        self.store
            .persona_domain_state()
            .modify(persona.id, domain, |state| {
                state.record_outcome(outcome, now, cooldown);
                state.clone()
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use kage_core::domain::{Engine, PersonaDomainState};
    use kage_core::outcome::Outcome;

    use super::{DomainStates, Eligibility};
    use crate::test_support::persona;

    #[test]
    fn a_cooldown_and_the_daily_cap_keep_a_persona_away() {
        let mut persona = persona(Engine::Firefox, 156);
        persona.timezone = "Europe/Berlin".to_owned();
        // 22:30 in Berlin on a summer evening.
        let now = Utc.with_ymd_and_hms(2026, 7, 14, 20, 30, 0).unwrap();
        let today = now.date_naive();
        let judge = |state: Option<&PersonaDomainState>, cap| {
            DomainStates::judge(&persona, state, cap, now)
        };

        assert_eq!(judge(None, Some(1)), Eligibility::Eligible);
        let mut state = PersonaDomainState::new(persona.id, "example.org");
        assert!(judge(Some(&state), Some(1)).is_eligible());

        // Two requests today against a cap of two: out until local
        // midnight, which is 22:00 UTC in summer.
        state.record_request(now, today);
        assert!(judge(Some(&state), Some(2)).is_eligible());
        state.record_request(now, today);
        let capped = judge(Some(&state), Some(2));
        assert_eq!(
            capped,
            Eligibility::CapReached {
                until: Utc.with_ymd_and_hms(2026, 7, 14, 22, 0, 0).unwrap()
            }
        );
        assert_eq!(capped.until(), Some(now + Duration::minutes(90)));
        // No cap: nothing to reach.
        assert_eq!(judge(Some(&state), None), Eligibility::Eligible);
        assert_eq!(Eligibility::Eligible.until(), None);
        // After the persona's midnight the count is another day's.
        let tomorrow = now + Duration::hours(2);
        assert!(DomainStates::judge(&persona, Some(&state), Some(2), tomorrow).is_eligible());

        // A block cools the domain, and that outranks the cap.
        state.record_outcome(Outcome::Forbidden, now, Some(Duration::hours(24)));
        assert_eq!(
            judge(Some(&state), Some(2)),
            Eligibility::CoolingDown {
                until: now + Duration::hours(24)
            }
        );
        // The cooldown is over a day later; the cap of that day is fresh.
        let later = now + Duration::hours(24);
        assert!(DomainStates::judge(&persona, Some(&state), Some(2), later).is_eligible());
    }

    #[test]
    fn the_cap_follows_the_personas_own_calendar() {
        let mut tokyo = persona(Engine::Chromium, 155);
        tokyo.timezone = "Asia/Tokyo".to_owned();
        // 16:00 UTC on the 14th is 01:00 on the 15th in Tokyo and 18:00 on
        // the 14th in Berlin.
        let now = Utc.with_ymd_and_hms(2026, 7, 14, 16, 0, 0).unwrap();
        let mut state = PersonaDomainState::new(tokyo.id, "example.org");
        // Counted fifteen hours earlier: the 14th in both places.
        let earlier = now - Duration::hours(15);
        state.record_request(earlier, earlier.date_naive());
        assert!(DomainStates::judge(&tokyo, Some(&state), Some(1), now).is_eligible());
        // The same count on a Berlin persona is still today's.
        let mut berlin = persona(Engine::Chromium, 155);
        berlin.timezone = "Europe/Berlin".to_owned();
        let mut state = PersonaDomainState::new(berlin.id, "example.org");
        state.record_request(earlier, earlier.date_naive());
        assert!(!DomainStates::judge(&berlin, Some(&state), Some(1), now).is_eligible());
    }
}
