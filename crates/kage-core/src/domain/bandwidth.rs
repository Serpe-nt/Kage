//! Bytes through the forwarder (decision D5, spec §18).

use chrono::{DateTime, DurationRound, TimeDelta, Utc};

use crate::ids::PersonaId;

/// The `domain` of bytes that passed outside any attempt: a browser's own
/// traffic while its window is open.
pub const IDLE_DOMAIN: &str = "(idle)";

/// What one persona exchanged with its proxy in one hour while one domain
/// was being fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandwidthUsage {
    /// Start of the hour, UTC.
    pub hour: DateTime<Utc>,
    /// The persona.
    pub persona_id: PersonaId,
    /// The host being fetched while the bytes passed, or [`IDLE_DOMAIN`].
    pub domain: String,
    /// Bytes sent towards the proxy.
    pub bytes_up: u64,
    /// Bytes received from the proxy.
    pub bytes_down: u64,
}

impl BandwidthUsage {
    /// Both directions together: what the provider bills.
    pub const fn total(&self) -> u64 {
        self.bytes_up.saturating_add(self.bytes_down)
    }
}

/// The start of the hour an instant falls in.
pub fn hour_of(at: DateTime<Utc>) -> DateTime<Utc> {
    at.duration_trunc(TimeDelta::hours(1)).unwrap_or(at)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::{BandwidthUsage, IDLE_DOMAIN, hour_of};
    use crate::ids::PersonaId;

    #[test]
    fn an_hour_starts_on_the_hour() {
        let at = Utc.with_ymd_and_hms(2026, 10, 10, 17, 59, 59).unwrap()
            + chrono::Duration::milliseconds(999);
        let hour = Utc.with_ymd_and_hms(2026, 10, 10, 17, 0, 0).unwrap();

        assert_eq!(hour_of(at), hour);
        assert_eq!(hour_of(hour), hour);
        assert_eq!(
            hour_of(hour - chrono::Duration::nanoseconds(1)),
            hour - chrono::Duration::hours(1)
        );
    }

    #[test]
    fn the_total_is_both_directions() {
        let usage = BandwidthUsage {
            hour: hour_of(Utc::now()),
            persona_id: PersonaId::new(),
            domain: IDLE_DOMAIN.to_owned(),
            bytes_up: 1200,
            bytes_down: 98_800,
        };

        assert_eq!(usage.total(), 100_000);
        assert_eq!(usage.domain, "(idle)");
        let huge = BandwidthUsage {
            bytes_up: u64::MAX,
            ..usage
        };
        assert_eq!(huge.total(), u64::MAX);
    }
}
