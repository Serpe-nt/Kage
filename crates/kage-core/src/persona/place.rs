//! Where a persona is: its time zone, its local time, and the position it
//! would report (spec §6.1 step 1).
//!
//! All of it derives from the observed geo record of the proxy's exit
//! address, which is what a target's own geo database will most likely
//! agree with; the provider's country is the fallback only while no lookup
//! has succeeded.

use std::str::FromStr;

use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::domain::{CountryCode, Geolocation};
use crate::seed::Seed;

/// Where a persona's time zone came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneSource {
    /// The geo lookup named it.
    Observed,
    /// The lookup named none, or one that is no IANA zone: the main zone
    /// of the country.
    Country,
    /// The country is not known either: UTC, which the caller reports.
    Unknown,
}

/// The main time zone of a country, for an address whose lookup named no
/// zone. For a country with several zones it is the one most people live
/// in.
fn zone_of_country(country: CountryCode) -> Option<&'static str> {
    Some(match country.as_str() {
        "AT" => "Europe/Vienna",
        "BE" => "Europe/Brussels",
        "CA" => "America/Toronto",
        "CH" => "Europe/Zurich",
        "DE" => "Europe/Berlin",
        "ES" => "Europe/Madrid",
        "FR" => "Europe/Paris",
        "GB" => "Europe/London",
        "IE" => "Europe/Dublin",
        "IT" => "Europe/Rome",
        "LU" => "Europe/Luxembourg",
        "NL" => "Europe/Amsterdam",
        "PL" => "Europe/Warsaw",
        "PT" => "Europe/Lisbon",
        "RO" => "Europe/Bucharest",
        "SE" => "Europe/Stockholm",
        "US" => "America/New_York",
        _ => return None,
    })
}

/// Whether a name is an IANA time zone.
pub fn is_zone(name: &str) -> bool {
    Tz::from_str(name).is_ok()
}

/// The time zone of a persona: the one the geo lookup observed when it is
/// an IANA zone, else the main zone of the country, else UTC.
pub fn timezone(observed: Option<&str>, country: Option<CountryCode>) -> (String, ZoneSource) {
    if let Some(name) = observed.map(str::trim).filter(|name| is_zone(name)) {
        return (name.to_owned(), ZoneSource::Observed);
    }
    match country.and_then(zone_of_country) {
        Some(zone) => (zone.to_owned(), ZoneSource::Country),
        None => ("UTC".to_owned(), ZoneSource::Unknown),
    }
}

fn zone(name: &str) -> Tz {
    Tz::from_str(name).unwrap_or(Tz::UTC)
}

/// The wall-clock time of an instant in a time zone. An unknown zone name
/// reads as UTC.
pub fn local_time(now: DateTime<Utc>, timezone: &str) -> NaiveDateTime {
    now.with_timezone(&zone(timezone)).naive_local()
}

/// The calendar day of an instant in a time zone: the day a persona's
/// daily counters belong to (spec §5).
pub fn local_day(now: DateTime<Utc>, timezone: &str) -> NaiveDate {
    local_time(now, timezone).date()
}

/// The instant a wall-clock time stands for in a time zone. A time that
/// exists twice (clocks set back) is the earlier one; a time that does not
/// exist (clocks set forward) is the first instant after the gap.
pub fn instant_of(local: NaiveDateTime, timezone: &str) -> DateTime<Utc> {
    let zone = zone(timezone);
    let mut candidate = local;
    // A gap is an hour at most almost everywhere; never more than a day.
    for _ in 0..(24 * 4) {
        match zone.from_local_datetime(&candidate) {
            LocalResult::Single(instant) | LocalResult::Ambiguous(instant, _) => {
                return instant.with_timezone(&Utc);
            }
            LocalResult::None => candidate += Duration::minutes(15),
        }
    }
    Utc.from_utc_datetime(&local)
}

/// The next local midnight after `now`: when a persona that reached a daily
/// cap is eligible again (spec §6.3).
pub fn next_local_midnight(now: DateTime<Utc>, timezone: &str) -> DateTime<Utc> {
    let today = local_day(now, timezone);
    let tomorrow = today.succ_opt().unwrap_or(today);
    instant_of(tomorrow.and_time(NaiveTime::MIN), timezone)
}

/// The hours of the day in which a household is awake, in its own time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveHours {
    /// The first minute of the day something may happen.
    pub from: NaiveTime,
    /// The first minute after which nothing starts any more. Not before
    /// `from`.
    pub until: NaiveTime,
}

impl ActiveHours {
    /// Whether an instant falls into the active hours of a time zone.
    pub fn contains(&self, at: DateTime<Utc>, timezone: &str) -> bool {
        let time = local_time(at, timezone).time();
        time >= self.from && time < self.until
    }

