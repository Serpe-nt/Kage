//! Conversions between column values and domain values. Each one names the
//! table and column it is for, so a bad stored value is reported precisely.

use std::fmt::Display;
use std::str::FromStr;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::StoreError;

/// Where a value lives: `(table, column)`.
pub(crate) type Place = (&'static str, &'static str);

fn corrupt((table, column): Place, reason: impl Display) -> StoreError {
    StoreError::Corrupt {
        table,
        column,
        reason: reason.to_string(),
    }
}

const fn out_of_range((table, column): Place) -> StoreError {
    StoreError::OutOfRange { table, column }
}

/// Parses a text column into an enum or another `FromStr` type.
pub(crate) fn parse<T>(place: Place, text: &str) -> Result<T, StoreError>
where
    T: FromStr,
    T::Err: Display,
{
    text.parse().map_err(|error| corrupt(place, error))
}

/// Parses a nullable text column.
pub(crate) fn parse_opt<T>(place: Place, text: Option<&str>) -> Result<Option<T>, StoreError>
where
    T: FromStr,
    T::Err: Display,
{
    text.map(|text| parse(place, text)).transpose()
}

/// Decodes a jsonb column.
pub(crate) fn from_json<T: DeserializeOwned>(
    place: Place,
    value: serde_json::Value,
) -> Result<T, StoreError> {
    serde_json::from_value(value).map_err(|error| corrupt(place, error))
}

/// Decodes a nullable jsonb column.
pub(crate) fn from_json_opt<T: DeserializeOwned>(
    place: Place,
    value: Option<serde_json::Value>,
) -> Result<Option<T>, StoreError> {
    value.map(|value| from_json(place, value)).transpose()
}

/// Encodes a value for a jsonb column.
pub(crate) fn to_json<T: Serialize>(
    place: Place,
    value: &T,
) -> Result<serde_json::Value, StoreError> {
    serde_json::to_value(value).map_err(|error| corrupt(place, error))
}

/// Encodes an optional value for a nullable jsonb column.
pub(crate) fn to_json_opt<T: Serialize>(
    place: Place,
    value: Option<&T>,
) -> Result<Option<serde_json::Value>, StoreError> {
    value.map(|value| to_json(place, value)).transpose()
}

/// An unsigned count for an `integer` column.
pub(crate) fn to_i32(place: Place, value: u32) -> Result<i32, StoreError> {
    i32::try_from(value).map_err(|_| out_of_range(place))
}

/// An `integer` column that holds a count.
pub(crate) fn to_u32(place: Place, value: i32) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| corrupt(place, format!("{value} is negative")))
}

/// An `integer` column that holds a port or an HTTP status.
pub(crate) fn to_u16(place: Place, value: i32) -> Result<u16, StoreError> {
    u16::try_from(value).map_err(|_| corrupt(place, format!("{value} does not fit 16 bits")))
}

/// A nullable `integer` column that holds an HTTP status.
pub(crate) fn to_u16_opt(place: Place, value: Option<i32>) -> Result<Option<u16>, StoreError> {
    value.map(|value| to_u16(place, value)).transpose()
}

/// An unsigned size or sequence number for a `bigint` column.
pub(crate) fn to_i64(place: Place, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| out_of_range(place))
}

/// A `bigint` column that holds a size or a sequence number.
pub(crate) fn to_u64(place: Place, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| corrupt(place, format!("{value} is negative")))
}

/// A 64-bit hash as the `bigint` with the same bit pattern.
pub(crate) const fn bits_to_i64(value: u64) -> i64 {
    i64::from_ne_bytes(value.to_ne_bytes())
}

/// The 64-bit hash behind a `bigint` bit pattern.
pub(crate) const fn bits_to_u64(value: i64) -> u64 {
    u64::from_ne_bytes(value.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use kage_core::domain::{ProxyState, Screen};
    use serde_json::json;

    use super::{
        bits_to_i64, bits_to_u64, from_json, from_json_opt, parse, parse_opt, to_i32, to_i64,
        to_json, to_json_opt, to_u16, to_u16_opt, to_u32, to_u64,
    };
    use crate::StoreError;

    const PLACE: (&str, &str) = ("t", "c");

    #[test]
    fn text_columns_parse_or_report_their_place() {
        assert_eq!(
            parse::<ProxyState>(PLACE, "healthy").unwrap(),
            ProxyState::Healthy
        );
        assert_eq!(parse_opt::<ProxyState>(PLACE, None).unwrap(), None);
        assert_eq!(
            parse_opt::<ProxyState>(PLACE, Some("backoff")).unwrap(),
            Some(ProxyState::Backoff)
        );

        let error = parse::<ProxyState>(PLACE, "zombie").unwrap_err();
        assert!(matches!(
            &error,
            StoreError::Corrupt { table: "t", column: "c", reason } if reason.contains("zombie")
        ));
        assert!(parse_opt::<ProxyState>(PLACE, Some("zombie")).is_err());
    }

    #[test]
    fn json_columns_round_trip() {
        let screen = Screen {
            width: 1920,
            height: 1080,
            dpr: 1.0,
            color_depth: 24,
        };

        let json = to_json(PLACE, &screen).unwrap();
        assert_eq!(from_json::<Screen>(PLACE, json.clone()).unwrap(), screen);
        assert_eq!(
            to_json_opt(PLACE, Some(&screen)).unwrap(),
            Some(json.clone())
        );
        assert_eq!(to_json_opt::<Screen>(PLACE, None).unwrap(), None);
        assert_eq!(
            from_json_opt::<Screen>(PLACE, Some(json)).unwrap(),
            Some(screen)
        );
        assert_eq!(from_json_opt::<Screen>(PLACE, None).unwrap(), None);

        assert!(matches!(
            from_json::<Screen>(PLACE, json!({ "width": "wide" })),
            Err(StoreError::Corrupt { .. })
        ));
    }

    #[test]
    fn integers_are_checked_in_both_directions() {
        assert_eq!(to_i32(PLACE, 300).unwrap(), 300);
        assert!(matches!(
            to_i32(PLACE, u32::MAX),
            Err(StoreError::OutOfRange { .. })
        ));
        assert_eq!(to_u32(PLACE, 300).unwrap(), 300);
        assert!(matches!(to_u32(PLACE, -1), Err(StoreError::Corrupt { .. })));

        assert_eq!(to_u16(PLACE, 8080).unwrap(), 8080);
        assert!(to_u16(PLACE, 70_000).is_err());
        assert!(to_u16(PLACE, -1).is_err());
        assert_eq!(to_u16_opt(PLACE, None).unwrap(), None);
        assert_eq!(to_u16_opt(PLACE, Some(404)).unwrap(), Some(404));

        assert_eq!(to_i64(PLACE, 5_000_000_000).unwrap(), 5_000_000_000);
        assert!(matches!(
            to_i64(PLACE, u64::MAX),
            Err(StoreError::OutOfRange { .. })
        ));
        assert_eq!(to_u64(PLACE, 5_000_000_000).unwrap(), 5_000_000_000);
        assert!(to_u64(PLACE, -1).is_err());
    }

    #[test]
    fn hashes_keep_all_64_bits() {
        for value in [0, 1, u64::MAX, 1 << 63, 0xDEAD_BEEF_CAFE_F00D] {
            assert_eq!(bits_to_u64(bits_to_i64(value)), value);
        }
        assert_eq!(bits_to_i64(u64::MAX), -1);
    }
}
