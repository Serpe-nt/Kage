//! When a persona is awake (spec §11).
//!
//! A household is not at its computer at four in the morning. Every
//! persona has a day of its own: it gets up and goes to bed at times drawn
//! once from its seed, inside the configured ranges, and both move a little
//! from one day to the next, so that twenty personas neither start their
//! day on the same minute nor one persona on the same minute every day.
//!
//! Times are wall-clock times of the persona's own time zone. A day belongs
//! to the calendar date it starts on and may end after midnight.

use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};

use crate::persona::place;
use crate::seed::Seed;

const DAY_SECONDS: i64 = 24 * 60 * 60;

/// The ranges a persona's day is drawn from, in seconds after midnight of
/// the date the day starts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayRules {
    up: (i64, i64),
    to_bed: (i64, i64),
    drift: i64,
}

fn seconds(time: NaiveTime) -> i64 {
    i64::from(time.num_seconds_from_midnight())
}

impl DayRules {
    /// Rules from the two ranges and the most both ends move from day to
    /// day. A bedtime at or before the latest time of getting up is on the
    /// next day, and so is the end of a range that lies before its start.
    pub fn new(
        up: (NaiveTime, NaiveTime),
        to_bed: (NaiveTime, NaiveTime),
        drift: std::time::Duration,
    ) -> Self {
        let up_from = seconds(up.0);
        let up_until = seconds(up.1).max(up_from);
        let mut bed_from = seconds(to_bed.0);
        if bed_from <= up_until {
            bed_from += DAY_SECONDS;
        }
        let mut bed_until = seconds(to_bed.1);
        while bed_until < bed_from {
            bed_until += DAY_SECONDS;
        }
        Self {
            up: (up_from, up_until),
            to_bed: (bed_from, bed_until),
            drift: i64::try_from(drift.as_secs()).unwrap_or(i64::MAX / 4),
        }
    }

    /// The shortest a persona can be awake on one day.
    pub fn shortest_day(&self) -> Duration {
        Duration::seconds((self.to_bed.0 - self.drift) - (self.up.1 + self.drift))
    }

    /// The longest a persona can be awake on one day.
    pub fn longest_day(&self) -> Duration {
        Duration::seconds((self.to_bed.1 + self.drift) - (self.up.0 - self.drift))
    }
}

/// One day of one persona, in the wall-clock time of its time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersonaDay {
    /// When it gets up.
    pub up: NaiveDateTime,
    /// When it goes to bed; after midnight for most.
    pub to_bed: NaiveDateTime,
}

impl PersonaDay {
    /// Whether the persona is awake at a wall-clock time.
    pub fn contains(&self, local: NaiveDateTime) -> bool {
        self.up <= local && local < self.to_bed
    }
}

/// A point of a range, drawn from a seed.
fn within(range: (i64, i64), seed: &Seed) -> i64 {
    let (from, until) = range;
    // The ranges are seconds of a day or two: far inside what f64 holds
    // exactly, and the product is rounded down to a whole second.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let offset = ((until - from) as f64 * seed.rng().unit()) as i64;
    from + offset
}

/// The day of a persona that starts on `date`: its own times of getting up
/// and going to bed, each moved by up to the drift for this date.
pub fn day_of(seed: &Seed, date: NaiveDate, rules: &DayRules) -> PersonaDay {
    let up = within(rules.up, &seed.child("day-up"));
    let to_bed = within(rules.to_bed, &seed.child("day-to-bed"));
    let today = Seed::derive("day-drift", &[seed.as_bytes(), date.to_string().as_bytes()]);
    let drift = |label: &str| within((-rules.drift, rules.drift), &today.child(label));
    let midnight = date.and_time(NaiveTime::MIN);
    PersonaDay {
        up: midnight + Duration::seconds(up + drift("up")),
        to_bed: midnight + Duration::seconds(to_bed + drift("to-bed")),
    }
}

/// Whether a persona is awake at an instant. Yesterday's day counts too:
/// it may run past midnight.
pub fn awake(seed: &Seed, now: DateTime<Utc>, timezone: &str, rules: &DayRules) -> bool {
    DayBook::new(*rules).awake(seed, now, timezone)
}