    /// `at` when it falls into the active hours, else the start of the
    /// next active stretch plus `late`, so that not every persona begins
    /// its day on the minute.
    pub fn at_or_after(&self, at: DateTime<Utc>, timezone: &str, late: Duration) -> DateTime<Utc> {
        if self.contains(at, timezone) {
            return at;
        }
        let local = local_time(at, timezone);
        let day = if local.time() < self.from {
            local.date()
        } else {
            local.date().succ_opt().unwrap_or(local.date())
        };
        instant_of(day.and_time(self.from), timezone) + late
    }
}

/// Kilometres per degree of latitude.
const KM_PER_DEGREE: f64 = 111.32;

/// How far from the lookup's coordinates a persona lives, in kilometres.
const HOME_DISTANCE_KM: (f64, f64) = (0.5, 4.0);

/// The accuracy a browser reports for a position found from Wi-Fi networks,
/// in metres.
const ACCURACY_M: (f64, f64) = (20.0, 90.0);

/// The position a persona would report once a site has permission: a point
/// between half a kilometre and four kilometres from the coordinates the
/// geo lookup returned (the centre of the city), drawn from the persona's
/// seed, with a reported accuracy as a laptop's is. `None` when the lookup
/// returned no coordinates: a persona reports no position rather than an
/// invented one. The permission itself is never granted automatically.
pub fn position(coordinates: Option<(f64, f64)>, seed: &Seed) -> Option<Geolocation> {
    let (latitude, longitude) = coordinates?;
    let mut rng = seed.child("position").rng();
    let (near, far) = HOME_DISTANCE_KM;
    // Uniform over the area of the ring, not over its radius.
    let distance = rng.uniform(near * near, far * far).sqrt();
    let bearing = rng.uniform(0.0, std::f64::consts::TAU);
    let north = distance * bearing.cos() / KM_PER_DEGREE;
    // A degree of longitude shrinks with the cosine of the latitude; near
    // a pole the offset is left out rather than blown up.
    let shrink = latitude.to_radians().cos();
    let east = if shrink > 0.01 {
        distance * bearing.sin() / (KM_PER_DEGREE * shrink)
    } else {
        0.0
    };
    let round = |value: f64, digits: i32| {
        let scale = 10_f64.powi(digits);
        (value * scale).round() / scale
    };
    Some(Geolocation {
        latitude: round((latitude + north).clamp(-90.0, 90.0), 6),
        longitude: round((longitude + east).clamp(-180.0, 180.0), 6),
        accuracy_m: round(rng.uniform(ACCURACY_M.0, ACCURACY_M.1), 0),
    })
}

