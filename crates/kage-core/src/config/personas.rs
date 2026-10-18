//! Configuration of personas and sessions (spec §5, §6): the mix of engines
//! and systems, where browser profiles live and how long a retired one is
//! kept, the warm-up visits of a new persona, and how long sessions last.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use chrono::NaiveTime;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::{CountryCode, SessionLimits};
use crate::duration::HumanDuration;
use crate::persona::mix::{self, MixEntry};
use crate::persona::place::ActiveHours;

/// Most sites a warm-up list may name.
pub const MAX_WARMUP_SITES: usize = 200;

/// Most visits one persona's warm-up may make.
pub const MAX_WARMUP_VISITS: u32 = 50;

/// The placeholder in a warm-up URL that stands for the language of the
/// persona's locale: `https://{language}.wikipedia.org/`.
pub const LANGUAGE_PLACEHOLDER: &str = "{language}";

/// A time of day written as `HH:MM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClockTime(NaiveTime);

impl ClockTime {
    /// A time of day; `None` when the hour or the minute does not exist.
    pub fn new(hour: u32, minute: u32) -> Option<Self> {
        NaiveTime::from_hms_opt(hour, minute, 0).map(Self)
    }

    /// A time of day, or midnight when the hour or the minute does not
    /// exist. For defaults written in the code, where both do.
    pub(crate) const fn new_or_midnight(hour: u32, minute: u32) -> Self {
        match NaiveTime::from_hms_opt(hour, minute, 0) {
            Some(time) => Self(time),
            None => Self(NaiveTime::MIN),
        }
    }

    /// The time.
    pub const fn get(self) -> NaiveTime {
        self.0
    }
}

impl fmt::Display for ClockTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.format("%H:%M"))
    }
}

/// A time of day that is not `HH:MM`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a time of day written as HH:MM")]
pub struct InvalidClockTime(pub String);

impl FromStr for ClockTime {
    type Err = InvalidClockTime;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidClockTime(text.to_owned());
        let (hour, minute) = text.split_once(':').ok_or_else(invalid)?;
        if hour.len() != 2 || minute.len() != 2 {
            return Err(invalid());
        }
        let hour: u32 = hour.parse().map_err(|_| invalid())?;
        let minute: u32 = minute.parse().map_err(|_| invalid())?;
        Self::new(hour, minute).ok_or_else(invalid)
    }
}

impl Serialize for ClockTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ClockTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// One site a new persona may visit while it warms up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmupSite {
    /// The page to load. `{language}` stands for the language of the
    /// persona's locale (`de`, `fr`, `en`).
    pub url: String,
    /// The countries whose personas visit it; empty for everyone. A site
    /// of a persona's own country is three times as likely to be drawn as
    /// one for everyone.
    #[serde(default)]
    pub countries: Vec<CountryCode>,
}

/// A whole number of visits, from `min` to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisitRange {
    /// Fewest.
    pub min: u32,
    /// Most.
    pub max: u32,
}

/// A span of time, from `min` to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelayRange {
    /// Shortest.
    pub min: HumanDuration,
    /// Longest.
    pub max: HumanDuration,
}

/// The time between two warm-up visits: a log-normal with this median and
/// this 95th percentile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GapConfig {
    /// Half of the gaps are shorter.
    pub median: HumanDuration,
    /// Nineteen gaps in twenty are shorter.
    pub p95: HumanDuration,
}

/// The hours in which a persona does anything by itself, in its own time
/// zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveHoursConfig {
    /// The first minute of the day.
    pub from: ClockTime,
    /// The minute after which nothing starts any more.
    pub until: ClockTime,
}

impl ActiveHoursConfig {
    /// The hours as the planner takes them.
    pub const fn hours(&self) -> ActiveHours {
        ActiveHours {
            from: self.from.get(),
            until: self.until.get(),
        }
    }
}

/// The warm-up of a new persona (spec §6.1 step 5): a few visits to
/// popular neutral sites during its first hours, through its own browser
/// with human behavior, so that its profile is not empty when it first
/// meets a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarmupConfig {
    /// Whether new personas warm up at all. Off: a persona is ready for
    /// work at once, with an empty profile.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Finished visits after which a persona is ready for real work.
    #[serde(default = "default_min_visits")]
    pub min_visits: u32,
    /// A persona is ready after this long whatever its warm-up did, so that
    /// a warm-up that cannot run never keeps a persona out for good.
    #[serde(default = "default_max_wait")]
    pub max_wait: HumanDuration,
    /// How many visits one persona makes; the number is drawn from its
    /// seed.
    #[serde(default = "default_visits")]
    pub visits: VisitRange,
    /// How long after generation the first visit happens.
    #[serde(default = "default_first_visit_after")]
    pub first_visit_after: DelayRange,
    /// The time between two visits.
    #[serde(default = "default_gap")]
    pub gap: GapConfig,
    /// A visit that would fall outside these hours of the persona's own
    /// time zone waits for the next morning.
    #[serde(default = "default_active_hours")]
    pub active_hours: ActiveHoursConfig,
    /// The sites. Each persona takes its own selection in its own order.
    #[serde(default = "default_sites")]
    pub sites: Vec<WarmupSite>,
}

