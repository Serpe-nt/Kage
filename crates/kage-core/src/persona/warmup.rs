//! The warm-up of a new persona (spec §6.1 step 5): which sites it visits
//! in its first hours, and when.
//!
//! A profile that is empty on first contact with a target is a tell, so a
//! new persona reads a few popular neutral pages first, through its own
//! browser and with human behavior. This module makes the plan; the
//! persona manager carries it out.
//!
//! The plan is a function of the persona's seed, the moment it was
//! generated, its time zone, country and locale, and the configured list:
//!
//! - the number of visits is drawn from `personas.warmup.visits`;
//! - each visit's site is drawn from the entries for everyone and the
//!   entries of the persona's own country, the latter three times as
//!   likely, never the same site twice in a row. Every installation ships
//!   the same default list; no two personas walk it the same way;
//! - the first visit is a few minutes after generation, the following ones
//!   a log-normal gap apart;
//! - nothing happens at night: a visit that would fall outside the active
//!   hours of the persona's own time zone waits for the next morning, a
//!   few minutes into it.
//!
//! The plan is not stored. It is computed again from the same inputs
//! whenever the manager looks, and the persona's count of handled visits
//! says how far it has come.

use chrono::{DateTime, Duration, Utc};

use crate::config::{WarmupConfig, WarmupSite, warmup_url};
use crate::domain::CountryCode;
use crate::pacing::Lognormal;
use crate::persona::locale::language_of;
use crate::persona::place::ActiveHours;
use crate::seed::{Seed, SeededRng};

/// How much likelier a site of the persona's own country is than a site
/// for everyone.
const OWN_COUNTRY_WEIGHT: f64 = 3.0;

/// No two warm-up visits are closer than this.
const SHORTEST_GAP_MS: f64 = 120_000.0;

/// How far into the morning a visit that waited for it happens, at most.
const LATEST_MORNING_START_MIN: f64 = 45.0;

/// One visit of a persona's warm-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedVisit {
    /// Its place in the plan, from zero.
    pub number: u32,
    /// The page to load.
    pub url: url::Url,
    /// When it is due.
    pub due_at: DateTime<Utc>,
}

impl PlannedVisit {
    /// The host the visit goes to: the domain its outcome is recorded
    /// under.
    pub fn domain(&self) -> &str {
        self.url.host_str().unwrap_or_default()
    }
}

/// Who the plan is for.
#[derive(Debug, Clone, Copy)]
pub struct WarmupSubject<'a> {
    /// The persona's seed.
    pub seed: &'a Seed,
    /// When the persona was generated.
    pub created_at: DateTime<Utc>,
    /// Its IANA time zone.
    pub timezone: &'a str,
    /// The country of its address.
    pub country: Option<CountryCode>,
    /// Its locale, for the language of localized sites.
    pub locale: &'a str,
}

fn milliseconds(duration: std::time::Duration) -> f64 {
    // Durations in configuration are far below 2^53 milliseconds.
    #[allow(clippy::cast_precision_loss)]
    let value = duration.as_millis() as f64;
    value
}

fn span(milliseconds: f64) -> Duration {
    // Rounded and bounded: a gap is minutes or hours.
    #[allow(clippy::cast_possible_truncation)]
    let whole = milliseconds.round().clamp(0.0, 1e12) as i64;
    Duration::milliseconds(whole)
}

/// The sites a persona may visit, each with its weight.
fn candidates<'a>(
    sites: &'a [WarmupSite],
    country: Option<CountryCode>,
    language: &str,
) -> Vec<(url::Url, f64, &'a WarmupSite)> {
    sites
        .iter()
        .filter_map(|site| {
            let weight = if site.countries.is_empty() {
                1.0
            } else if country.is_some_and(|country| site.countries.contains(&country)) {
                OWN_COUNTRY_WEIGHT
            } else {
                return None;
            };
            Some((warmup_url(&site.url, language)?, weight, site))
        })
        .collect()
}