/// The distance between two points in kilometres (haversine).
pub fn distance_km(from: (f64, f64), to: (f64, f64)) -> f64 {
    const EARTH_RADIUS_KM: f64 = 6371.0;
    let (lat1, lon1) = (from.0.to_radians(), from.1.to_radians());
    let (lat2, lon2) = (to.0.to_radians(), to.1.to_radians());
    let a = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((lon2 - lon1) / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDate, NaiveTime, TimeZone, Utc};

    use super::{
        ActiveHours, ZoneSource, distance_km, instant_of, is_zone, local_day, local_time,
        next_local_midnight, position, timezone,
    };
    use crate::domain::CountryCode;
    use crate::seed::Seed;

    fn country(code: &str) -> CountryCode {
        code.parse().unwrap()
    }

    #[test]
    fn the_observed_zone_wins_and_the_country_is_the_fallback() {
        assert_eq!(
            timezone(Some("Europe/Vienna"), Some(country("DE"))),
            ("Europe/Vienna".to_owned(), ZoneSource::Observed)
        );
        assert_eq!(
            timezone(Some(" Europe/Rome "), None),
            ("Europe/Rome".to_owned(), ZoneSource::Observed)
        );
        // No zone, or something that is none: the country's main zone.
        for observed in [None, Some(""), Some("Mitteleuropa"), Some("+02:00")] {
            assert_eq!(
                timezone(observed, Some(country("AT"))),
                ("Europe/Vienna".to_owned(), ZoneSource::Country),
                "{observed:?}"
            );
        }
        for (code, zone) in [
            ("DE", "Europe/Berlin"),
            ("FR", "Europe/Paris"),
            ("GB", "Europe/London"),
            ("IT", "Europe/Rome"),
            ("US", "America/New_York"),
        ] {
            assert_eq!(timezone(None, Some(country(code))).0, zone);
            assert!(is_zone(zone));
        }
        assert_eq!(
            timezone(None, Some(country("JP"))),
            ("UTC".to_owned(), ZoneSource::Unknown)
        );
        assert_eq!(timezone(None, None).1, ZoneSource::Unknown);
        assert!(!is_zone("Europe/Atlantis"));
    }

    #[test]
    fn the_local_day_turns_at_local_midnight() {
        // 22:30 UTC on the 8th is 00:30 on the 9th in Vienna (summer time).
        let late = Utc.with_ymd_and_hms(2026, 10, 8, 22, 30, 0).unwrap();
        let ninth = NaiveDate::from_ymd_opt(2026, 10, 9).unwrap();
        assert_eq!(local_day(late, "Europe/Vienna"), ninth);
        assert_eq!(local_day(late, "Europe/London"), ninth.pred_opt().unwrap());
        assert_eq!(
            local_time(late, "Europe/Vienna").time(),
            NaiveTime::from_hms_opt(0, 30, 0).unwrap()
        );
        // An unknown zone reads as UTC.
        assert_eq!(
            local_day(late, "Nowhere/Nothing"),
            ninth.pred_opt().unwrap()
        );

        // The next midnight in Vienna is 22:00 UTC that evening.
        let noon = Utc.with_ymd_and_hms(2026, 10, 9, 10, 0, 0).unwrap();
        assert_eq!(
            next_local_midnight(noon, "Europe/Vienna"),
            Utc.with_ymd_and_hms(2026, 10, 9, 22, 0, 0).unwrap()
        );
        // After the clocks went back it is 23:00 UTC.
        let winter = Utc.with_ymd_and_hms(2026, 11, 2, 10, 0, 0).unwrap();
        assert_eq!(
            next_local_midnight(winter, "Europe/Vienna"),
            Utc.with_ymd_and_hms(2026, 11, 2, 23, 0, 0).unwrap()
        );
    }

    #[test]
    fn wall_clock_times_that_exist_twice_or_not_at_all_have_an_instant() {
        // 25 October 2026, Europe: 02:30 exists twice. The earlier one.
        let twice = NaiveDate::from_ymd_opt(2026, 10, 25)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        assert_eq!(
            instant_of(twice, "Europe/Berlin"),
            Utc.with_ymd_and_hms(2026, 10, 25, 0, 30, 0).unwrap()
        );
        // 29 March 2026: 02:30 does not exist. The first instant after.
        let never = NaiveDate::from_ymd_opt(2026, 3, 29)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        assert_eq!(
            instant_of(never, "Europe/Berlin"),
            Utc.with_ymd_and_hms(2026, 3, 29, 1, 0, 0).unwrap()
        );
    }

    #[test]
    fn nothing_starts_at_night_in_the_personas_own_time() {
        let hours = ActiveHours {
            from: NaiveTime::from_hms_opt(7, 0, 0).unwrap(),
            until: NaiveTime::from_hms_opt(23, 30, 0).unwrap(),
        };
        let late = Duration::minutes(12);
        // 14:00 in Berlin: now.
        let afternoon = Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap();
        assert!(hours.contains(afternoon, "Europe/Berlin"));
        assert_eq!(
            hours.at_or_after(afternoon, "Europe/Berlin", late),
            afternoon
        );
        // 23:45 in Berlin: tomorrow at 07:12.
        let night = Utc.with_ymd_and_hms(2026, 10, 9, 21, 45, 0).unwrap();
        assert!(!hours.contains(night, "Europe/Berlin"));
        assert_eq!(
            hours.at_or_after(night, "Europe/Berlin", late),
            Utc.with_ymd_and_hms(2026, 10, 10, 5, 12, 0).unwrap()
        );
        // 04:00 in Berlin: today at 07:12.
        let early = Utc.with_ymd_and_hms(2026, 10, 10, 2, 0, 0).unwrap();
        assert_eq!(
            hours.at_or_after(early, "Europe/Berlin", late),
            Utc.with_ymd_and_hms(2026, 10, 10, 5, 12, 0).unwrap()
        );
        // The same instant is a different hour in London.
        assert!(hours.contains(night, "Europe/London"));
    }

    #[test]
    fn a_position_is_a_few_kilometres_from_the_citys_centre() {
        let vienna = (48.2085, 16.3721);
        let mut distances = Vec::new();
        for index in 0..500 {
            let point = position(Some(vienna), &Seed::from_u64(index)).unwrap();
            let distance = distance_km(vienna, (point.latitude, point.longitude));
            assert!((0.49..=4.01).contains(&distance), "{distance}");
            assert!((20.0..=90.0).contains(&point.accuracy_m));
            distances.push(distance);
        }
        // Spread over the ring, not all at one distance or in one place.
        assert!(distances.iter().any(|distance| *distance < 1.5));
        assert!(distances.iter().any(|distance| *distance > 3.5));
        let first = position(Some(vienna), &Seed::from_u64(1)).unwrap();
        let second = position(Some(vienna), &Seed::from_u64(2)).unwrap();
        assert_ne!(first, second);
        // One persona, one home.
        assert_eq!(position(Some(vienna), &Seed::from_u64(1)), Some(first));
        // No coordinates, no position.
        assert_eq!(position(None, &Seed::from_u64(1)), None);
        // Near a pole nothing blows up.
        let north = position(Some((89.999, 10.0)), &Seed::from_u64(3)).unwrap();
        assert!((-180.0..=180.0).contains(&north.longitude));
        assert!(north.latitude <= 90.0);
    }
}