const fn default_true() -> bool {
    true
}

const fn default_min_visits() -> u32 {
    3
}

const fn default_max_wait() -> HumanDuration {
    HumanDuration::from_hours(12)
}

const fn default_visits() -> VisitRange {
    VisitRange { min: 6, max: 10 }
}

const fn default_first_visit_after() -> DelayRange {
    DelayRange {
        min: HumanDuration::from_mins(3),
        max: HumanDuration::from_mins(20),
    }
}

const fn default_gap() -> GapConfig {
    GapConfig {
        median: HumanDuration::from_mins(25),
        p95: HumanDuration::from_mins(90),
    }
}

fn default_active_hours() -> ActiveHoursConfig {
    // Both times exist.
    ActiveHoursConfig {
        from: ClockTime(NaiveTime::from_hms_opt(7, 0, 0).unwrap_or(NaiveTime::MIN)),
        until: ClockTime(NaiveTime::from_hms_opt(23, 30, 0).unwrap_or(NaiveTime::MIN)),
    }
}

/// Reference and public-service pages that are among the most visited in
/// each of the pool's countries, are no scraping targets, and tolerate a
/// reader who looks and leaves: the Wikipedia of the persona's language,
/// OpenStreetMap, and per country the public broadcaster's news front page
/// and the national weather service.
fn default_sites() -> Vec<WarmupSite> {
    let everyone = |url: &str| WarmupSite {
        url: url.to_owned(),
        countries: Vec::new(),
    };
    let of = |code: &str, url: &str| WarmupSite {
        url: url.to_owned(),
        countries: CountryCode::new(code).into_iter().collect(),
    };
    vec![
        everyone("https://{language}.wikipedia.org/"),
        everyone("https://{language}.wikipedia.org/wiki/Special:Random"),
        everyone("https://www.openstreetmap.org/"),
        of("AT", "https://orf.at/"),
        of("AT", "https://www.geosphere.at/"),
        of("DE", "https://www.tagesschau.de/"),
        of("DE", "https://www.dwd.de/"),
        of("FR", "https://www.franceinfo.fr/"),
        of("FR", "https://meteofrance.com/"),
        of("GB", "https://www.bbc.co.uk/news"),
        of("GB", "https://www.metoffice.gov.uk/"),
        of("IT", "https://www.rainews.it/"),
        of("IT", "https://www.meteoam.it/"),
    ]
}

impl Default for WarmupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_visits: default_min_visits(),
            max_wait: default_max_wait(),
            visits: default_visits(),
            first_visit_after: default_first_visit_after(),
            gap: default_gap(),
            active_hours: default_active_hours(),
            sites: default_sites(),
        }
    }
}

/// Personas (spec §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersonasConfig {
    /// The personas volume as Kage sees it: the directory the sidecars keep
    /// browser profiles in. Kage archives and deletes the profile of a
    /// retired persona there; it never reads a profile.
    #[serde(default = "default_personas_dir")]
    pub dir: PathBuf,
    /// The shares of engines and claimed systems among personas.
    #[serde(default = "mix::default_mix")]
    pub mix: Vec<MixEntry>,
    /// Days the archived profile of a retired persona is kept.
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Days the copy of a profile that a sidecar takes before the
    /// profile's first start under a newer browser major
    /// (`.upgrade/<persona id>` on the volume) is kept: the way back to the
    /// previous engine image, since a Firefox profile does not go back to
    /// an older major.
    #[serde(default = "default_upgrade_backup_days")]
    pub upgrade_backup_days: u32,
    /// How long after retirement a profile is archived. Longer than the
    /// sidecar's idle close, so that no browser still has it open.
    #[serde(default = "default_archive_after")]
    pub archive_after: HumanDuration,
    /// How often the persona manager looks for work that no event
    /// announces: proxies without a persona, warm-up visits and self-tests
    /// that are due, profiles to archive or delete.
    #[serde(default = "default_sweep_interval")]
    pub sweep_interval: HumanDuration,
    /// The warm-up of a new persona.
    #[serde(default)]
    pub warmup: WarmupConfig,
}