/// When a persona is next awake: `now` when it is, else the moment it gets
/// up.
pub fn next_up(seed: &Seed, now: DateTime<Utc>, timezone: &str, rules: &DayRules) -> DateTime<Utc> {
    DayBook::new(*rules).next_up(seed, now, timezone)
}

/// Days kept beyond which a book starts over.
const BOOK_LIMIT: usize = 4096;

/// The days of personas, remembered: a day is drawn once and looked up
/// from then on. Whoever asks about the same personas many times a second
/// keeps one.
#[derive(Debug, Clone)]
pub struct DayBook {
    rules: DayRules,
    days: HashMap<(Seed, NaiveDate), PersonaDay>,
}

impl DayBook {
    /// An empty book for these rules.
    pub fn new(rules: DayRules) -> Self {
        Self {
            rules,
            days: HashMap::new(),
        }
    }

    /// The rules the days are drawn by.
    pub const fn rules(&self) -> &DayRules {
        &self.rules
    }

    fn day(&mut self, seed: &Seed, date: NaiveDate) -> PersonaDay {
        if self.days.len() >= BOOK_LIMIT {
            self.days.clear();
        }
        let rules = self.rules;
        *self
            .days
            .entry((*seed, date))
            .or_insert_with(|| day_of(seed, date, &rules))
    }

    /// Whether a persona is awake at an instant. Yesterday's day counts
    /// too: it may run past midnight.
    pub fn awake(&mut self, seed: &Seed, now: DateTime<Utc>, timezone: &str) -> bool {
        let local = place::local_time(now, timezone);
        let today = local.date();
        today
            .pred_opt()
            .into_iter()
            .chain([today])
            .any(|date| self.day(seed, date).contains(local))
    }

