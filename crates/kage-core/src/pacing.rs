//! Pacing profiles and their seeded samplers (spec §11).
//!
//! A profile describes how one persona spaces its requests to one domain:
//! the gap between pages, how many pages make a burst, how long the break
//! after a burst is, and the caps the scheduler enforces. Profiles are data
//! in `policies/pacing-profiles.yaml`.
//!
//! # Shape of a bounded log-normal
//!
//! A log-normal with a `min` is sampled as `min + LogNormal`, fitted so that
//! the total still has exactly the configured `median` and `p95`. The
//! density rises smoothly from zero at `min`: there is neither a pile of
//! samples at the floor (what clamping gives) nor a hard edge there (what
//! rejection gives). An optional `max` is enforced by drawing again, never
//! by clamping, for the same reason.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::seed::{Seed, SeededRng};

/// Standard normal quantile at 0.95: `p95 = median * exp(sigma * Z95)`.
const Z95: f64 = 1.644_853_626_951_472_2;

/// Draws allowed before a sample above `max` is replaced by `max` itself.
/// Validation keeps `max >= p95`, so each draw is accepted with probability
/// at least 0.95 and the fallback is unreachable in practice.
const MAX_REDRAWS: usize = 64;

/// A problem with a pacing profile or a reference to one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PacingError {
    /// The YAML document could not be parsed into profiles.
    #[error("pacing profiles: {0}")]
    Parse(String),
    /// A profile name is not a lowercase slug or is reserved.
    #[error(
        "pacing profile name {0:?} is not allowed: use a-z, 0-9, _ or -, starting with a letter; \"custom\" is reserved"
    )]
    InvalidName(String),
    /// A field of a profile is out of range.
    #[error("pacing profile {profile:?}: {field}: {reason}")]
    InvalidField {
        /// The profile.
        profile: String,
        /// The field inside it.
        field: &'static str,
        /// What is wrong.
        reason: String,
    },
    /// The global default profile is not defined.
    #[error("pacing profiles must define {0:?}, the global default")]
    MissingDefault(&'static str),
    /// A request or a policy names a profile that does not exist.
    #[error("pacing profile {0:?} is not defined")]
    UnknownProfile(String),
}

/// Parameters of a log-normal distribution of durations, in milliseconds.
///
/// The parameters are a plain value, and sampling takes them together with
/// a random stream, so whoever resolves a profile for a persona can adjust
/// them first. That is the hook for per-persona variation: the persona
/// manager (milestones M6 and M7) is meant to scale `median` and `p95` by a
/// fixed factor drawn once from the persona's seed, roughly in
/// `[0.8, 1.25]`, so that twenty personas do not share one identical gap
/// distribution, which would correlate them with each other.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lognormal {
    /// The median.
    pub median: f64,
    /// The 95th percentile.
    pub p95: f64,
    /// No sample is at or below this. Defaults to 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// No sample is above this. Defaults to unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

impl Lognormal {
    /// The same distribution at a persona's own pace: the median and the
    /// 95th percentile are multiplied by `factor`. The floor and the
    /// ceiling are limits of the profile, not of the persona, and stay:
    /// when the scaled median would fall to the floor, only the part above
    /// the floor is scaled, and a p95 that would rise above the ceiling
    /// stops there. A factor that is not a positive number changes nothing.
    #[must_use]
    pub fn scaled(&self, factor: f64) -> Self {
        if !(factor.is_finite() && factor > 0.0) {
            return *self;
        }
        let floor = self.min.unwrap_or(0.0);
        let (mut median, mut p95) = (self.median * factor, self.p95 * factor);
        if median <= floor {
            median = floor + (self.median - floor) * factor;
            p95 = floor + (self.p95 - floor) * factor;
        }
        p95 = p95.max(median);
        if let Some(max) = self.max {
            p95 = p95.min(max);
            median = median.min(p95);
        }
        Self {
            median,
            p95,
            min: self.min,
            max: self.max,
        }
    }

    /// Checks that the parameters describe a distribution.
    ///
    /// # Errors
    ///
    /// Returns a description of the first parameter that is out of range.
    pub fn validate(&self) -> Result<(), String> {
        let floor = self.min.unwrap_or(0.0);
        non_negative("median", self.median)?;
        non_negative("p95", self.p95)?;
        non_negative("min", floor)?;
        if self.median <= floor {
            return Err(format!(
                "median {} must be greater than min {floor}",
                self.median
            ));
        }
        if self.p95 < self.median {
            return Err(format!(
                "p95 {} is less than median {}",
                self.p95, self.median
            ));
        }
        if let Some(max) = self.max {
            non_negative("max", max)?;
            if max < self.p95 {
                return Err(format!("max {max} is less than p95 {}", self.p95));
            }
        }
        Ok(())
    }

    /// Draws one value.
    ///
    /// With a `min` this is a shifted log-normal: `min` plus a log-normal
    /// fitted to the part above it, so the total has exactly the configured
    /// median and p95 and its density rises smoothly from zero at `min`.
    /// A `max` is enforced by drawing again, never by clamping.
    pub fn sample(&self, rng: &mut SeededRng) -> f64 {
        let floor = self.min.unwrap_or(0.0);
        let mu = (self.median - floor).ln();
        let sigma = ((self.p95 - floor) / (self.median - floor)).ln() / Z95;
        for _ in 0..MAX_REDRAWS {
            let value = floor + (mu + sigma * rng.standard_normal()).exp();
            if self.max.is_none_or(|max| value <= max) {
                return value;
            }
        }
        self.max.unwrap_or(self.p95)
    }
}

/// Parameters of a uniform distribution of durations, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Uniform {
    /// Lower bound.
    pub min: f64,
    /// Upper bound.
    pub max: f64,
}

impl Uniform {
    /// Checks that the parameters describe a distribution.
    ///
    /// # Errors
    ///
    /// Returns a description of the first parameter that is out of range.
    pub fn validate(&self) -> Result<(), String> {
        non_negative("min", self.min)?;
        non_negative("max", self.max)?;
        if self.min > self.max {
            return Err(format!("min {} is greater than max {}", self.min, self.max));
        }
        Ok(())
    }