fn default_personas_dir() -> PathBuf {
    PathBuf::from("personas")
}

const fn default_retention_days() -> u32 {
    7
}

const fn default_upgrade_backup_days() -> u32 {
    7
}

const fn default_archive_after() -> HumanDuration {
    HumanDuration::from_mins(10)
}

const fn default_sweep_interval() -> HumanDuration {
    HumanDuration::from_mins(1)
}

impl Default for PersonasConfig {
    fn default() -> Self {
        Self {
            dir: default_personas_dir(),
            mix: mix::default_mix(),
            retention_days: default_retention_days(),
            upgrade_backup_days: default_upgrade_backup_days(),
            archive_after: default_archive_after(),
            sweep_interval: default_sweep_interval(),
            warmup: WarmupConfig::default(),
        }
    }
}

impl PersonasConfig {
    /// How long the archived profile of a retired persona is kept.
    pub fn retention(&self) -> Duration {
        Duration::from_secs(u64::from(self.retention_days) * 24 * 60 * 60)
    }

    /// How long the copy of a profile taken before a browser upgrade is
    /// kept.
    pub fn upgrade_backup(&self) -> Duration {
        Duration::from_secs(u64::from(self.upgrade_backup_days) * 24 * 60 * 60)
    }
}

/// Sessions (spec §5): handles that pin a caller's requests to one persona.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsConfig {
    /// A session ends after this long without use.
    #[serde(default = "default_idle_ttl")]
    pub idle_ttl: HumanDuration,
    /// A session ends this long after it was opened, however busy it is.
    #[serde(default = "default_max_lifetime")]
    pub max_lifetime: HumanDuration,
    /// How often ended sessions are removed.
    #[serde(default = "default_sweep_interval")]
    pub sweep_interval: HumanDuration,
}

const fn default_idle_ttl() -> HumanDuration {
    HumanDuration::from_mins(10)
}

const fn default_max_lifetime() -> HumanDuration {
    HumanDuration::from_hours(6)
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            idle_ttl: default_idle_ttl(),
            max_lifetime: default_max_lifetime(),
            sweep_interval: default_sweep_interval(),
        }
    }
}

impl SessionsConfig {
    /// The limits as the session type takes them.
    pub const fn limits(&self) -> SessionLimits {
        SessionLimits {
            idle_ttl: self.idle_ttl.get(),
            max_lifetime: self.max_lifetime.get(),
        }
    }
}

/// A warm-up URL with its placeholder filled in, when the result is an
/// `https` URL with a host.
pub fn warmup_url(template: &str, language: &str) -> Option<url::Url> {
    let filled = template.replace(LANGUAGE_PLACEHOLDER, language);
    url::Url::parse(&filled)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host_str().is_some())
}

