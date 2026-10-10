//! The bandwidth budget of the proxy plan (decision D5).
//!
//! The plan includes a fixed amount of traffic per billing term. Kage
//! counts what passes through its forwarder, per persona and domain, and
//! reads the provider's own total for the term; the budget is judged on the
//! larger of the two, because the provider also bills what Kage does not
//! see. At a share of the cap an alert is raised, once per term, with the
//! day the cap would be reached at the rate so far. At the cap everything
//! stops when the hard stop is on.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::macros::string_enum;

/// The elapsed share of a term before a rate is worth projecting from.
const PROJECTION_MIN_SECONDS: i64 = 60 * 60;

string_enum! {
    /// Where the budget stands.
    pub enum BudgetLevel {
        /// Below the warning share.
        Ok => "ok",
        /// At or above the warning share, below the cap.
        Warning => "warning",
        /// At or above the cap.
        Exhausted => "exhausted",
    }
}

/// The cap and what happens near it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetRules {
    /// What the plan includes per term, in bytes.
    pub cap_bytes: u64,
    /// The share of the cap at which the warning is raised.
    pub warn_at: f64,
    /// Whether everything stops at the cap.
    pub hard_stop: bool,
}

/// The budget at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Budget {
    /// What Kage counted in the term, in bytes.
    pub counted_bytes: u64,
    /// What the provider reports for the term; `None` when it reports
    /// nothing.
    pub provider_bytes: Option<u64>,
    /// The larger of the two: what the budget is judged on.
    pub used_bytes: u64,
    /// The cap.
    pub cap_bytes: u64,
    /// `used_bytes` as a share of the cap.
    pub share: f64,
    /// Where that stands.
    pub level: BudgetLevel,
    /// Whether nothing may be fetched: the cap is reached and the hard stop
    /// is on.
    pub stop: bool,
    /// When the cap would be reached at the rate so far, if that is inside
    /// the term.
    pub exhausted_at: Option<DateTime<Utc>>,
}