    /// Draws one value in `[min, max)`.
    pub fn sample(&self, rng: &mut SeededRng) -> f64 {
        rng.uniform(self.min, self.max)
    }
}

fn non_negative(name: &str, value: f64) -> Result<(), String> {
    if value.is_finite() && value >= 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be a non-negative number, got {value}"))
    }
}

/// A distribution of durations in milliseconds, as written in a profile:
/// `{ dist: lognormal, median, p95, min, max }` or
/// `{ dist: uniform, min, max }`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "dist", rename_all = "snake_case")]
pub enum Distribution {
    /// Log-normal given by its median and 95th percentile, optionally
    /// bounded below by `min` and above by `max`.
    Lognormal(Lognormal),
    /// Uniform between `min` and `max`.
    Uniform(Uniform),
}

impl Distribution {
    /// Checks that the parameters describe a distribution.
    ///
    /// # Errors
    ///
    /// Returns a description of the first parameter that is out of range.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Lognormal(parameters) => parameters.validate(),
            Self::Uniform(parameters) => parameters.validate(),
        }
    }

    /// Draws one value in milliseconds.
    pub fn sample(&self, rng: &mut SeededRng) -> f64 {
        match self {
            Self::Lognormal(parameters) => parameters.sample(rng),
            Self::Uniform(parameters) => parameters.sample(rng),
        }
    }

    /// Draws one duration, rounded to whole milliseconds.
    pub fn sample_duration(&self, rng: &mut SeededRng) -> Duration {
        Duration::from_millis(to_millis(self.sample(rng)))
    }
}

/// Rounds a non-negative float to whole milliseconds.
fn to_millis(value: f64) -> u64 {
    // Float-to-integer `as` saturates and maps NaN to zero, which is the
    // behaviour wanted here; the value is rounded and non-negative first.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let millis = value.max(0.0).round() as u64;
    millis
}

/// An inclusive range of whole numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntRange {
    /// Smallest value.
    pub min: u32,
    /// Largest value.
    pub max: u32,
}

/// How one persona spaces its requests to one domain.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PacingProfile {
    /// Gap between two consecutive pages.
    pub gap_ms: Distribution,
    /// How many pages make a burst.
    pub burst_pages: IntRange,
    /// Extra pause after a burst, on top of the normal gap.
    pub burst_break_ms: Distribution,
    /// Pages one persona may fetch from one domain per day.
    pub daily_cap_per_persona_per_domain: u32,
    /// Requests in flight to one domain at the same time, over all personas.
    pub domain_concurrency: u32,
    /// Requests per minute to one domain, over all personas.
    pub domain_rpm: u32,
    /// How many bursts make a visit. After the last burst of a visit the
    /// persona leaves the domain for `visit_break_ms`. Absent: a visit
    /// never ends by itself. Set together with `visit_break_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visit_bursts: Option<IntRange>,
    /// The pause after a visit, on top of the normal gap and in place of
    /// the burst break. Set together with `visit_bursts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visit_break_ms: Option<Distribution>,
}

impl PacingProfile {
    /// The profile as one persona paces it (spec §11): the log-normal gap
    /// and breaks are stretched by the persona's factor, which is drawn
    /// once from its seed ([`crate::persona::pacing_factor`], roughly 0.8
    /// to 1.25), so that twenty personas never share one identical gap
    /// distribution. Uniform distributions, the burst and visit lengths,
    /// the daily cap and the per-domain limits are the profile's and do not
    /// move.
    #[must_use]
    pub fn for_persona(&self, factor: f64) -> Self {
        let stretch = |distribution: &Distribution| match distribution {
            Distribution::Lognormal(lognormal) => Distribution::Lognormal(lognormal.scaled(factor)),
            Distribution::Uniform(uniform) => Distribution::Uniform(*uniform),
        };
        Self {
            gap_ms: stretch(&self.gap_ms),
            burst_break_ms: stretch(&self.burst_break_ms),
            visit_break_ms: self.visit_break_ms.as_ref().map(stretch),
            ..*self
        }
    }