/// Checks the two sections; returns the first problem as a message that
/// names the key. `allow_cross_os_claims` is
/// `engines.chromium.allow_cross_os_claims`.
pub(super) fn validate(
    personas: &PersonasConfig,
    sessions: &SessionsConfig,
    allow_cross_os_claims: bool,
) -> Result<(), String> {
    let fail = |message: &str| Err(message.to_owned());
    let second = Duration::from_secs(1);

    if personas.dir.as_os_str().is_empty() {
        return fail("personas.dir must not be empty");
    }
    mix::validate(&personas.mix, allow_cross_os_claims).map_err(|error| error.to_string())?;
    if personas.retention_days == 0 {
        return fail("personas.retention_days must be at least 1");
    }
    if personas.upgrade_backup_days == 0 {
        return fail("personas.upgrade_backup_days must be at least 1");
    }
    if personas.sweep_interval.get() < second {
        return fail("personas.sweep_interval must be at least 1s");
    }

    let warmup = &personas.warmup;
    if warmup.visits.min > warmup.visits.max {
        return fail("personas.warmup.visits: min must not be greater than max");
    }
    if warmup.visits.max > MAX_WARMUP_VISITS {
        return Err(format!(
            "personas.warmup.visits.max must be at most {MAX_WARMUP_VISITS}"
        ));
    }
    if warmup.min_visits > warmup.visits.min {
        return fail(
            "personas.warmup.min_visits must not be greater than personas.warmup.visits.min, or a \
             persona could never finish enough visits",
        );
    }
    if warmup.max_wait.get() < second {
        return fail("personas.warmup.max_wait must be at least 1s");
    }
    if warmup.first_visit_after.min > warmup.first_visit_after.max {
        return fail("personas.warmup.first_visit_after: min must not be greater than max");
    }
    if warmup.gap.median.get() < second {
        return fail("personas.warmup.gap.median must be at least 1s");
    }
    if warmup.gap.p95 < warmup.gap.median {
        return fail("personas.warmup.gap.p95 must not be less than the median");
    }
    if warmup.active_hours.from >= warmup.active_hours.until {
        return fail("personas.warmup.active_hours: from must be earlier than until");
    }
    if warmup.sites.len() > MAX_WARMUP_SITES {
        return Err(format!(
            "personas.warmup.sites must name at most {MAX_WARMUP_SITES} sites"
        ));
    }
    for site in &warmup.sites {
        if warmup_url(&site.url, "en").is_none() {
            return Err(format!(
                "personas.warmup.sites: {:?} is not an https URL",
                site.url
            ));
        }
    }

    if sessions.idle_ttl.get() < second {
        return fail("sessions.idle_ttl must be at least 1s");
    }
    if sessions.max_lifetime < sessions.idle_ttl {
        return fail("sessions.max_lifetime must not be shorter than sessions.idle_ttl");
    }
    if sessions.sweep_interval.get() < second {
        return fail("sessions.sweep_interval must be at least 1s");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ClockTime, InvalidClockTime, PersonasConfig, SessionsConfig, WarmupSite, validate,
        warmup_url,
    };
    use crate::domain::{ClaimedOs, Engine, SessionLimits};
    use crate::duration::HumanDuration;
    use crate::persona::mix::MixEntry;

    #[test]
    fn the_defaults_are_the_specs_and_valid() {
        let personas = PersonasConfig::default();
        let sessions = SessionsConfig::default();

        assert_eq!(validate(&personas, &sessions, false), Ok(()));
        assert_eq!(personas.retention_days, 7);
        assert_eq!(personas.retention(), Duration::from_hours(7 * 24));
        assert_eq!(personas.upgrade_backup_days, 7);
        assert_eq!(personas.upgrade_backup(), Duration::from_hours(7 * 24));
        assert_eq!(personas.archive_after, HumanDuration::from_mins(10));
        let shares: Vec<(Engine, ClaimedOs, u32)> = personas
            .mix
            .iter()
            .map(|entry| (entry.engine, entry.os, entry.weight))
            .collect();
        assert_eq!(
            shares,
            [
                (Engine::Chromium, ClaimedOs::Linux, 40),
                (Engine::Firefox, ClaimedOs::Windows, 45),
                (Engine::Firefox, ClaimedOs::Macos, 15),
            ]
        );
        let warmup = &personas.warmup;
        assert!(warmup.enabled);
        assert_eq!(warmup.min_visits, 3);
        assert_eq!(warmup.max_wait, HumanDuration::from_hours(12));
        assert_eq!((warmup.visits.min, warmup.visits.max), (6, 10));
        assert_eq!(warmup.active_hours.from.to_string(), "07:00");
        assert_eq!(warmup.active_hours.until.to_string(), "23:30");
        // Every country of the pool has its broadcaster and its weather
        // service, and everyone has the reference sites.
        for country in ["AT", "DE", "FR", "GB", "IT"] {
            let own = warmup
                .sites
                .iter()
                .filter(|site| site.countries.iter().any(|code| code.as_str() == country))
                .count();
            assert_eq!(own, 2, "{country}");
        }
        assert_eq!(
            warmup
                .sites
                .iter()
                .filter(|site| site.countries.is_empty())
                .count(),
            3
        );
        assert_eq!(sessions.limits(), SessionLimits::default());
    }

    #[test]
    fn clock_times_read_and_write_as_hours_and_minutes() {
        let time: ClockTime = "07:05".parse().unwrap();
        assert_eq!(time, ClockTime::new(7, 5).unwrap());
        assert_eq!(time.to_string(), "07:05");
        assert_eq!(serde_json::to_string(&time).unwrap(), "\"07:05\"");
        assert_eq!(
            serde_json::from_str::<ClockTime>("\"23:30\"")
                .unwrap()
                .to_string(),
            "23:30"
        );
        for bad in [
            "", "7", "7:00", "07:5", "24:00", "12:60", "ab:cd", "07:00:00",
        ] {
            assert_eq!(
                bad.parse::<ClockTime>(),
                Err(InvalidClockTime(bad.to_owned())),
                "{bad:?}"
            );
        }
        assert!(serde_json::from_str::<ClockTime>("\"25:00\"").is_err());
        assert_eq!(ClockTime::new(24, 0), None);
    }

    #[test]
    fn a_warmup_url_takes_the_personas_language() {
        assert_eq!(
            warmup_url("https://{language}.wikipedia.org/wiki/Special:Random", "de")
                .unwrap()
                .as_str(),
            "https://de.wikipedia.org/wiki/Special:Random"
        );
        assert_eq!(
            warmup_url("https://orf.at/", "de").unwrap().host_str(),
            Some("orf.at")
        );
        for bad in ["http://orf.at/", "orf.at", "https://", "ftp://x/", ""] {
            assert_eq!(warmup_url(bad, "de"), None, "{bad:?}");
        }
    }

    // One table of every bound, read in one piece.
    #[allow(clippy::too_many_lines)]
    #[test]
    fn out_of_range_values_are_rejected_by_name() {
        type Change = fn(&mut PersonasConfig, &mut SessionsConfig);
        let cases: &[(&str, Change)] = &[
            ("personas.dir", |p, _| p.dir = "".into()),
            ("personas.mix must name", |p, _| p.mix.clear()),
            ("personas.mix: at least one", |p, _| {
                for entry in &mut p.mix {
                    entry.weight = 0;
                }
            }),
            (
                "personas.mix names chromium on linux more than once",
                |p, _| {
                    let first = p.mix[0];
                    p.mix.push(first);
                },
            ),
            ("personas.mix names chromium on windows", |p, _| {
                p.mix.push(MixEntry {
                    engine: Engine::Chromium,
                    os: ClaimedOs::Windows,
                    weight: 10,
                });
            }),
            ("personas.retention_days", |p, _| p.retention_days = 0),
            ("personas.upgrade_backup_days", |p, _| {
                p.upgrade_backup_days = 0;
            }),
            ("personas.sweep_interval", |p, _| {
                p.sweep_interval = HumanDuration::from_secs(0);
            }),
            ("personas.warmup.visits: min", |p, _| {
                p.warmup.visits.min = 11;
            }),
            ("personas.warmup.visits.max", |p, _| {
                p.warmup.visits.max = 51;
            }),
            ("personas.warmup.min_visits", |p, _| {
                p.warmup.min_visits = 7;
            }),
            ("personas.warmup.max_wait", |p, _| {
                p.warmup.max_wait = HumanDuration::from_secs(0);
            }),
            ("personas.warmup.first_visit_after", |p, _| {
                p.warmup.first_visit_after.min = HumanDuration::from_hours(1);
            }),
            ("personas.warmup.gap.median", |p, _| {
                p.warmup.gap.median = HumanDuration::from_secs(0);
            }),
            ("personas.warmup.gap.p95", |p, _| {
                p.warmup.gap.p95 = HumanDuration::from_mins(5);
            }),
            ("personas.warmup.active_hours", |p, _| {
                p.warmup.active_hours.from = p.warmup.active_hours.until;
            }),
            ("personas.warmup.sites must name at most", |p, _| {
                let site = p.warmup.sites[0].clone();
                p.warmup.sites = vec![site; 201];
            }),
            (
                "personas.warmup.sites: \"http://plain.example/\"",
                |p, _| {
                    p.warmup.sites.push(WarmupSite {
                        url: "http://plain.example/".to_owned(),
                        countries: Vec::new(),
                    });
                },
            ),
            ("sessions.idle_ttl", |_, s| {
                s.idle_ttl = HumanDuration::from_secs(0);
            }),
            ("sessions.max_lifetime", |_, s| {
                s.max_lifetime = HumanDuration::from_mins(5);
            }),
            ("sessions.sweep_interval", |_, s| {
                s.sweep_interval = HumanDuration::from_secs(0);
            }),
        ];
        for (expected, change) in cases {
            let mut personas = PersonasConfig::default();
            let mut sessions = SessionsConfig::default();
            change(&mut personas, &mut sessions);
            let error = validate(&personas, &sessions, false).unwrap_err();
            assert!(error.starts_with(expected), "{expected}: {error}");
        }

        // The same chromium entry is fine once cross-OS claims are allowed,
        // and warm-up may be switched off or given no sites.
        let mut personas = PersonasConfig::default();
        personas.mix.push(MixEntry {
            engine: Engine::Chromium,
            os: ClaimedOs::Windows,
            weight: 10,
        });
        personas.warmup.enabled = false;
        personas.warmup.sites.clear();
        assert_eq!(
            validate(&personas, &SessionsConfig::default(), true),
            Ok(())
        );
    }
}