/// Judges the budget of the term that began at `term_start`.
pub fn assess(
    counted_bytes: u64,
    provider_bytes: Option<u64>,
    term_start: DateTime<Utc>,
    term_end: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    rules: &BudgetRules,
) -> Budget {
    let used_bytes = counted_bytes.max(provider_bytes.unwrap_or(0));
    // Byte counts of a proxy plan are far below 2^53.
    #[allow(clippy::cast_precision_loss)]
    let (used, cap) = (used_bytes as f64, rules.cap_bytes as f64);
    let share = if rules.cap_bytes == 0 {
        1.0
    } else {
        used / cap
    };
    let level = if used_bytes >= rules.cap_bytes {
        BudgetLevel::Exhausted
    } else if share >= rules.warn_at {
        BudgetLevel::Warning
    } else {
        BudgetLevel::Ok
    };
    let elapsed = now.signed_duration_since(term_start).num_seconds();
    let exhausted_at = if level == BudgetLevel::Exhausted {
        Some(now)
    } else if used_bytes == 0 || elapsed < PROJECTION_MIN_SECONDS {
        None
    } else {
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let seconds_to_cap = (cap / used * elapsed as f64).ceil() as i64;
        chrono::Duration::try_seconds(seconds_to_cap)
            .and_then(|after| term_start.checked_add_signed(after))
            .filter(|at| term_end.is_none_or(|end| *at < end))
    };
    Budget {
        counted_bytes,
        provider_bytes,
        used_bytes,
        cap_bytes: rules.cap_bytes,
        share,
        level,
        stop: level == BudgetLevel::Exhausted && rules.hard_stop,
        exhausted_at,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Duration, TimeZone, Utc};

    use super::{BudgetLevel, BudgetRules, assess};
    use crate::macros::test_support::assert_string_enum;

    const GB: u64 = 1_000_000_000;

    fn rules() -> BudgetRules {
        BudgetRules {
            cap_bytes: 250 * GB,
            warn_at: 0.8,
            hard_stop: true,
        }
    }

    fn start() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()
    }

    fn end() -> Option<DateTime<Utc>> {
        start().checked_add_signed(Duration::days(30))
    }

    #[test]
    fn level_names_are_stable() {
        assert_string_enum(
            BudgetLevel::ALL,
            BudgetLevel::as_str,
            &["ok", "warning", "exhausted"],
        );
    }

    #[test]
    fn the_budget_is_judged_on_the_larger_count() {
        let now = start() + Duration::days(10);

        let own = assess(100 * GB, Some(60 * GB), start(), end(), now, &rules());
        assert_eq!(own.used_bytes, 100 * GB);
        assert_eq!(own.counted_bytes, 100 * GB);
        assert_eq!(own.provider_bytes, Some(60 * GB));
        assert!((own.share - 0.4).abs() < 1e-12);
        assert_eq!(own.level, BudgetLevel::Ok);
        assert!(!own.stop);

        // The provider bills more than Kage saw: its number counts.
        let theirs = assess(100 * GB, Some(210 * GB), start(), end(), now, &rules());
        assert_eq!(theirs.used_bytes, 210 * GB);
        assert_eq!(theirs.level, BudgetLevel::Warning);
        let alone = assess(7 * GB, None, start(), end(), now, &rules());
        assert_eq!(alone.used_bytes, 7 * GB);
        assert_eq!(alone.cap_bytes, 250 * GB);
    }

    #[test]
    fn eighty_percent_warns_and_the_cap_stops() {
        let now = start() + Duration::days(20);
        let at = |bytes| assess(bytes, None, start(), end(), now, &rules());

        assert_eq!(at(200 * GB - 1).level, BudgetLevel::Ok);
        assert_eq!(at(200 * GB).level, BudgetLevel::Warning);
        assert!(!at(200 * GB).stop);
        assert_eq!(at(250 * GB - 1).level, BudgetLevel::Warning);
        let full = at(250 * GB);
        assert_eq!(full.level, BudgetLevel::Exhausted);
        assert!(full.stop);
        assert_eq!(full.exhausted_at, Some(now));
        assert!((full.share - 1.0).abs() < 1e-12);
        assert_eq!(at(400 * GB).level, BudgetLevel::Exhausted);

        // With the switch off the cap only reports.
        let lenient = BudgetRules {
            hard_stop: false,
            ..rules()
        };
        let over = assess(300 * GB, None, start(), end(), now, &lenient);
        assert_eq!(over.level, BudgetLevel::Exhausted);
        assert!(!over.stop);
        // A warning share of zero warns from the first byte on; a cap of
        // zero is always exhausted.
        let eager = BudgetRules {
            warn_at: 0.0,
            ..rules()
        };
        assert_eq!(
            assess(0, None, start(), end(), now, &eager).level,
            BudgetLevel::Warning
        );
        let nothing = BudgetRules {
            cap_bytes: 0,
            ..rules()
        };
        let empty = assess(0, None, start(), end(), now, &nothing);
        assert_eq!(empty.level, BudgetLevel::Exhausted);
        assert!((empty.share - 1.0).abs() < 1e-12);
    }

    #[test]
    fn the_projection_names_the_day_the_cap_is_reached() {
        // 100 GB in ten days is 10 GB a day: 250 GB on day 25.
        let now = start() + Duration::days(10);
        let budget = assess(100 * GB, None, start(), end(), now, &rules());
        assert_eq!(budget.exhausted_at, Some(start() + Duration::days(25)));

        // 50 GB in ten days reaches the cap on day 50: not in this term.
        let slow = assess(50 * GB, None, start(), end(), now, &rules());
        assert_eq!(slow.exhausted_at, None);
        // Without a known end of the term the day is named all the same.
        let open = assess(50 * GB, None, start(), None, now, &rules());
        assert_eq!(open.exhausted_at, Some(start() + Duration::days(50)));
        // Exactly at the end of the term is not inside it.
        let edge = assess(
            100 * GB,
            None,
            start(),
            Some(start() + Duration::days(25)),
            now,
            &rules(),
        );
        assert_eq!(edge.exhausted_at, None);

        // Nothing used, or less than an hour of the term gone: no rate.
        assert_eq!(
            assess(0, None, start(), end(), now, &rules()).exhausted_at,
            None
        );
        let early = start() + Duration::minutes(59);
        assert_eq!(
            assess(GB, None, start(), end(), early, &rules()).exhausted_at,
            None
        );
        let hour = start() + Duration::hours(1);
        assert!(
            assess(GB, None, start(), end(), hour, &rules())
                .exhausted_at
                .is_some()
        );
    }
}