    /// Checks every field.
    ///
    /// # Errors
    ///
    /// Returns the first field that is out of range, attributed to `name`.
    pub fn validate(&self, name: &str) -> Result<(), PacingError> {
        let invalid = |field: &'static str, reason: String| PacingError::InvalidField {
            profile: name.to_owned(),
            field,
            reason,
        };
        self.gap_ms
            .validate()
            .map_err(|reason| invalid("gap_ms", reason))?;
        self.burst_break_ms
            .validate()
            .map_err(|reason| invalid("burst_break_ms", reason))?;
        if self.burst_pages.min == 0 {
            return Err(invalid("burst_pages", "min must be at least 1".to_owned()));
        }
        if self.burst_pages.min > self.burst_pages.max {
            return Err(invalid(
                "burst_pages",
                format!(
                    "min {} is greater than max {}",
                    self.burst_pages.min, self.burst_pages.max
                ),
            ));
        }
        for (field, value) in [
            (
                "daily_cap_per_persona_per_domain",
                self.daily_cap_per_persona_per_domain,
            ),
            ("domain_concurrency", self.domain_concurrency),
            ("domain_rpm", self.domain_rpm),
        ] {
            if value == 0 {
                return Err(invalid(field, "must be at least 1".to_owned()));
            }
        }
        match (&self.visit_bursts, &self.visit_break_ms) {
            (None, None) => {}
            (Some(bursts), Some(pause)) => {
                pause
                    .validate()
                    .map_err(|reason| invalid("visit_break_ms", reason))?;
                if bursts.min == 0 {
                    return Err(invalid("visit_bursts", "min must be at least 1".to_owned()));
                }
                if bursts.min > bursts.max {
                    return Err(invalid(
                        "visit_bursts",
                        format!("min {} is greater than max {}", bursts.min, bursts.max),
                    ));
                }
            }
            (Some(_), None) => {
                return Err(invalid(
                    "visit_break_ms",
                    "is required when visit_bursts is set".to_owned(),
                ));
            }
            (None, Some(_)) => {
                return Err(invalid(
                    "visit_bursts",
                    "is required when visit_break_ms is set".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// The named profiles from `policies/pacing-profiles.yaml`. Always contains
/// [`PacingProfiles::DEFAULT`].
#[derive(Debug, Clone, PartialEq)]
pub struct PacingProfiles {
    profiles: BTreeMap<String, PacingProfile>,
    default: PacingProfile,
}

impl PacingProfiles {
    /// Name of the global default profile (spec §11).
    pub const DEFAULT: &'static str = "human";

    /// Name reported as `pacing.applied` when a request supplies its own
    /// profile inline. No stored profile may use it.
    pub const CUSTOM: &'static str = "custom";

    /// Validates a set of named profiles.
    ///
    /// # Errors
    ///
    /// Fails when a name or a field is invalid or when the default profile
    /// is missing.
    pub fn new(profiles: BTreeMap<String, PacingProfile>) -> Result<Self, PacingError> {
        for (name, profile) in &profiles {
            if !is_profile_name(name) {
                return Err(PacingError::InvalidName(name.clone()));
            }
            profile.validate(name)?;
        }
        let default = *profiles
            .get(Self::DEFAULT)
            .ok_or(PacingError::MissingDefault(Self::DEFAULT))?;
        Ok(Self { profiles, default })
    }

    /// Parses and validates the YAML document.
    ///
    /// # Errors
    ///
    /// Fails when the document does not parse or does not validate.
    pub fn from_yaml(text: &str) -> Result<Self, PacingError> {
        let profiles = crate::yaml::from_str(text).map_err(PacingError::Parse)?;
        Self::new(profiles)
    }

    /// The profile with this name.
    pub fn get(&self, name: &str) -> Option<&PacingProfile> {
        self.profiles.get(name)
    }

    /// The profile with this name.
    ///
    /// # Errors
    ///
    /// Fails with [`PacingError::UnknownProfile`] when there is none.
    pub fn require(&self, name: &str) -> Result<&PacingProfile, PacingError> {
        self.get(name)
            .ok_or_else(|| PacingError::UnknownProfile(name.to_owned()))
    }

    /// The global default profile.
    pub const fn default_profile(&self) -> &PacingProfile {
        &self.default
    }

    /// Profile names in alphabetical order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.profiles.keys().map(String::as_str)
    }

    /// Every profile by name.
    pub const fn as_map(&self) -> &BTreeMap<String, PacingProfile> {
        &self.profiles
    }
}

fn is_profile_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && name.len() <= 64
        && name != PacingProfiles::CUSTOM
}

/// The wait before a persona's next page on a domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// How long to wait.
    pub wait: Duration,
    /// Whether this wait ends a burst and so includes a break.
    pub after_burst: bool,
    /// Whether this wait ends a visit: the break is the long one, and the
    /// persona has left the domain until it is over. Only a profile with
    /// `visit_bursts` ever ends a visit.
    pub after_visit: bool,
}

/// Produces the sequence of waits for one persona on one domain.
///
/// The sequence is a function of the profile and the seed alone, so a
/// recorded seed replays it exactly.
///
/// The sampler owns its profile as a value and never looks one up by name.
/// Whoever resolves pacing for a persona passes the profile it wants
/// sampled, which is where a per-persona adjustment of the parameters goes
/// (see [`Lognormal`]); the seed is the second input and should be derived
/// from the persona and the domain.
#[derive(Debug)]
pub struct PacingSampler {
    profile: PacingProfile,
    rng: SeededRng,
    /// Pages left in the current burst, counting the next one to be fetched.
    remaining_in_burst: u32,
    /// Bursts left in the current visit, counting the one in progress.
    /// `None` when the profile has no visits.
    remaining_in_visit: Option<u32>,
}

impl PacingSampler {
    /// Starts a sequence. The first burst length is drawn immediately, and
    /// after it the length of the first visit when the profile has visits.
    pub fn new(profile: PacingProfile, seed: Seed) -> Self {
        let mut rng = seed.rng();
        let remaining_in_burst = draw_burst(&profile, &mut rng);
        let remaining_in_visit = draw_visit(&profile, &mut rng);
        Self {
            profile,
            rng,
            remaining_in_burst,
            remaining_in_visit,
        }
    }

    /// The profile being sampled.
    pub const fn profile(&self) -> &PacingProfile {
        &self.profile
    }

    /// Pages left in the current burst, counting the next one to be fetched.
    pub const fn remaining_in_burst(&self) -> u32 {
        self.remaining_in_burst
    }

    /// Bursts left in the current visit, counting the one in progress;
    /// `None` when the profile has no visits.
    pub const fn remaining_in_visit(&self) -> Option<u32> {
        self.remaining_in_visit
    }

    /// The wait between the page just fetched and the next one.
    ///
    /// Inside a burst this is a draw from `gap_ms`. After the last page of a
    /// burst it is a draw from `gap_ms` plus a draw from `burst_break_ms`, so
    /// a break is never shorter than an ordinary gap, and a new burst length
    /// is drawn. After the last burst of a visit the break is a draw from
    /// `visit_break_ms` instead, and the length of the next visit is drawn.
    pub fn next_gap(&mut self) -> Gap {
        let gap = self.profile.gap_ms.sample_duration(&mut self.rng);
        self.remaining_in_burst = self.remaining_in_burst.saturating_sub(1);
        if self.remaining_in_burst > 0 {
            return Gap {
                wait: gap,
                after_burst: false,
                after_visit: false,
            };
        }
        let last_of_visit = match &mut self.remaining_in_visit {
            Some(bursts) => {
                *bursts = bursts.saturating_sub(1);
                *bursts == 0
            }
            None => false,
        };
        let pause = match (&self.profile.visit_break_ms, last_of_visit) {
            (Some(away), true) => away.sample_duration(&mut self.rng),
            _ => self.profile.burst_break_ms.sample_duration(&mut self.rng),
        };
        self.remaining_in_burst = draw_burst(&self.profile, &mut self.rng);
        if last_of_visit {
            self.remaining_in_visit = draw_visit(&self.profile, &mut self.rng);
        }
        Gap {
            wait: gap + pause,
            after_burst: true,
            after_visit: last_of_visit,
        }
    }
}

fn draw_in(range: IntRange, rng: &mut SeededRng) -> u32 {
    let IntRange { min, max } = range;
    let drawn = rng.int_inclusive(u64::from(min), u64::from(max));
    // The draw lies between two u32 values.
    u32::try_from(drawn).unwrap_or(max).max(1)
}

fn draw_burst(profile: &PacingProfile, rng: &mut SeededRng) -> u32 {
    draw_in(profile.burst_pages, rng)
}

/// The number of bursts of the next visit. Nothing is drawn for a profile
/// without visits, so its sequence of waits is what it was before visits
/// existed.
fn draw_visit(profile: &PacingProfile, rng: &mut SeededRng) -> Option<u32> {
    profile.visit_bursts.map(|range| draw_in(range, rng))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::{
        Distribution, Gap, IntRange, Lognormal, PacingError, PacingProfile, PacingProfiles,
        PacingSampler, Uniform,
    };
    use crate::seed::Seed;

    const SPEC_YAML: &str = "
human:
  gap_ms:        { dist: lognormal, median: 4500, p95: 15000, min: 1500 }
  burst_pages:   { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 60000, p95: 240000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
fast:
  gap_ms:        { dist: uniform, min: 300, max: 1200 }
  burst_pages:   { min: 50, max: 50 }
  burst_break_ms: { dist: uniform, min: 0, max: 0 }
  daily_cap_per_persona_per_domain: 5000
  domain_concurrency: 10
  domain_rpm: 300
";

    fn profiles() -> PacingProfiles {
        PacingProfiles::from_yaml(SPEC_YAML).unwrap()
    }

    fn human() -> PacingProfile {
        *profiles().require("human").unwrap()
    }

    fn fast() -> PacingProfile {
        *profiles().require("fast").unwrap()
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let index = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[index]
    }

    fn sorted_samples(distribution: &Distribution, seed: u64, count: usize) -> Vec<f64> {
        let mut rng = Seed::from_u64(seed).rng();
        let mut samples: Vec<f64> = (0..count).map(|_| distribution.sample(&mut rng)).collect();
        samples.sort_by(f64::total_cmp);
        samples
    }

    #[test]
    fn spec_profiles_parse() {
        let profiles = profiles();

        assert_eq!(profiles.names().collect::<Vec<_>>(), ["fast", "human"]);
        assert_eq!(profiles.as_map().len(), 2);
        assert_eq!(profiles.default_profile(), profiles.get("human").unwrap());

        let human = human();
        assert_eq!(
            human.gap_ms,
            Distribution::Lognormal(Lognormal {
                median: 4500.0,
                p95: 15000.0,
                min: Some(1500.0),
                max: None
            })
        );
        assert_eq!(human.burst_pages, IntRange { min: 4, max: 14 });
        assert_eq!(human.daily_cap_per_persona_per_domain, 300);
        assert_eq!(human.domain_concurrency, 3);
        assert_eq!(human.domain_rpm, 20);

        let fast = fast();
        assert_eq!(
            fast.gap_ms,
            Distribution::Uniform(Uniform {
                min: 300.0,
                max: 1200.0
            })
        );
        assert_eq!(fast.burst_pages, IntRange { min: 50, max: 50 });
    }

    #[test]
    fn unknown_profiles_are_reported_by_name() {
        assert_eq!(
            profiles().require("warp"),
            Err(PacingError::UnknownProfile("warp".to_owned()))
        );
        assert!(profiles().get("warp").is_none());
    }

    #[test]
    fn the_default_profile_is_mandatory() {
        let only_fast = "fast:\n  gap_ms: { dist: uniform, min: 1, max: 2 }\n  burst_pages: { min: 1, max: 1 }\n  burst_break_ms: { dist: uniform, min: 0, max: 0 }\n  daily_cap_per_persona_per_domain: 1\n  domain_concurrency: 1\n  domain_rpm: 1\n";

        assert_eq!(
            PacingProfiles::from_yaml(only_fast),
            Err(PacingError::MissingDefault("human"))
        );
    }

    #[test]
    fn malformed_documents_are_parse_errors() {
        // An empty document parses as no profiles at all.
        assert_eq!(
            PacingProfiles::from_yaml(""),
            Err(PacingError::MissingDefault("human"))
        );

        for text in [
            "human: [unclosed",
            "human: 5",
            "human:\n  gap_ms: { dist: gamma, k: 2 }",
            "human:\n  gap_ms: { dist: uniform, min: 1, max: 2, extra: 3 }",
        ] {
            assert!(
                matches!(PacingProfiles::from_yaml(text), Err(PacingError::Parse(_))),
                "{text:?}"
            );
        }

        let unknown_field = SPEC_YAML.replace("domain_rpm: 20", "domain_rpm: 20\n  domain_rps: 1");
        assert!(matches!(
            PacingProfiles::from_yaml(&unknown_field),
            Err(PacingError::Parse(message)) if message.contains("domain_rps")
        ));
    }

    #[test]
    fn profile_names_are_validated() {
        for name in ["", "Human", "9lives", "with space", "custom"] {
            let mut map = BTreeMap::new();
            map.insert("human".to_owned(), human());
            map.insert(name.to_owned(), human());
            assert_eq!(
                PacingProfiles::new(map),
                Err(PacingError::InvalidName(name.to_owned())),
                "{name:?}"
            );
        }

        let mut map = BTreeMap::new();
        map.insert("human".to_owned(), human());
        map.insert("night-owl_2".to_owned(), human());
        assert!(PacingProfiles::new(map).is_ok());
    }

    fn rejected_field(mutate: impl FnOnce(&mut PacingProfile)) -> &'static str {
        let mut profile = human();
        mutate(&mut profile);
        match profile.validate("p") {
            Err(PacingError::InvalidField { profile, field, .. }) => {
                assert_eq!(profile, "p");
                field
            }
            other => panic!("expected an invalid field, got {other:?}"),
        }
    }

    #[test]
    fn out_of_range_fields_are_rejected() {
        assert_eq!(
            rejected_field(|p| p.burst_pages = IntRange { min: 0, max: 3 }),
            "burst_pages"
        );
        assert_eq!(
            rejected_field(|p| p.burst_pages = IntRange { min: 5, max: 3 }),
            "burst_pages"
        );
        assert_eq!(
            rejected_field(|p| p.daily_cap_per_persona_per_domain = 0),
            "daily_cap_per_persona_per_domain"
        );
        assert_eq!(
            rejected_field(|p| p.domain_concurrency = 0),
            "domain_concurrency"
        );
        assert_eq!(rejected_field(|p| p.domain_rpm = 0), "domain_rpm");
        assert_eq!(
            rejected_field(|p| p.gap_ms = Distribution::Uniform(Uniform { min: 5.0, max: 1.0 })),
            "gap_ms"
        );
        assert_eq!(
            rejected_field(|p| p.burst_break_ms = Distribution::Uniform(Uniform {
                min: -1.0,
                max: 1.0
            })),
            "burst_break_ms"
        );
    }

    #[test]
    fn distribution_parameters_are_validated() {
        let lognormal = |median, p95, min, max| {
            Distribution::Lognormal(Lognormal {
                median,
                p95,
                min,
                max,
            })
        };

        assert!(
            lognormal(4500.0, 15000.0, Some(1500.0), None)
                .validate()
                .is_ok()
        );
        assert!(
            lognormal(4500.0, 15000.0, None, Some(45000.0))
                .validate()
                .is_ok()
        );
        assert!(lognormal(1000.0, 1000.0, None, None).validate().is_ok());

        assert!(lognormal(0.0, 10.0, None, None).validate().is_err());
        assert!(lognormal(4500.0, 4000.0, None, None).validate().is_err());
        assert!(
            lognormal(4500.0, 15000.0, Some(4500.0), None)
                .validate()
                .is_err()
        );
        assert!(
            lognormal(4500.0, 15000.0, Some(-1.0), None)
                .validate()
                .is_err()
        );
        assert!(
            lognormal(4500.0, 15000.0, None, Some(9000.0))
                .validate()
                .is_err()
        );
        assert!(lognormal(f64::NAN, 15000.0, None, None).validate().is_err());
        assert!(
            lognormal(4500.0, f64::INFINITY, None, None)
                .validate()
                .is_err()
        );
        assert!(
            lognormal(4500.0, 15000.0, None, Some(f64::NAN))
                .validate()
                .is_err()
        );

        assert!(
            Distribution::Uniform(Uniform { min: 0.0, max: 0.0 })
                .validate()
                .is_ok()
        );
        assert!(
            Distribution::Uniform(Uniform { min: 2.0, max: 1.0 })
                .validate()
                .is_err()
        );
        assert!(
            Distribution::Uniform(Uniform {
                min: 0.0,
                max: f64::NAN
            })
            .validate()
            .is_err()
        );
    }

    #[test]
    fn human_gaps_have_the_configured_median_and_p95() {
        let samples = sorted_samples(&human().gap_ms, 11, 200_000);

        let median = percentile(&samples, 0.5);
        let p95 = percentile(&samples, 0.95);

        assert!((median - 4500.0).abs() < 4500.0 * 0.02, "median {median}");
        assert!((p95 - 15000.0).abs() < 15000.0 * 0.03, "p95 {p95}");
    }

    #[test]
    fn human_gaps_never_reach_the_floor_and_do_not_pile_up_near_it() {
        let samples = sorted_samples(&human().gap_ms, 12, 200_000);

        assert!(samples.iter().all(|&gap| gap > 1500.0));
        // Clamping would put 6.7 % of all samples at exactly 1500 ms. The
        // shifted curve leaves the first 50 ms above the floor almost empty.
        let near_floor = samples.iter().filter(|&&gap| gap < 1550.0).count();
        assert!(
            near_floor < 20,
            "{near_floor} samples within 50 ms of the floor"
        );
        // No two samples share a value, so the histogram has no spike.
        assert!(samples.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn an_unbounded_lognormal_has_the_configured_median_and_p95() {
        let samples = sorted_samples(&human().burst_break_ms, 13, 200_000);

        let median = percentile(&samples, 0.5);
        let p95 = percentile(&samples, 0.95);

        assert!(
            (median - 60_000.0).abs() < 60_000.0 * 0.02,
            "median {median}"
        );
        assert!((p95 - 240_000.0).abs() < 240_000.0 * 0.03, "p95 {p95}");
        assert!(samples.iter().all(|&value| value > 0.0));
    }

    #[test]
    fn max_is_respected_without_a_spike_at_the_bound() {
        let capped = Distribution::Lognormal(Lognormal {
            median: 6000.0,
            p95: 20000.0,
            min: None,
            max: Some(45000.0),
        });
        let samples = sorted_samples(&capped, 14, 100_000);

        assert!(samples.iter().all(|&value| value <= 45000.0));
        let at_bound = samples.iter().filter(|&&value| value >= 44_999.5).count();
        assert!(at_bound < 5, "{at_bound} samples at the cap");
    }

    #[test]
    fn a_degenerate_lognormal_is_constant() {
        let constant = Distribution::Lognormal(Lognormal {
            median: 2000.0,
            p95: 2000.0,
            min: None,
            max: None,
        });
        let mut rng = Seed::from_u64(15).rng();

        for _ in 0..100 {
            assert!((constant.sample(&mut rng) - 2000.0).abs() < 1e-6);
        }
    }

    #[test]
    fn uniform_gaps_cover_their_range_evenly() {
        let samples = sorted_samples(&fast().gap_ms, 16, 100_000);

        assert!(samples.iter().all(|&gap| (300.0..1200.0).contains(&gap)));
        let median = percentile(&samples, 0.5);
        assert!((median - 750.0).abs() < 10.0, "median {median}");
        let lowest_tenth = samples.iter().filter(|&&gap| gap < 390.0).count();
        assert!((9_500..10_500).contains(&lowest_tenth), "{lowest_tenth}");
    }

    #[test]
    fn durations_are_whole_milliseconds() {
        let mut rng = Seed::from_u64(17).rng();

        let zero = Distribution::Uniform(Uniform { min: 0.0, max: 0.0 });
        assert_eq!(zero.sample_duration(&mut rng), Duration::ZERO);

        let gap = human().gap_ms.sample_duration(&mut rng);
        assert_eq!(gap.subsec_nanos() % 1_000_000, 0);
        assert!(gap > Duration::from_millis(1500));

        assert_eq!(super::to_millis(-5.0), 0);
        assert_eq!(super::to_millis(f64::NAN), 0);
        assert_eq!(super::to_millis(1499.6), 1500);
        assert_eq!(super::to_millis(f64::INFINITY), u64::MAX);
    }

    #[test]
    fn consecutive_gaps_never_repeat() {
        // Raw samples: no two neighbours are equal.
        let mut rng = Seed::from_u64(22).rng();
        let raw: Vec<f64> = (0..50_000)
            .map(|_| human().gap_ms.sample(&mut rng))
            .collect();
        assert!(
            raw.windows(2)
                .all(|pair| pair[0].to_bits() != pair[1].to_bits())
        );

        // Rounded to milliseconds two neighbours can coincide by chance, but
        // a run of three identical waits would be a pattern.
        let mut sampler = PacingSampler::new(human(), Seed::from_u64(23));
        let waits: Vec<Duration> = (0..50_000).map(|_| sampler.next_gap().wait).collect();
        assert!(
            waits
                .windows(3)
                .all(|run| !(run[0] == run[1] && run[1] == run[2]))
        );
        let distinct: std::collections::BTreeSet<Duration> = waits.iter().copied().collect();
        assert!(distinct.len() > 10_000, "{} distinct waits", distinct.len());
    }

    /// The hook for per-persona variation: the parameters are a value, so a
    /// caller can scale them before sampling, and the result still has the
    /// median and p95 it was given.
    #[test]
    fn scaled_parameters_give_a_scaled_distribution() {
        let Distribution::Lognormal(base) = human().gap_ms else {
            panic!("the human profile uses a log-normal gap");
        };
        for factor in [0.8, 1.0, 1.25] {
            let scaled = Lognormal {
                median: base.median * factor,
                p95: base.p95 * factor,
                ..base
            };
            scaled.validate().unwrap();
            let mut rng = Seed::from_u64(24).rng();
            let mut samples: Vec<f64> = (0..100_000).map(|_| scaled.sample(&mut rng)).collect();
            samples.sort_by(f64::total_cmp);

            let median = percentile(&samples, 0.5);
            let p95 = percentile(&samples, 0.95);
            assert!(
                (median - scaled.median).abs() < scaled.median * 0.03,
                "factor {factor}: median {median}"
            );
            assert!(
                (p95 - scaled.p95).abs() < scaled.p95 * 0.04,
                "factor {factor}: p95 {p95}"
            );
            // The floor is the profile's, whatever the persona's factor.
            assert!(samples[0] > 1500.0);
            // It is what `scaled` gives.
            assert_eq!(base.scaled(factor), scaled);
        }
    }

    #[test]
    fn a_scaled_distribution_stays_inside_the_profiles_limits() {
        // A floor close to the median: only the part above it is scaled,
        // so the result is still a distribution.
        let tight = Lognormal {
            median: 1800.0,
            p95: 4000.0,
            min: Some(1500.0),
            max: Some(4500.0),
        };
        let slower = tight.scaled(0.8);
        slower.validate().unwrap();
        assert!((slower.median - 1740.0).abs() < 1e-9);
        assert!((slower.p95 - 3500.0).abs() < 1e-9);
        // A ceiling stops the p95.
        let faster = tight.scaled(1.25);
        faster.validate().unwrap();
        assert!((faster.median - 2250.0).abs() < 1e-9);
        assert!((faster.p95 - 4500.0).abs() < 1e-9);
        assert_eq!((faster.min, faster.max), (tight.min, tight.max));
        // Not a factor: nothing changes.
        for factor in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(tight.scaled(factor), tight);
        }
    }

    /// Spec §11: twenty personas never share one identical gap
    /// distribution, and each keeps the profile's limits.
    #[test]
    fn a_profile_is_paced_differently_by_every_persona() {
        let profile = human();
        let mut medians = std::collections::BTreeSet::new();
        for index in 0..20 {
            let factor = crate::persona::pacing_factor(&Seed::from_u64(index));
            let own = profile.for_persona(factor);
            own.validate("human").unwrap();
            let (Distribution::Lognormal(gap), Distribution::Lognormal(base)) =
                (own.gap_ms, profile.gap_ms)
            else {
                panic!("the human profile uses a log-normal gap");
            };
            assert!((gap.median / base.median - factor).abs() < 1e-9);
            assert!((gap.p95 / base.p95 - factor).abs() < 1e-9);
            assert_eq!(gap.min, base.min);
            medians.insert(gap.median.to_bits());
            // What belongs to the profile and to the domain does not move.
            assert_eq!(own.burst_pages, profile.burst_pages);
            assert_eq!(
                own.daily_cap_per_persona_per_domain,
                profile.daily_cap_per_persona_per_domain
            );
            assert_eq!(own.domain_rpm, profile.domain_rpm);
            assert_eq!(own.domain_concurrency, profile.domain_concurrency);
        }
        assert_eq!(medians.len(), 20);

        // A uniform gap has no median to scale.
        let fast = fast();
        assert_eq!(fast.for_persona(1.2), fast);
    }

    #[test]
    fn the_same_seed_replays_the_same_waits() {
        let seed = Seed::derive("pacing", &[b"persona-1", b"example.org"]);
        let mut first = PacingSampler::new(human(), seed);
        let mut second = PacingSampler::new(human(), seed);

        let a: Vec<Gap> = (0..500).map(|_| first.next_gap()).collect();
        let b: Vec<Gap> = (0..500).map(|_| second.next_gap()).collect();
        assert_eq!(a, b);

        let mut other = PacingSampler::new(
            human(),
            Seed::derive("pacing", &[b"persona-2", b"example.org"]),
        );
        let c: Vec<Gap> = (0..500).map(|_| other.next_gap()).collect();
        assert_ne!(a, c);
    }

    #[test]
    fn bursts_have_between_min_and_max_pages_and_end_with_a_break() {
        let mut sampler = PacingSampler::new(human(), Seed::from_u64(18));
        assert_eq!(sampler.profile(), &human());
        assert!((4..=14).contains(&sampler.remaining_in_burst()));

        let mut burst_lengths = Vec::new();
        let mut pages_in_burst = 0_u32;
        for _ in 0..20_000 {
            let gap = sampler.next_gap();
            pages_in_burst += 1;
            // The floor of gap_ms holds for every wait, breaks included.
            assert!(gap.wait > Duration::from_millis(1500));
            if gap.after_burst {
                burst_lengths.push(pages_in_burst);
                pages_in_burst = 0;
            }
        }

        assert!(
            burst_lengths.len() > 1_500,
            "{} bursts",
            burst_lengths.len()
        );
        assert!(burst_lengths.iter().all(|pages| (4..=14).contains(pages)));
        for pages in 4..=14 {
            assert!(burst_lengths.contains(&pages), "no burst of {pages} pages");
        }
    }

    #[test]
    fn breaks_are_longer_than_ordinary_gaps() {
        let mut sampler = PacingSampler::new(human(), Seed::from_u64(19));
        let (mut inside, mut breaks) = (Vec::new(), Vec::new());

        for _ in 0..20_000 {
            let gap = sampler.next_gap();
            if gap.after_burst {
                breaks.push(gap.wait);
            } else {
                inside.push(gap.wait);
            }
        }
        inside.sort();
        breaks.sort();

        let inside_median = inside[inside.len() / 2];
        let break_median = breaks[breaks.len() / 2];
        assert!(inside_median < Duration::from_secs(6), "{inside_median:?}");
        assert!(break_median > Duration::from_secs(45), "{break_median:?}");
    }

    #[test]
    fn a_zero_break_leaves_the_fast_profile_at_its_plain_gap() {
        let mut sampler = PacingSampler::new(fast(), Seed::from_u64(20));

        let gaps: Vec<Gap> = (0..200).map(|_| sampler.next_gap()).collect();

        assert!(gaps.iter().all(|gap| {
            (Duration::from_millis(300)..=Duration::from_millis(1200)).contains(&gap.wait)
        }));
        let burst_ends: Vec<usize> = gaps
            .iter()
            .enumerate()
            .filter(|(_, gap)| gap.after_burst)
            .map(|(index, _)| index + 1)
            .collect();
        assert_eq!(burst_ends, [50, 100, 150, 200]);
    }

    #[test]
    fn single_page_bursts_break_after_every_page() {
        let mut profile = human();
        profile.burst_pages = IntRange { min: 1, max: 1 };
        let mut sampler = PacingSampler::new(profile, Seed::from_u64(21));

        assert!((0..50).all(|_| sampler.next_gap().after_burst));
    }

    fn with_visits() -> PacingProfile {
        let mut profile = human();
        profile.visit_bursts = Some(IntRange { min: 2, max: 5 });
        profile.visit_break_ms = Some(Distribution::Lognormal(Lognormal {
            median: 1_500_000.0,
            p95: 5_400_000.0,
            min: None,
            max: None,
        }));
        profile
    }

    #[test]
    fn a_profile_without_visits_never_ends_one_and_keeps_its_sequence() {
        let mut sampler = PacingSampler::new(human(), Seed::from_u64(22));
        assert_eq!(sampler.remaining_in_visit(), None);

        let gaps: Vec<Gap> = (0..2_000).map(|_| sampler.next_gap()).collect();

        assert!(gaps.iter().all(|gap| !gap.after_visit));
        assert_eq!(sampler.remaining_in_visit(), None);
        // A profile that does not ask for visits draws nothing for them: its
        // stream is one burst length, then the first gap, as it was before
        // visits existed.
        let mut rng = Seed::from_u64(22).rng();
        let burst = rng.int_inclusive(4, 14);
        let first_gap = human().gap_ms.sample_duration(&mut rng);
        let plain = PacingSampler::new(human(), Seed::from_u64(22));
        assert_eq!(u64::from(plain.remaining_in_burst()), burst);
        assert_eq!(gaps[0].wait, first_gap);
        // With visits one more number is drawn before the first gap.
        let mut visiting = PacingSampler::new(with_visits(), Seed::from_u64(22));
        assert_eq!(u64::from(visiting.remaining_in_burst()), burst);
        assert_ne!(visiting.next_gap().wait, first_gap);
    }

    #[test]
    fn visits_have_between_min_and_max_bursts_and_end_with_the_long_break() {
        let mut sampler = PacingSampler::new(with_visits(), Seed::from_u64(23));
        let first_visit = sampler.remaining_in_visit().unwrap();
        assert!((2..=5).contains(&first_visit));

        let mut bursts_in_visit = 0_u32;
        let mut visit_lengths = Vec::new();
        let mut burst_breaks = Vec::new();
        let mut visit_breaks = Vec::new();
        for _ in 0..20_000 {
            let gap = sampler.next_gap();
            // A wait that ends a visit also ends a burst, never the reverse.
            assert!(!gap.after_visit || gap.after_burst);
            if gap.after_visit {
                visit_lengths.push(bursts_in_visit + 1);
                bursts_in_visit = 0;
                visit_breaks.push(gap.wait);
            } else if gap.after_burst {
                bursts_in_visit += 1;
                burst_breaks.push(gap.wait);
            }
        }

        assert_eq!(visit_lengths[0], first_visit);
        assert!(visit_lengths.iter().all(|bursts| (2..=5).contains(bursts)));
        for length in 2..=5 {
            assert!(
                visit_lengths.contains(&length),
                "no visit of {length} bursts"
            );
        }
        burst_breaks.sort();
        visit_breaks.sort();
        let burst_median = burst_breaks[burst_breaks.len() / 2];
        let visit_median = visit_breaks[visit_breaks.len() / 2];
        // The burst break is the profile's (about a minute), the break after
        // a visit is the long one (about 25 minutes) and replaces it.
        assert!(
            (Duration::from_secs(45)..Duration::from_secs(90)).contains(&burst_median),
            "{burst_median:?}"
        );
        assert!(
            (Duration::from_mins(20)..Duration::from_mins(31)).contains(&visit_median),
            "{visit_median:?}"
        );
        // Never shorter than an ordinary gap.
        assert!(visit_breaks[0] > Duration::from_millis(1500));
    }

    #[test]
    fn a_visit_of_one_burst_ends_after_every_burst() {
        let mut profile = with_visits();
        profile.visit_bursts = Some(IntRange { min: 1, max: 1 });
        profile.burst_pages = IntRange { min: 3, max: 3 };
        let mut sampler = PacingSampler::new(profile, Seed::from_u64(24));

        let ends: Vec<(bool, bool)> = (0..9)
            .map(|_| {
                let gap = sampler.next_gap();
                (gap.after_burst, gap.after_visit)
            })
            .collect();

        let inside = (false, false);
        let over = (true, true);
        assert_eq!(
            ends,
            [
                inside, inside, over, inside, inside, over, inside, inside, over
            ]
        );
        assert_eq!(sampler.remaining_in_visit(), Some(1));
        assert_eq!(sampler.remaining_in_burst(), 3);
    }

    #[test]
    fn the_visit_fields_come_together_or_not_at_all() {
        let mut only_bursts = with_visits();
        only_bursts.visit_break_ms = None;
        assert_eq!(
            only_bursts.validate("p"),
            Err(PacingError::InvalidField {
                profile: "p".to_owned(),
                field: "visit_break_ms",
                reason: "is required when visit_bursts is set".to_owned(),
            })
        );
        let mut only_break = with_visits();
        only_break.visit_bursts = None;
        assert_eq!(
            only_break.validate("p"),
            Err(PacingError::InvalidField {
                profile: "p".to_owned(),
                field: "visit_bursts",
                reason: "is required when visit_break_ms is set".to_owned(),
            })
        );

        let mut none = with_visits();
        none.visit_bursts = Some(IntRange { min: 0, max: 2 });
        assert!(matches!(
            none.validate("p"),
            Err(PacingError::InvalidField { field: "visit_bursts", reason, .. })
                if reason == "min must be at least 1"
        ));
        let mut backwards = with_visits();
        backwards.visit_bursts = Some(IntRange { min: 4, max: 2 });
        assert!(matches!(
            backwards.validate("p"),
            Err(PacingError::InvalidField { field: "visit_bursts", reason, .. })
                if reason == "min 4 is greater than max 2"
        ));
        let mut broken = with_visits();
        broken.visit_break_ms = Some(Distribution::Uniform(Uniform { min: 9.0, max: 1.0 }));
        assert!(matches!(
            broken.validate("p"),
            Err(PacingError::InvalidField {
                field: "visit_break_ms",
                ..
            })
        ));
        assert_eq!(with_visits().validate("p"), Ok(()));
        // The smallest visit there is.
        let mut single = with_visits();
        single.visit_bursts = Some(IntRange { min: 1, max: 1 });
        assert_eq!(single.validate("p"), Ok(()));
    }

    #[test]
    fn a_persona_stretches_the_break_after_a_visit_too() {
        let slow = with_visits().for_persona(1.25);

        assert_eq!(
            slow.visit_break_ms,
            Some(Distribution::Lognormal(Lognormal {
                median: 1_875_000.0,
                p95: 6_750_000.0,
                min: None,
                max: None,
            }))
        );
        assert_eq!(slow.visit_bursts, Some(IntRange { min: 2, max: 5 }));
        assert_eq!(human().for_persona(1.25).visit_break_ms, None);
    }

    #[test]
    fn the_visit_fields_parse_from_a_profile() {
        let text = format!(
            "{SPEC_YAML}calm:\n  gap_ms: {{ dist: uniform, min: 1, max: 2 }}\n  burst_pages: {{ min: 1, max: 1 }}\n  burst_break_ms: {{ dist: uniform, min: 0, max: 0 }}\n  daily_cap_per_persona_per_domain: 1\n  domain_concurrency: 1\n  domain_rpm: 1\n  visit_bursts: {{ min: 2, max: 5 }}\n  visit_break_ms: {{ dist: lognormal, median: 1500000, p95: 5400000 }}\n"
        );
        let profiles = PacingProfiles::from_yaml(&text).unwrap();

        let calm = profiles.require("calm").unwrap();
        assert_eq!(calm.visit_bursts, Some(IntRange { min: 2, max: 5 }));
        assert!(calm.visit_break_ms.is_some());
        assert_eq!(profiles.require("human").unwrap().visit_bursts, None);
        // A profile without visits serializes without the keys.
        let written = serde_json::to_value(human()).unwrap();
        assert!(written.get("visit_bursts").is_none());
        assert!(written.get("visit_break_ms").is_none());
    }
}