    /// When a persona is next awake: `now` when it is, else the moment it
    /// gets up.
    pub fn next_up(&mut self, seed: &Seed, now: DateTime<Utc>, timezone: &str) -> DateTime<Utc> {
        if self.awake(seed, now, timezone) {
            return now;
        }
        let local = place::local_time(now, timezone);
        let today = local.date();
        let up = [Some(today), today.succ_opt()]
            .into_iter()
            .flatten()
            .map(|date| self.day(seed, date).up)
            .find(|up| *up > local);
        // A drift of more than a day does not validate; without one, one
        // of the two dates has its morning ahead.
        up.map_or(now, |up| place::instant_of(up, timezone))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::{Duration, NaiveDate, NaiveTime, TimeZone, Utc};

    use super::{BOOK_LIMIT, DayBook, DayRules, awake, day_of, next_up};
    use crate::seed::Seed;

    fn time(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    fn rules() -> DayRules {
        DayRules::new(
            (time(6, 30), time(8, 30)),
            (time(22, 30), time(0, 30)),
            std::time::Duration::from_mins(20),
        )
    }

    fn date(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap()
    }

    #[test]
    fn a_bedtime_after_midnight_belongs_to_the_day_before() {
        let rules = rules();

        // Up at the latest 08:30 plus 20 minutes, to bed at the earliest
        // 22:30 minus 20: 13 h 20 min. From 06:10 to 00:50: 18 h 40 min.
        assert_eq!(rules.shortest_day(), Duration::minutes(13 * 60 + 20));
        assert_eq!(rules.longest_day(), Duration::minutes(18 * 60 + 40));

        // A bedtime at or before the time of getting up is the next day's,
        // and a range may end where it began.
        let night_owl = DayRules::new(
            (time(10, 0), time(10, 0)),
            (time(3, 0), time(2, 0)),
            std::time::Duration::ZERO,
        );
        assert_eq!(night_owl.shortest_day(), Duration::hours(17));
        assert_eq!(night_owl.longest_day(), Duration::hours(40));
        // An end of the first range before its start is the start.
        let fixed = DayRules::new(
            (time(9, 0), time(8, 0)),
            (time(21, 0), time(21, 0)),
            std::time::Duration::ZERO,
        );
        assert_eq!(fixed.shortest_day(), Duration::hours(12));
        assert_eq!(fixed.longest_day(), Duration::hours(12));
        // A bedtime exactly at the latest getting up is a day later.
        let around = DayRules::new(
            (time(8, 0), time(8, 0)),
            (time(8, 0), time(8, 0)),
            std::time::Duration::ZERO,
        );
        assert_eq!(around.shortest_day(), Duration::hours(24));
    }

    #[test]
    fn every_persona_has_its_own_day_inside_the_ranges() {
        let rules = rules();
        let mut ups = BTreeSet::new();
        let mut beds = BTreeSet::new();
        for persona in 0..200_u64 {
            let seed = Seed::from_u64(persona);
            let day = day_of(&seed, date(12), &rules);
            let midnight = date(12).and_time(NaiveTime::MIN);

            let up = day.up - midnight;
            let to_bed = day.to_bed - midnight;
            assert!(
                up >= Duration::minutes(6 * 60 + 10) && up < Duration::minutes(8 * 60 + 50),
                "{up}"
            );
            assert!(
                to_bed >= Duration::minutes(22 * 60 + 10)
                    && to_bed < Duration::minutes(24 * 60 + 50),
                "{to_bed}"
            );
            assert!(day.contains(day.up));
            assert!(!day.contains(day.to_bed));
            assert!(!day.contains(day.up - Duration::seconds(1)));
            assert!(day.contains(day.to_bed - Duration::seconds(1)));
            ups.insert(up.num_minutes());
            beds.insert(to_bed.num_minutes());
            // The same question has the same answer.
            assert_eq!(day, day_of(&seed, date(12), &rules));
        }
        // Spread over the two hours, not gathered on a few minutes.
        assert!(ups.len() > 100, "{}", ups.len());
        assert!(beds.len() > 100, "{}", beds.len());
        assert!(*ups.first().unwrap() < 7 * 60 && *ups.last().unwrap() > 8 * 60);
        assert!(*beds.last().unwrap() > 24 * 60);
    }

    #[test]
    fn a_day_drifts_a_little_from_one_date_to_the_next() {
        let rules = rules();
        let seed = Seed::from_u64(7);
        let midnight = |day: u32| date(day).and_time(NaiveTime::MIN);

        let ups: Vec<i64> = (1..=28)
            .map(|day| (day_of(&seed, date(day), &rules).up - midnight(day)).num_seconds())
            .collect();

        let (least, most) = (ups.iter().min().unwrap(), ups.iter().max().unwrap());
        // Never the same minute twice in a row, never further than the
        // drift from the persona's own time, which is twice the drift apart
        // at the most.
        assert!(ups.windows(2).all(|pair| pair[0] != pair[1]));
        assert!(most - least <= 40 * 60, "{}", most - least);
        assert!(most - least > 20 * 60, "{}", most - least);

        // Without a drift every day is the same.
        let fixed = DayRules::new(
            (time(6, 30), time(8, 30)),
            (time(22, 30), time(0, 30)),
            std::time::Duration::ZERO,
        );
        let first = day_of(&seed, date(1), &fixed).up - midnight(1);
        assert!((2..=28).all(|day| day_of(&seed, date(day), &fixed).up - midnight(day) == first));
    }

    #[test]
    fn awake_follows_the_personas_own_clock() {
        let rules = rules();
        let seed = Seed::from_u64(11);
        // Noon in Berlin is awake for everyone, 04:00 for no one.
        let noon = Utc.with_ymd_and_hms(2026, 10, 12, 10, 0, 0).unwrap();
        let night = Utc.with_ymd_and_hms(2026, 10, 12, 2, 0, 0).unwrap();
        assert!(awake(&seed, noon, "Europe/Berlin", &rules));
        assert!(!awake(&seed, night, "Europe/Berlin", &rules));
        // The same instants are the other way round in Tokyo and Honolulu:
        // 19:00 and 11:00 in Tokyo, 00:00 and 16:00 the day before in
        // Honolulu.
        assert!(awake(&seed, night, "Asia/Tokyo", &rules));
        assert!(awake(&seed, night, "Pacific/Honolulu", &rules));
        assert!(awake(&seed, noon, "Asia/Tokyo", &rules));

        // Just after midnight a persona whose day runs past it is still
        // up: that is yesterday's day.
        let late = (0..200_u64).map(Seed::from_u64).find(|seed| {
            let day = day_of(seed, date(12), &rules);
            day.to_bed > date(13).and_time(time(0, 20))
        });
        let late = late.unwrap();
        // 00:10 on the 13th in Berlin (summer time until the 25th).
        let after_midnight = Utc.with_ymd_and_hms(2026, 10, 12, 22, 10, 0).unwrap();
        assert!(awake(&late, after_midnight, "Europe/Berlin", &rules));
        // An early sleeper is in bed by then.
        let early = (0..200_u64)
            .map(Seed::from_u64)
            .find(|seed| day_of(seed, date(12), &rules).to_bed < date(12).and_time(time(23, 0)))
            .unwrap();
        assert!(!awake(&early, after_midnight, "Europe/Berlin", &rules));
    }

    #[test]
    fn a_book_answers_as_the_functions_do_and_starts_over_when_it_is_full() {
        let rules = rules();
        let mut book = DayBook::new(rules);
        assert_eq!(book.rules(), &rules);
        let zone = "Europe/Berlin";
        let start = Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap();
        for persona in 0..40_u64 {
            let seed = Seed::from_u64(persona);
            for quarter in 0..(4 * 48) {
                let at = start + Duration::minutes(quarter * 15);
                assert_eq!(book.awake(&seed, at, zone), awake(&seed, at, zone, &rules));
                assert_eq!(
                    book.next_up(&seed, at, zone),
                    next_up(&seed, at, zone, &rules)
                );
            }
        }
        // Forty personas over two days and the day before: far below the
        // limit, every day drawn once.
        assert_eq!(book.days.len(), 40 * 4);

        // A full book starts over and still answers the same.
        let mut book = DayBook::new(rules);
        let seed = Seed::from_u64(1);
        let noon = start + Duration::hours(10);
        for day in 0..BOOK_LIMIT {
            let date = date(1) + Duration::days(i64::try_from(day).unwrap());
            book.day(&seed, date);
        }
        assert_eq!(book.days.len(), BOOK_LIMIT);
        assert!(book.awake(&seed, noon, zone));
        // Yesterday (not it) and today (it): the two days it looked at.
        assert_eq!(book.days.len(), 2);
    }

    #[test]
    fn the_next_morning_is_today_before_dawn_and_tomorrow_after_bedtime() {
        let rules = rules();
        let seed = Seed::from_u64(3);
        let zone = "Europe/Vienna";
        let noon = Utc.with_ymd_and_hms(2026, 10, 12, 10, 0, 0).unwrap();
        assert_eq!(next_up(&seed, noon, zone, &rules), noon);

        // 04:00 local on the 12th: up this morning.
        let before_dawn = Utc.with_ymd_and_hms(2026, 10, 12, 2, 0, 0).unwrap();
        let morning = next_up(&seed, before_dawn, zone, &rules);
        let local = crate::persona::place::local_time(morning, zone);
        assert_eq!(local, day_of(&seed, date(12), &rules).up);
        assert!(morning > before_dawn && morning - before_dawn < Duration::hours(5));
        assert!(awake(&seed, morning, zone, &rules));
        assert!(!awake(&seed, morning - Duration::seconds(1), zone, &rules));

        // An early sleeper at 23:30 local on the 12th: up on the 13th.
        let early = (0..200_u64)
            .map(Seed::from_u64)
            .find(|seed| day_of(seed, date(12), &rules).to_bed < date(12).and_time(time(23, 0)))
            .unwrap();
        let late_evening = Utc.with_ymd_and_hms(2026, 10, 12, 21, 30, 0).unwrap();
        assert!(!awake(&early, late_evening, zone, &rules));
        let tomorrow = next_up(&early, late_evening, zone, &rules);
        assert_eq!(
            crate::persona::place::local_time(tomorrow, zone),
            day_of(&early, date(13), &rules).up
        );
    }
}