/// Draws one site by weight, leaving out the one visited last when there is
/// another.
fn draw_site(weights: &[f64], last: Option<usize>, rng: &mut SeededRng) -> usize {
    let usable = |index: usize| weights.len() == 1 || Some(index) != last;
    let total: f64 = weights
        .iter()
        .enumerate()
        .filter(|(index, _)| usable(*index))
        .map(|(_, weight)| weight)
        .sum();
    let mut threshold = rng.unit() * total;
    let mut chosen = 0;
    for (index, weight) in weights.iter().enumerate() {
        if !usable(index) {
            continue;
        }
        chosen = index;
        threshold -= weight;
        if threshold < 0.0 {
            break;
        }
    }
    chosen
}

/// The warm-up plan of a persona. Empty when warm-up is switched off or no
/// configured site is for this persona.
pub fn plan(subject: &WarmupSubject<'_>, config: &WarmupConfig) -> Vec<PlannedVisit> {
    if !config.enabled {
        return Vec::new();
    }
    let sites = candidates(&config.sites, subject.country, language_of(subject.locale));
    if sites.is_empty() {
        return Vec::new();
    }
    let weights: Vec<f64> = sites.iter().map(|(_, weight, _)| *weight).collect();
    let hours: ActiveHours = config.active_hours.hours();
    let median = milliseconds(config.gap.median.get());
    let p95 = milliseconds(config.gap.p95.get());
    let floor = SHORTEST_GAP_MS.min(median / 2.0);
    let gap = Lognormal {
        median,
        p95: p95.max(median),
        min: Some(floor),
        max: None,
    };

    let mut rng = subject.seed.child("warmup").rng();
    let count = rng.int_inclusive(u64::from(config.visits.min), u64::from(config.visits.max));
    let first = rng.uniform(
        milliseconds(config.first_visit_after.min.get()),
        milliseconds(config.first_visit_after.max.get()),
    );
    let mut due = subject.created_at + span(first);
    let mut last = None;
    let mut visits = Vec::new();
    for number in 0..count {
        let late = span(rng.uniform(0.0, LATEST_MORNING_START_MIN) * 60_000.0);
        due = hours.at_or_after(due, subject.timezone, late);
        let site = draw_site(&weights, last, &mut rng);
        last = Some(site);
        visits.push(PlannedVisit {
            number: u32::try_from(number).unwrap_or(u32::MAX),
            url: sites[site].0.clone(),
            due_at: due,
        });
        due += span(gap.sample(&mut rng));
    }
    visits
}

