//! Human-readable durations as written in configuration and policy files:
//! `500ms`, `30s`, `30m`, `2h`, `7d`.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Why a duration string was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurationParseError {
    /// The string is empty.
    #[error("duration is empty")]
    Empty,
    /// The numeric part is missing or is not a non-negative integer.
    #[error("duration {0:?} must start with a non-negative integer")]
    InvalidNumber(String),
    /// The unit is not one of `ms`, `s`, `m`, `h`, `d`.
    #[error("duration {0:?} needs a unit: ms, s, m, h or d")]
    InvalidUnit(String),
    /// The value does not fit in the supported range.
    #[error("duration {0:?} is too large")]
    Overflow(String),
}

/// A duration that reads and writes as `30m`, `2h` and so on.
///
/// Accepted units are `ms`, `s`, `m`, `h` and `d`. The value is a
/// non-negative integer; `0` may be written without a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct HumanDuration(Duration);

impl HumanDuration {
    /// Wraps a [`Duration`].
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// A duration of whole seconds.
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    /// A duration of whole minutes.
    pub const fn from_mins(mins: u64) -> Self {
        Self(Duration::from_secs(mins * 60))
    }

    /// A duration of whole hours.
    pub const fn from_hours(hours: u64) -> Self {
        Self(Duration::from_secs(hours * 3600))
    }

    /// The wrapped [`Duration`].
    pub const fn get(self) -> Duration {
        self.0
    }
}

impl From<Duration> for HumanDuration {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl From<HumanDuration> for Duration {
    fn from(duration: HumanDuration) -> Self {
        duration.0
    }
}

impl FromStr for HumanDuration {
    type Err = DurationParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.is_empty() {
            return Err(DurationParseError::Empty);
        }
        let digits_end = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (digits, unit) = text.split_at(digits_end);
        if digits.is_empty() {
            return Err(DurationParseError::InvalidNumber(text.to_owned()));
        }
        let value: u64 = digits
            .parse()
            .map_err(|_| DurationParseError::Overflow(text.to_owned()))?;

        let millis_per_unit: u64 = match unit.trim() {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "" if value == 0 => 1,
            _ => return Err(DurationParseError::InvalidUnit(text.to_owned())),
        };
        let millis = value
            .checked_mul(millis_per_unit)
            .ok_or_else(|| DurationParseError::Overflow(text.to_owned()))?;
        Ok(Self(Duration::from_millis(millis)))
    }
}

impl fmt::Display for HumanDuration {
    /// Writes the largest unit that represents the value exactly, so the
    /// output parses back to the same duration (rounded down to whole
    /// milliseconds).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let millis = self.0.as_millis();
        if millis == 0 {
            return f.write_str("0s");
        }
        for (unit, size) in [
            ("d", 86_400_000),
            ("h", 3_600_000),
            ("m", 60_000),
            ("s", 1_000),
        ] {
            if millis.is_multiple_of(size) {
                return write!(f, "{}{unit}", millis / size);
            }
        }
        write!(f, "{millis}ms")
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = HumanDuration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration such as \"30m\", \"2h\" or \"500ms\"")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }

            // A bare `0` in YAML arrives as a number.
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                if value == 0 {
                    Ok(HumanDuration::default())
                } else {
                    Err(E::custom(DurationParseError::InvalidUnit(
                        value.to_string(),
                    )))
                }
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                match u64::try_from(value) {
                    Ok(value) => self.visit_u64(value),
                    Err(_) => Err(E::custom(DurationParseError::InvalidNumber(
                        value.to_string(),
                    ))),
                }
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{DurationParseError, HumanDuration};

    fn parse(text: &str) -> Result<Duration, DurationParseError> {
        text.parse::<HumanDuration>().map(HumanDuration::get)
    }

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse("30m"), Ok(Duration::from_mins(30)));
        assert_eq!(parse("2h"), Ok(Duration::from_hours(2)));
        assert_eq!(parse("7d"), Ok(Duration::from_hours(7 * 24)));
        assert_eq!(parse(" 24h "), Ok(Duration::from_hours(24)));
        assert_eq!(parse("0"), Ok(Duration::ZERO));
        assert_eq!(parse("0s"), Ok(Duration::ZERO));
    }

    #[test]
    fn rejects_malformed_input() {
        assert_eq!(parse(""), Err(DurationParseError::Empty));
        assert_eq!(parse("   "), Err(DurationParseError::Empty));
        assert!(matches!(
            parse("m"),
            Err(DurationParseError::InvalidNumber(_))
        ));
        assert!(matches!(
            parse("-5m"),
            Err(DurationParseError::InvalidNumber(_))
        ));
        assert!(matches!(
            parse("5"),
            Err(DurationParseError::InvalidUnit(_))
        ));
        assert!(matches!(
            parse("5 minutes"),
            Err(DurationParseError::InvalidUnit(_))
        ));
        assert!(matches!(
            parse("1.5h"),
            Err(DurationParseError::InvalidUnit(_))
        ));
        assert!(matches!(
            parse("99999999999999999999s"),
            Err(DurationParseError::Overflow(_))
        ));
        assert!(matches!(
            parse("18446744073709551615d"),
            Err(DurationParseError::Overflow(_))
        ));
    }

    #[test]
    fn displays_the_largest_exact_unit() {
        let shown = |millis: u64| HumanDuration::new(Duration::from_millis(millis)).to_string();
        assert_eq!(shown(0), "0s");
        assert_eq!(shown(1), "1ms");
        assert_eq!(shown(1_500), "1500ms");
        assert_eq!(shown(30_000), "30s");
        assert_eq!(shown(90_000), "90s");
        assert_eq!(shown(1_800_000), "30m");
        assert_eq!(shown(7_200_000), "2h");
        assert_eq!(shown(172_800_000), "2d");
    }

    #[test]
    fn display_round_trips() {
        for text in ["1ms", "45s", "30m", "6h", "24h", "7d", "0s"] {
            let parsed: HumanDuration = text.parse().unwrap();
            let again: HumanDuration = parsed.to_string().parse().unwrap();
            assert_eq!(parsed, again, "{text}");
        }
    }

    #[test]
    fn serde_uses_the_string_form() {
        let value: HumanDuration = serde_json::from_str("\"30m\"").unwrap();
        assert_eq!(value, HumanDuration::from_mins(30));
        assert_eq!(serde_json::to_string(&value).unwrap(), "\"30m\"");

        assert_eq!(
            serde_json::from_str::<HumanDuration>("0").unwrap(),
            HumanDuration::default()
        );
        assert!(serde_json::from_str::<HumanDuration>("30").is_err());
        assert!(serde_json::from_str::<HumanDuration>("-1").is_err());
        assert!(serde_json::from_str::<HumanDuration>("true").is_err());
        assert!(serde_json::from_str::<HumanDuration>("\"soon\"").is_err());
    }

    #[test]
    fn constructors_and_conversions_agree() {
        assert_eq!(HumanDuration::from_secs(60), HumanDuration::from_mins(1));
        assert_eq!(HumanDuration::from_hours(1), HumanDuration::from_mins(60));
        let duration: Duration = HumanDuration::from_hours(2).into();
        assert_eq!(duration, Duration::from_hours(2));
        assert_eq!(HumanDuration::from(duration).get(), duration);
    }
}