/// Whether a persona is ready for real work (spec §6.1 step 5): warm-up is
/// off or has nothing to visit, enough visits are done, or the longest
/// wait is over.
pub fn ready(
    planned: usize,
    handled: u32,
    created_at: DateTime<Utc>,
    now: DateTime<Utc>,
    config: &WarmupConfig,
) -> bool {
    if !config.enabled || planned == 0 {
        return true;
    }
    let enough = handled >= config.min_visits
        || usize::try_from(handled).is_ok_and(|handled| handled >= planned);
    let waited = Duration::from_std(config.max_wait.get())
        .is_ok_and(|longest| now.signed_duration_since(created_at) >= longest);
    enough || waited
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};

    use super::{PlannedVisit, WarmupSubject, plan, ready};
    use crate::config::{WarmupConfig, WarmupSite};
    use crate::domain::CountryCode;
    use crate::duration::HumanDuration;
    use crate::persona::place::local_time;
    use crate::seed::Seed;

    /// Noon in Vienna.
    fn generated() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 9, 10, 0, 0).unwrap()
    }

    fn austrian(seed: &Seed, created_at: DateTime<Utc>) -> WarmupSubject<'_> {
        WarmupSubject {
            seed,
            created_at,
            timezone: "Europe/Vienna",
            country: Some("AT".parse::<CountryCode>().unwrap()),
            locale: "de-AT",
        }
    }

    fn plan_of(seed: u64) -> Vec<PlannedVisit> {
        let seed = Seed::from_u64(seed);
        plan(&austrian(&seed, generated()), &WarmupConfig::default())
    }

    #[test]
    fn a_persona_makes_six_to_ten_visits_to_sites_of_its_own() {
        let mut counts = BTreeSet::new();
        let mut hosts: BTreeMap<String, u32> = BTreeMap::new();
        for seed in 0..400 {
            let visits = plan_of(seed);
            assert!((6..=10).contains(&visits.len()), "{}", visits.len());
            counts.insert(visits.len());
            for (index, visit) in visits.iter().enumerate() {
                assert_eq!(visit.number as usize, index);
                *hosts.entry(visit.domain().to_owned()).or_default() += 1;
                // Never the same page twice in a row.
                if index > 0 {
                    assert_ne!(visit.url, visits[index - 1].url, "seed {seed}");
                }
            }
        }
        assert_eq!(counts.into_iter().collect::<Vec<_>>(), [6, 7, 8, 9, 10]);
        // An Austrian reads the German Wikipedia, Austrian news and
        // weather, and the map; nothing of another country.
        let visited: Vec<&str> = hosts.keys().map(String::as_str).collect();
        assert_eq!(
            visited,
            [
                "de.wikipedia.org",
                "orf.at",
                "www.geosphere.at",
                "www.openstreetmap.org"
            ]
        );
        // Its own country's sites are the likelier ones.
        assert!(hosts["orf.at"] > hosts["www.openstreetmap.org"]);
    }

    #[test]
    fn visits_are_spread_over_hours_and_never_at_night() {
        let from = NaiveTime::from_hms_opt(7, 0, 0).unwrap();
        let until = NaiveTime::from_hms_opt(23, 30, 0).unwrap();
        let mut gaps = Vec::new();
        for seed in 0..300_u64 {
            // Personas are generated at every hour of the day.
            let created_at = generated() + Duration::minutes(i64::try_from(seed).unwrap() * 37);
            let seed = Seed::from_u64(seed);
            let visits = plan(&austrian(&seed, created_at), &WarmupConfig::default());
            let first = visits[0].due_at - created_at;
            assert!(first >= Duration::minutes(3), "{first}");
            for (index, visit) in visits.iter().enumerate() {
                let time = local_time(visit.due_at, "Europe/Vienna").time();
                assert!(time >= from && time < until, "visit at {time}");
                if index > 0 {
                    let gap = visit.due_at - visits[index - 1].due_at;
                    assert!(gap >= Duration::minutes(2), "{gap}");
                    gaps.push(gap);
                }
            }
        }
        // The first visit of a persona generated at noon is 3 to 20
        // minutes later.
        for seed in 0..100 {
            let first = plan_of(seed)[0].due_at - generated();
            assert!(
                first >= Duration::minutes(3) && first <= Duration::minutes(20),
                "{first}"
            );
        }
        // Gaps: a median near 25 minutes among those that did not cross a
        // night.
        let mut within_a_day: Vec<i64> = gaps
            .iter()
            .filter(|gap| **gap < Duration::hours(6))
            .map(Duration::num_minutes)
            .collect();
        within_a_day.sort_unstable();
        let median = within_a_day[within_a_day.len() / 2];
        assert!((18..=32).contains(&median), "median gap {median} min");
    }

    #[test]
    fn the_plan_is_the_personas_own_and_always_the_same() {
        assert_eq!(plan_of(5), plan_of(5));
        let orders: BTreeSet<String> = (0..20)
            .map(|seed| {
                plan_of(seed)
                    .iter()
                    .map(|visit| visit.url.as_str().to_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        // Twenty personas, twenty walks through the same list.
        assert_eq!(orders.len(), 20);
        let starts: BTreeSet<DateTime<Utc>> = (0..20).map(|seed| plan_of(seed)[0].due_at).collect();
        assert_eq!(starts.len(), 20);
    }

    #[test]
    fn the_language_and_the_country_choose_the_sites() {
        let seed = Seed::from_u64(1);
        let british = WarmupSubject {
            seed: &seed,
            created_at: generated(),
            timezone: "Europe/London",
            country: Some("GB".parse::<CountryCode>().unwrap()),
            locale: "en-GB",
        };
        let hosts: BTreeSet<String> = (0..50)
            .flat_map(|index| {
                let seed = Seed::from_u64(index);
                plan(
                    &WarmupSubject {
                        seed: &seed,
                        ..british
                    },
                    &WarmupConfig::default(),
                )
            })
            .map(|visit| visit.domain().to_owned())
            .collect();
        assert_eq!(
            hosts.into_iter().collect::<Vec<_>>(),
            [
                "en.wikipedia.org",
                "www.bbc.co.uk",
                "www.metoffice.gov.uk",
                "www.openstreetmap.org"
            ]
        );

        // A country without entries of its own still has the ones for
        // everyone; a persona of no country likewise.
        let elsewhere = WarmupSubject {
            country: Some("SE".parse::<CountryCode>().unwrap()),
            locale: "sv-SE",
            ..british
        };
        let visits = plan(&elsewhere, &WarmupConfig::default());
        assert!(
            visits.iter().all(
                |visit| ["sv.wikipedia.org", "www.openstreetmap.org"].contains(&visit.domain())
            )
        );
        let nowhere = WarmupSubject {
            country: None,
            ..british
        };
        assert_ne!(
            plan(&nowhere, &WarmupConfig::default()),
            Vec::<PlannedVisit>::new()
        );
    }

    #[test]
    fn no_warmup_means_no_plan_and_a_persona_that_is_ready_at_once() {
        let seed = Seed::from_u64(1);
        let subject = austrian(&seed, generated());
        let now = generated();

        let off = WarmupConfig {
            enabled: false,
            ..WarmupConfig::default()
        };
        assert_eq!(plan(&subject, &off), []);
        assert!(ready(0, 0, generated(), now, &off));

        // Sites of other countries only: nothing for this persona.
        let foreign = WarmupConfig {
            sites: vec![WarmupSite {
                url: "https://www.tagesschau.de/".to_owned(),
                countries: vec!["DE".parse().unwrap()],
            }],
            ..WarmupConfig::default()
        };
        assert_eq!(plan(&subject, &foreign), []);
        assert!(ready(0, 0, generated(), now, &foreign));

        // One site: it is visited again and again, since there is no other.
        let single = WarmupConfig {
            sites: vec![WarmupSite {
                url: "https://{language}.wikipedia.org/wiki/Special:Random".to_owned(),
                countries: Vec::new(),
            }],
            ..WarmupConfig::default()
        };
        let visits = plan(&subject, &single);
        assert!(visits.len() >= 6);
        assert!(
            visits
                .iter()
                .all(|visit| visit.url.as_str() == "https://de.wikipedia.org/wiki/Special:Random")
        );
    }

    #[test]
    fn a_persona_is_ready_after_three_visits_or_twelve_hours() {
        let config = WarmupConfig::default();
        let created = generated();
        let soon = created + Duration::hours(1);

        assert!(!ready(8, 0, created, soon, &config));
        assert!(!ready(8, 2, created, soon, &config));
        assert!(ready(8, 3, created, soon, &config));
        assert!(ready(8, 8, created, soon, &config));
        // The longest wait ends it whatever happened.
        assert!(!ready(
            8,
            0,
            created,
            created + Duration::hours(12) - Duration::seconds(1),
            &config
        ));
        assert!(ready(8, 0, created, created + Duration::hours(12), &config));
        // A plan shorter than the minimum is done when it is done.
        let short = WarmupConfig {
            min_visits: 3,
            ..config.clone()
        };
        assert!(ready(2, 2, created, soon, &short));
        assert!(!ready(2, 1, created, soon, &short));
        let quick = WarmupConfig {
            max_wait: HumanDuration::from_mins(30),
            ..config
        };
        assert!(ready(8, 0, created, soon, &quick));
    }
}
