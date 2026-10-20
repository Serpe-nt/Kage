//! Per-domain policy: the model, its file format and its resolution order
//! (spec §8.1, §11, §17).
//!
//! `policies/domains.yaml` holds a `defaults` block and a map of domains.
//! A host inherits, field by field, along a chain:
//!
//! 1. the entry for the exact host;
//! 2. the entries for its parent domains, nearest first (`a.b.example.org`
//!    looks at `b.example.org`, then `example.org`, then `org`);
//! 3. the `defaults` block;
//! 4. the built-in defaults.
//!
//! Each field takes the value of the first link in the chain that sets it.
//! An entry for `shop.example.org` therefore only needs to say what differs
//! from `example.org`, and a lock, a country list or a slower pacing set on
//! a domain covers every subdomain that does not override it. Cooldowns
//! cascade per outcome in the same way.

mod store;

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use store::{
    DOMAINS_FILE, PACING_FILE, PolicyLoadError, PolicySet, PolicyStore, PolicyWatcher,
    SIGNATURES_FILE, WatchError,
};

use crate::domain::{CountryCode, Engine, Level};
use crate::duration::HumanDuration;
use crate::macros::string_enum;
use crate::outcome::{CooldownClass, Outcome};
use crate::pacing::{PacingError, PacingProfile, PacingProfiles};

/// The shortest time a persona stays on a page of a browser render, unless
/// a domain says otherwise: long enough for a page's own scripts to run and
/// report, and for the page not to look like the http path in a browser.
pub const DEFAULT_MIN_DWELL: Duration = Duration::from_secs(2);

/// The bounds a `min_dwell` must lie in. Below the first a page is as good
/// as a bare load; above the second it is longer than a person's dwell is
/// ever drawn.
pub const MIN_DWELL_BOUNDS: (Duration, Duration) =
    (Duration::from_millis(500), Duration::from_secs(45));

/// Most retries a policy may ask for. Each retry spends another persona.
pub const MAX_RETRIES: u32 = 10;

string_enum! {
    /// What to do with robots.txt (spec §14, decision D15). A reader never
    /// asks for `/robots.txt`, so the request marks the address that makes
    /// it: a site's file is asked for only where its policy says so, and
    /// then by a persona that never reads the site.
    pub enum RobotsMode {
        /// Ask for it and report the verdict; never refuse.
        Report => "report",
        /// Ask for it, refuse disallowed URLs with `robots_denied`, and
        /// keep to its `Crawl-delay`.
        Enforce => "enforce",
        /// Do not ask for it: the default.
        Ignore => "ignore",
    }
}

/// What a domain entry says about the response cache (spec §15). Every
/// field is optional and cascades by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheRule {
    /// When true, a page one API key fetched may be served to another:
    /// the operator says the site shows everybody the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<bool>,
    /// The longest a result of this site may be served for; the
    /// configuration's `cache.max_ttl` when not set, and never beyond it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ttl: Option<HumanDuration>,
    /// How long a result is served for when a request does not say
    /// (`cache_ttl_s`). Not set: a request that does not ask is not served
    /// from the cache and fills none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<HumanDuration>,
}

/// The response cache as a domain's policy resolves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct CachePolicy {
    /// Whether a page one API key fetched may be served to another.
    pub shared: bool,
    /// The longest a result may be served for; `None` leaves it to the
    /// configuration's `cache.max_ttl`.
    pub max_ttl: Option<HumanDuration>,
    /// How long a result is served for when a request does not say.
    pub ttl: Option<HumanDuration>,
}

impl CachePolicy {
    /// The longest a result of the site may be served for, given the
    /// configuration's bound.
    pub fn longest(&self, bound: Duration) -> Duration {
        self.max_ttl.map_or(bound, |own| own.get().min(bound))
    }

    /// For how long a request is served from the cache and fills it:
    /// what it asks for (`Some(0)` is never), else the policy's `ttl`,
    /// else never. `Err` with the bound when it asks for longer than the
    /// site's results may be served.
    ///
    /// # Errors
    ///
    /// The longest time allowed, when the request asks for more.
    pub fn ttl_for(
        &self,
        asked: Option<Duration>,
        bound: Duration,
    ) -> Result<Option<Duration>, Duration> {
        let longest = self.longest(bound);
        match asked {
            Some(asked) if asked > longest => Err(longest),
            Some(asked) => Ok(Some(asked).filter(|asked| !asked.is_zero())),
            None => Ok(self.ttl.map(|own| own.get().min(longest))),
        }
    }
}

string_enum! {
    /// How the first navigation to a new origin arrives (spec §10.4).
    pub enum RefererStrategy {
        /// Like a typed URL or a bookmark: the first navigation carries no
        /// `Referer`, `Sec-Fetch-Site: none` and `Sec-Fetch-User: ?1`. Later
        /// navigations inside a session carry the previous same-site page as
        /// `Referer`.
        Direct => "direct",
        /// Load a search results page first and click through to the target.
        SearchEngine => "search_engine",
        /// Apply no entry strategy; send whatever `Referer` the caller gave.
        None => "none",
    }
}

string_enum! {
    /// What a persona's own day means for a domain (spec §11). Every
    /// persona is awake for a stretch of the day drawn from its seed.
    pub enum ActiveHoursMode {
        /// Only personas that are awake serve the domain. At night nothing
        /// is fetched from it.
        Enforce => "enforce",
        /// A persona that is awake is always chosen before one that sleeps.
        /// Job work never wakes a sleeper; a synchronous request may, for
        /// the one page it asks for.
        Prefer => "prefer",
        /// The hour of the day is not looked at.
        Ignore => "ignore",
    }
}

/// A problem in `policies/domains.yaml`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The YAML document could not be parsed.
    #[error("domain policies: {0}")]
    Parse(String),
    /// A key under `domains` is not a host name.
    #[error("domain policies: {domain:?} is not a valid domain: {reason}")]
    InvalidDomain {
        /// The key as written.
        domain: String,
        /// What is wrong with it.
        reason: String,
    },
    /// Two keys under `domains` are the same host once normalized.
    #[error("domain policies: {0:?} is listed more than once")]
    DuplicateDomain(String),
    /// A field of `defaults` or of a domain entry is out of range.
    #[error("domain policies: {scope}: {field}: {reason}")]
    InvalidField {
        /// `defaults` or the domain.
        scope: String,
        /// The field.
        field: &'static str,
        /// What is wrong with it.
        reason: String,
    },
    /// An entry names a pacing profile that is not defined.
    #[error("domain policies: {scope}: pacing profile {profile:?} is not defined")]
    UnknownPacing {
        /// `defaults` or the domain.
        scope: String,
        /// The missing profile.
        profile: String,
    },
}

/// Per-outcome cooldown overrides as written in a policy entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CooldownOverrides {
    /// After `rate_limited`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limited: Option<HumanDuration>,
    /// After `forbidden`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forbidden: Option<HumanDuration>,
    /// After `captcha`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captcha: Option<HumanDuration>,
    /// After `challenge_failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub challenge_failed: Option<HumanDuration>,
    /// After `soft_block`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_block: Option<HumanDuration>,
    /// For blocks whose signature carries the cooldown class `short`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<HumanDuration>,
}

/// How long a persona stays away from a domain after each kind of block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Cooldowns {
    /// After `rate_limited`.
    pub rate_limited: HumanDuration,
    /// After `forbidden`.
    pub forbidden: HumanDuration,
    /// After `captcha`.
    pub captcha: HumanDuration,
    /// After `challenge_failed`.
    pub challenge_failed: HumanDuration,
    /// After `soft_block`.
    pub soft_block: HumanDuration,
    /// For blocks whose signature carries the cooldown class `short`: the
    /// block is real but weakly attributed, such as a bare 403 or 429 with
    /// no vendor marker.
    pub short: HumanDuration,
}

impl Default for Cooldowns {
    /// Spec §6.3: `rate_limited` 2 h; `forbidden`, `captcha` and
    /// `challenge_failed` 24 h; `soft_block` 6 h. The short cooldown, which
    /// the spec does not define, is 30 min.
    fn default() -> Self {
        Self {
            rate_limited: HumanDuration::from_hours(2),
            forbidden: HumanDuration::from_hours(24),
            captcha: HumanDuration::from_hours(24),
            challenge_failed: HumanDuration::from_hours(24),
            soft_block: HumanDuration::from_hours(6),
            short: HumanDuration::from_mins(30),
        }
    }
}

impl Cooldowns {
    #[must_use]
    fn with(self, overrides: Option<&CooldownOverrides>) -> Self {
        let Some(overrides) = overrides else {
            return self;
        };
        Self {
            rate_limited: overrides.rate_limited.unwrap_or(self.rate_limited),
            forbidden: overrides.forbidden.unwrap_or(self.forbidden),
            captcha: overrides.captcha.unwrap_or(self.captcha),
            challenge_failed: overrides.challenge_failed.unwrap_or(self.challenge_failed),
            soft_block: overrides.soft_block.unwrap_or(self.soft_block),
            short: overrides.short.unwrap_or(self.short),
        }
    }

    /// The cooldown for a classified attempt: the class its signature
    /// carries, turned into a duration.
    ///
    /// `long` is the cooldown for the outcome ([`Cooldowns::after`]);
    /// `short` is the short cooldown, never longer than the outcome's own;
    /// `none` is no cooldown. The scheduler (milestone M7) may scale the
    /// result by the signature's confidence.
    pub fn resolve(&self, outcome: Outcome, class: CooldownClass) -> Option<Duration> {
        let for_outcome = self.after(outcome);
        match class {
            CooldownClass::None => None,
            CooldownClass::Long => for_outcome,
            CooldownClass::Short => {
                let short = self.short.get();
                Some(for_outcome.map_or(short, |long| long.min(short)))
            }
        }
    }

    /// The cooldown after `outcome`; `None` when the outcome is not a block.
    pub fn after(&self, outcome: Outcome) -> Option<Duration> {
        let cooldown = match outcome {
            Outcome::RateLimited => self.rate_limited,
            Outcome::Forbidden => self.forbidden,
            Outcome::Captcha => self.captcha,
            Outcome::ChallengeFailed => self.challenge_failed,
            Outcome::SoftBlock => self.soft_block,
            Outcome::Ok
            | Outcome::ChallengePassed
            | Outcome::ProviderError
            | Outcome::Timeout
            | Outcome::SsrfDenied
            | Outcome::RobotsDenied => return None,
        };
        Some(cooldown.get())
    }
}

/// A field of 1.x that a policy file may no longer hold: it is read only
/// to be refused with a message that says what took its place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Retired;

impl<'de> Deserialize<'de> for Retired {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Self)
    }
}

/// One entry of `domains.yaml` as written: the `defaults` block or a domain.
/// Every field is optional.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomainRule {
    /// The render mode of 1.x. Gone in 2.0: the request's level decides the
    /// path (spec §25); a file that still names it is refused, pointing at
    /// `min_level`.
    #[serde(default, skip_serializing)]
    pub render: Option<Retired>,
    /// The lowest level a request for the site may name (spec §25); a
    /// request below it is refused (`level_below_policy`), never raised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_level: Option<Level>,
    /// Only personas from these countries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub countries: Option<Vec<CountryCode>>,
    /// Only personas on these engines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines: Option<Vec<Engine>>,
    /// Name of the pacing profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pacing: Option<String>,
    /// When true, a request cannot choose its own pacing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pacing_locked: Option<bool>,
    /// robots.txt handling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub robots: Option<RobotsMode>,
    /// The response cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheRule>,
    /// How the first navigation arrives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referer_strategy: Option<RefererStrategy>,
    /// What the personas' own days mean for this domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_hours: Option<ActiveHoursMode>,
    /// The shortest time a persona stays on a page of a browser render,
    /// whatever a request asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_dwell: Option<HumanDuration>,
    /// When true, a request cannot choose its own behavior on a page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior_locked: Option<bool>,
    /// When true, a flow may ask for steps done by the browser's driver
    /// (`human: false`), which a page can tell from a person's input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_input: Option<bool>,
    /// Retries after the first attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    /// Blocks per hour, over all personas, that pause the domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breaker_threshold: Option<u32>,
    /// How long the domain stays paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breaker_pause: Option<HumanDuration>,
    /// Cooldowns that differ from the defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldowns: Option<CooldownOverrides>,
}

impl DomainRule {
    fn validate(&self, scope: &str) -> Result<(), PolicyError> {
        let invalid = |field: &'static str, reason: &str| PolicyError::InvalidField {
            scope: scope.to_owned(),
            field,
            reason: reason.to_owned(),
        };
        if self.render.is_some() {
            return Err(invalid(
                "render",
                "is gone in 2.0: every request names its level, which decides the path \
                 (spec §25); set min_level for the lowest level the site accepts",
            ));
        }
        if self.countries.as_ref().is_some_and(Vec::is_empty) {
            return Err(invalid(
                "countries",
                "must not be empty; omit it to allow any country",
            ));
        }
        if let Some(engines) = &self.engines {
            if engines.is_empty() {
                return Err(invalid("engines", "must name at least one engine"));
            }
            if has_duplicates(engines) {
                return Err(invalid("engines", "lists an engine twice"));
            }
        }
        if self.retries.is_some_and(|retries| retries > MAX_RETRIES) {
            return Err(invalid("retries", "must be at most 10"));
        }
        if self.breaker_threshold == Some(0) {
            return Err(invalid("breaker_threshold", "must be at least 1"));
        }
        if self
            .breaker_pause
            .is_some_and(|pause| pause.get().is_zero())
        {
            return Err(invalid("breaker_pause", "must be longer than zero"));
        }
        if self.pacing.as_deref() == Some("") {
            return Err(invalid("pacing", "must name a profile"));
        }
        if let Some(cache) = &self.cache {
            if cache.max_ttl.is_some_and(|ttl| ttl.get().is_zero()) {
                return Err(invalid("cache.max_ttl", "must be longer than zero"));
            }
            if cache.ttl.is_some_and(|ttl| ttl.get().is_zero()) {
                return Err(invalid(
                    "cache.ttl",
                    "must be longer than zero; omit it for no default",
                ));
            }
            if let (Some(ttl), Some(max_ttl)) = (cache.ttl, cache.max_ttl)
                && ttl.get() > max_ttl.get()
            {
                return Err(invalid(
                    "cache.ttl",
                    "must not be longer than cache.max_ttl",
                ));
            }
        }
        let (shortest, longest) = MIN_DWELL_BOUNDS;
        if self
            .min_dwell
            .is_some_and(|dwell| dwell.get() < shortest || dwell.get() > longest)
        {
            return Err(invalid("min_dwell", "must be between 500ms and 45s"));
        }
        Ok(())
    }
}

fn has_duplicates<T: Ord + Copy>(values: &[T]) -> bool {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.windows(2).any(|pair| pair[0] == pair[1])
}

/// The nearest entry that applies to a host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "domain", rename_all = "snake_case")]
pub enum PolicyMatch {
    /// The entry for the host itself.
    Exact(String),
    /// The entry for this parent domain.
    Parent(String),
    /// No entry; `defaults` only.
    Defaults,
}

/// Where a pacing profile came from (spec §11 resolution order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PacingSource {
    /// The request asked for it.
    Request,
    /// An entry for the host or for one of its parent domains set it.
    Domain,
    /// `defaults`, or the built-in default `human`.
    Default,
}

/// The pacing a request may ask for. The caller may fully control pacing
/// unless the domain policy locks it (decision D9).
#[derive(Debug, Clone, PartialEq)]
pub enum PacingRequest {
    /// A named profile.
    Profile(String),
    /// A profile supplied inline with the request.
    Custom(PacingProfile),
}

/// The pacing that applies to one request.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PacingDecision {
    /// Name reported as `pacing.applied`; `custom` for an inline profile.
    pub applied: String,
    /// The profile itself.
    pub profile: PacingProfile,
    /// Where it came from.
    pub source: PacingSource,
    /// Whether the domain policy locks pacing.
    pub locked: bool,
    /// Whether the request asked for pacing that the lock overrode.
    pub request_ignored: bool,
}

/// The policy in force for one host, with every field resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectivePolicy {
    /// The host that was resolved, normalized.
    pub host: String,
    /// The nearest entry that applies to the host.
    pub matched: PolicyMatch,
    /// Every domain entry that applies, nearest first. Each field comes from
    /// the first of these that sets it, then from `defaults`.
    pub inherits: Vec<String>,
    /// The lowest level a request for the site may name (spec §25); 0
    /// unless set.
    pub min_level: Level,
    /// Allowed persona countries; `None` allows any.
    pub countries: Option<Vec<CountryCode>>,
    /// Allowed engines.
    pub engines: Vec<Engine>,
    /// Name of the pacing profile.
    pub pacing: String,
    /// Whether `pacing` was set by a domain entry or came from the defaults.
    pub pacing_source: PacingSource,
    /// Whether requests may not choose their own pacing.
    pub pacing_locked: bool,
    /// robots.txt handling.
    pub robots: RobotsMode,
    /// The response cache.
    pub cache: CachePolicy,
    /// How the first navigation arrives.
    pub referer_strategy: RefererStrategy,
    /// What the personas' own days mean for this domain.
    pub active_hours: ActiveHoursMode,
    /// The shortest time a persona stays on a page of a browser render.
    pub min_dwell: HumanDuration,
    /// Whether requests may not choose their own behavior on a page.
    pub behavior_locked: bool,
    /// Whether a flow may ask for steps done by the browser's driver
    /// instead of the persona's mouse and keyboard. Off unless the
    /// operator says that the site does not look.
    pub driver_input: bool,
    /// Retries after the first attempt.
    pub retries: u32,
    /// Blocks per hour that pause the domain.
    pub breaker_threshold: u32,
    /// How long the domain stays paused.
    pub breaker_pause: HumanDuration,
    /// Cooldown after each kind of block.
    pub cooldowns: Cooldowns,
}

impl EffectivePolicy {
    /// Whether a request of this level may be made of the site: at or
    /// above the policy's floor (spec §25). A request below it is refused,
    /// never raised.
    pub fn admits(&self, level: Level) -> bool {
        level >= self.min_level
    }

    /// Whether personas on `engine` may serve this domain.
    pub fn allows_engine(&self, engine: Engine) -> bool {
        self.engines.contains(&engine)
    }

    /// Whether a persona whose proxy is in `country` may serve this domain.
    /// A persona of unknown country is only allowed when the policy names no
    /// countries.
    pub fn allows_country(&self, country: Option<CountryCode>) -> bool {
        match &self.countries {
            None => true,
            Some(allowed) => country.is_some_and(|country| allowed.contains(&country)),
        }
    }

    /// Resolves pacing for a request: request, then domain policy, then the
    /// global default (spec §11). With `pacing_locked` the request's choice
    /// is ignored and reported as such.
    ///
    /// # Errors
    ///
    /// Fails when an unlocked request names an unknown profile or supplies an
    /// invalid inline profile, or when the policy's own profile is missing
    /// from `profiles`.
    pub fn pacing_for(
        &self,
        request: Option<&PacingRequest>,
        profiles: &PacingProfiles,
    ) -> Result<PacingDecision, PacingError> {
        if !self.pacing_locked {
            match request {
                Some(PacingRequest::Profile(name)) => {
                    return Ok(PacingDecision {
                        applied: name.clone(),
                        profile: *profiles.require(name)?,
                        source: PacingSource::Request,
                        locked: false,
                        request_ignored: false,
                    });
                }
                Some(PacingRequest::Custom(profile)) => {
                    profile.validate(PacingProfiles::CUSTOM)?;
                    return Ok(PacingDecision {
                        applied: PacingProfiles::CUSTOM.to_owned(),
                        profile: *profile,
                        source: PacingSource::Request,
                        locked: false,
                        request_ignored: false,
                    });
                }
                None => {}
            }
        }
        Ok(PacingDecision {
            applied: self.pacing.clone(),
            profile: *profiles.require(&self.pacing)?,
            source: self.pacing_source,
            locked: self.pacing_locked,
            request_ignored: self.pacing_locked && request.is_some(),
        })
    }
}

/// The contents of `policies/domains.yaml`, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainPolicies {
    defaults: DomainRule,
    domains: BTreeMap<String, DomainRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DomainsFile {
    #[serde(default)]
    defaults: DomainRule,
    #[serde(default)]
    domains: BTreeMap<String, DomainRule>,
}

impl Default for DomainPolicies {
    /// No entries: every host gets the built-in defaults.
    fn default() -> Self {
        Self {
            defaults: DomainRule::default(),
            domains: BTreeMap::new(),
        }
    }
}

impl DomainPolicies {
    /// Validates the entries and normalizes the domain keys.
    ///
    /// # Errors
    ///
    /// Fails on an invalid or repeated domain key or an out-of-range field.
    pub fn new(
        defaults: DomainRule,
        domains: BTreeMap<String, DomainRule>,
    ) -> Result<Self, PolicyError> {
        defaults.validate("defaults")?;
        let mut normalized = BTreeMap::new();
        for (key, rule) in domains {
            let host = normalize_domain_key(&key)?;
            rule.validate(&host)?;
            if normalized.insert(host.clone(), rule).is_some() {
                return Err(PolicyError::DuplicateDomain(host));
            }
        }
        Ok(Self {
            defaults,
            domains: normalized,
        })
    }

    /// Parses and validates the YAML document.
    ///
    /// # Errors
    ///
    /// Fails when the document does not parse or does not validate.
    pub fn from_yaml(text: &str) -> Result<Self, PolicyError> {
        let file: DomainsFile = crate::yaml::from_str(text).map_err(PolicyError::Parse)?;
        Self::new(file.defaults, file.domains)
    }

    /// Checks that every pacing profile named here exists in `profiles`.
    ///
    /// # Errors
    ///
    /// Fails with [`PolicyError::UnknownPacing`] for the first missing one.
    pub fn check_pacing(&self, profiles: &PacingProfiles) -> Result<(), PolicyError> {
        let entries = std::iter::once(("defaults", &self.defaults)).chain(
            self.domains
                .iter()
                .map(|(host, rule)| (host.as_str(), rule)),
        );
        for (scope, rule) in entries {
            if let Some(profile) = &rule.pacing
                && profiles.get(profile).is_none()
            {
                return Err(PolicyError::UnknownPacing {
                    scope: scope.to_owned(),
                    profile: profile.clone(),
                });
            }
        }
        Ok(())
    }

    /// The `defaults` block as written.
    pub const fn defaults(&self) -> &DomainRule {
        &self.defaults
    }

    /// Every domain entry as written, by normalized host.
    pub const fn domains(&self) -> &BTreeMap<String, DomainRule> {
        &self.domains
    }

    /// Resolves the policy for a host: each field comes from the first entry
    /// in the host's chain that sets it (see the module documentation). A
    /// host that is not a valid host name gets the defaults.
    pub fn resolve(&self, host: &str) -> EffectivePolicy {
        let host = normalize_host(host).unwrap_or_else(|| host.trim().to_ascii_lowercase());
        let chain = self.chain(&host);
        let matched = match chain.first() {
            Some((domain, _)) if *domain == host => PolicyMatch::Exact((*domain).to_owned()),
            Some((domain, _)) => PolicyMatch::Parent((*domain).to_owned()),
            None => PolicyMatch::Defaults,
        };
        let inherits: Vec<String> = chain
            .iter()
            .map(|(domain, _)| (*domain).to_owned())
            .collect();

        let rules: Vec<&DomainRule> = chain.iter().map(|(_, rule)| *rule).collect();
        let defaults = &self.defaults;
        let pacing_source = if rules.iter().any(|rule| rule.pacing.is_some()) {
            PacingSource::Domain
        } else {
            PacingSource::Default
        };
        // Cooldowns cascade per outcome: the nearest entry that names an
        // outcome decides it, so layers are applied from the farthest in.
        let cooldowns = rules.iter().rev().fold(
            Cooldowns::default().with(defaults.cooldowns.as_ref()),
            |cooldowns, rule| cooldowns.with(rule.cooldowns.as_ref()),
        );

        EffectivePolicy {
            min_level: inherited(&rules, defaults, |rule| rule.min_level).unwrap_or(Level::Plain),
            countries: inherited(&rules, defaults, |rule| rule.countries.clone()),
            engines: inherited(&rules, defaults, |rule| rule.engines.clone())
                .unwrap_or_else(|| Engine::ALL.to_vec()),
            pacing: inherited(&rules, defaults, |rule| rule.pacing.clone())
                .unwrap_or_else(|| PacingProfiles::DEFAULT.to_owned()),
            pacing_source,
            pacing_locked: inherited(&rules, defaults, |rule| rule.pacing_locked).unwrap_or(false),
            robots: inherited(&rules, defaults, |rule| rule.robots).unwrap_or(RobotsMode::Ignore),
            cache: CachePolicy {
                shared: inherited(&rules, defaults, |rule| {
                    rule.cache.and_then(|cache| cache.shared)
                })
                .unwrap_or(false),
                max_ttl: inherited(&rules, defaults, |rule| {
                    rule.cache.and_then(|cache| cache.max_ttl)
                }),
                ttl: inherited(&rules, defaults, |rule| {
                    rule.cache.and_then(|cache| cache.ttl)
                }),
            },
            referer_strategy: inherited(&rules, defaults, |rule| rule.referer_strategy)
                .unwrap_or(RefererStrategy::Direct),
            active_hours: inherited(&rules, defaults, |rule| rule.active_hours)
                .unwrap_or(ActiveHoursMode::Prefer),
            min_dwell: inherited(&rules, defaults, |rule| rule.min_dwell)
                .unwrap_or(HumanDuration::new(DEFAULT_MIN_DWELL)),
            behavior_locked: inherited(&rules, defaults, |rule| rule.behavior_locked)
                .unwrap_or(false),
            driver_input: inherited(&rules, defaults, |rule| rule.driver_input).unwrap_or(false),
            retries: inherited(&rules, defaults, |rule| rule.retries).unwrap_or(2),
            breaker_threshold: inherited(&rules, defaults, |rule| rule.breaker_threshold)
                .unwrap_or(5),
            breaker_pause: inherited(&rules, defaults, |rule| rule.breaker_pause)
                .unwrap_or(HumanDuration::from_mins(30)),
            cooldowns,
            inherits,
            host,
            matched,
        }
    }

    /// Every domain entry that applies to a host, nearest first: the entry
    /// for the host itself, then the entries for its parent domains.
    fn chain<'a>(&'a self, host: &'a str) -> Vec<(&'a str, &'a DomainRule)> {
        let mut chain = Vec::new();
        if let Some((domain, rule)) = self.domains.get_key_value(host) {
            chain.push((domain.as_str(), rule));
        }
        // An IP address has no parent domains.
        if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
            return chain;
        }
        let mut rest = host;
        while let Some((_, parent)) = rest.split_once('.') {
            if let Some((domain, rule)) = self.domains.get_key_value(parent) {
                chain.push((domain.as_str(), rule));
            }
            rest = parent;
        }
        chain
    }
}

/// The value of one field for a host: from the nearest domain entry that
/// sets it, else from the `defaults` block.
fn inherited<T>(
    rules: &[&DomainRule],
    defaults: &DomainRule,
    field: impl Fn(&DomainRule) -> Option<T>,
) -> Option<T> {
    rules
        .iter()
        .find_map(|rule| field(rule))
        .or_else(|| field(defaults))
}

/// Lower-cases a host, drops a trailing dot and converts an
/// internationalized name to its ASCII form. `None` when it is not a host.
pub fn normalize_host(host: &str) -> Option<String> {
    let host = host.trim();
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    match url::Host::parse(host).ok()? {
        url::Host::Domain(domain) => Some(domain),
        url::Host::Ipv4(address) => Some(address.to_string()),
        url::Host::Ipv6(address) => Some(format!("[{address}]")),
    }
}

fn normalize_domain_key(key: &str) -> Result<String, PolicyError> {
    let invalid = |reason: &str| PolicyError::InvalidDomain {
        domain: key.to_owned(),
        reason: reason.to_owned(),
    };
    if key.contains('*') {
        return Err(invalid(
            "wildcards are not needed, an entry already covers its subdomains",
        ));
    }
    if key.contains("://") || key.contains('/') {
        return Err(invalid("write the host only, without a scheme or a path"));
    }
    normalize_host(key).ok_or_else(|| invalid("not a host name"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::{
        ActiveHoursMode, Cooldowns, DomainPolicies, DomainRule, PacingRequest, PacingSource,
        PolicyError, PolicyMatch, RefererStrategy, RobotsMode, normalize_host,
    };
    use crate::domain::{Engine, Level};
    use crate::duration::HumanDuration;
    use crate::macros::test_support::assert_string_enum;
    use crate::outcome::Outcome;
    use crate::pacing::{PacingError, PacingProfiles};

    const SPEC_DOMAINS: &str = r#"
defaults:
  pacing: human
  robots: report
  retries: 2
  breaker_threshold: 5
  breaker_pause: 30m
domains:
  "example.org":
    min_level: 3
    countries: ["DE", "AT"]
    engines: ["chromium", "firefox"]
    pacing: human
    pacing_locked: false
    robots: enforce
    referer_strategy: search_engine
    cooldowns: { rate_limited: 2h, forbidden: 24h }
  "api.lenient.example":
    min_level: 1
    pacing: fast
"#;

    const PROFILES: &str = "
human:
  gap_ms: { dist: lognormal, median: 4500, p95: 15000, min: 1500 }
  burst_pages: { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 60000, p95: 240000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
fast:
  gap_ms: { dist: uniform, min: 300, max: 1200 }
  burst_pages: { min: 50, max: 50 }
  burst_break_ms: { dist: uniform, min: 0, max: 0 }
  daily_cap_per_persona_per_domain: 5000
  domain_concurrency: 10
  domain_rpm: 300
";

    fn policies() -> DomainPolicies {
        DomainPolicies::from_yaml(SPEC_DOMAINS).unwrap()
    }

    fn profiles() -> PacingProfiles {
        PacingProfiles::from_yaml(PROFILES).unwrap()
    }

    fn with_domain(yaml_body: &str) -> DomainPolicies {
        DomainPolicies::from_yaml(&format!("domains:\n  example.org:\n{yaml_body}")).unwrap()
    }

    #[test]
    fn enum_names_are_the_spec_names() {
        assert_string_enum(
            RobotsMode::ALL,
            RobotsMode::as_str,
            &["report", "enforce", "ignore"],
        );
        assert_string_enum(
            RefererStrategy::ALL,
            RefererStrategy::as_str,
            &["direct", "search_engine", "none"],
        );
        assert_string_enum(
            ActiveHoursMode::ALL,
            ActiveHoursMode::as_str,
            &["enforce", "prefer", "ignore"],
        );
    }

    #[test]
    fn active_hours_default_to_prefer_and_cascade_like_every_field() {
        assert_eq!(
            DomainPolicies::default()
                .resolve("example.org")
                .active_hours,
            ActiveHoursMode::Prefer
        );
        let policies = DomainPolicies::from_yaml(
            "defaults:
  active_hours: ignore
domains:
  example.org:
    active_hours: enforce
  shop.example.org:
    min_level: 1
",
        )
        .unwrap();

        assert_eq!(
            policies.resolve("example.org").active_hours,
            ActiveHoursMode::Enforce
        );
        // Inherited from the parent domain, not from the defaults.
        assert_eq!(
            policies.resolve("shop.example.org").active_hours,
            ActiveHoursMode::Enforce
        );
        assert_eq!(
            policies.resolve("other.test").active_hours,
            ActiveHoursMode::Ignore
        );
        assert!(matches!(
            DomainPolicies::from_yaml(
                "defaults:
  active_hours: sometimes
"
            ),
            Err(PolicyError::Parse(_))
        ));
    }

    #[test]
    fn exact_match_uses_the_domain_entry() {
        let policy = policies().resolve("example.org");

        assert_eq!(policy.host, "example.org");
        assert_eq!(policy.matched, PolicyMatch::Exact("example.org".to_owned()));
        assert_eq!(policy.min_level, Level::Human);
        assert_eq!(
            policy.countries,
            Some(vec!["DE".parse().unwrap(), "AT".parse().unwrap()])
        );
        assert_eq!(policy.engines, [Engine::Chromium, Engine::Firefox]);
        assert_eq!(policy.pacing, "human");
        assert_eq!(policy.pacing_source, PacingSource::Domain);
        assert!(!policy.pacing_locked);
        assert_eq!(policy.robots, RobotsMode::Enforce);
        assert_eq!(policy.referer_strategy, RefererStrategy::SearchEngine);
        // Unset in the entry: taken from `defaults`.
        assert_eq!(policy.retries, 2);
        assert_eq!(policy.breaker_threshold, 5);
        assert_eq!(policy.breaker_pause, HumanDuration::from_mins(30));
    }

    #[test]
    fn exact_match_ignores_case_and_a_trailing_dot() {
        for host in ["EXAMPLE.ORG", "Example.Org.", "  example.org  "] {
            let policy = policies().resolve(host);
            assert_eq!(
                policy.matched,
                PolicyMatch::Exact("example.org".to_owned()),
                "{host:?}"
            );
            assert_eq!(policy.host, "example.org");
        }
    }

    #[test]
    fn subdomains_match_their_nearest_parent() {
        let policies = policies();

        for host in ["www.example.org", "a.b.c.example.org"] {
            let policy = policies.resolve(host);
            assert_eq!(
                policy.matched,
                PolicyMatch::Parent("example.org".to_owned()),
                "{host}"
            );
            assert_eq!(policy.min_level, Level::Human);
            assert_eq!(policy.robots, RobotsMode::Enforce);
        }

        let lenient = policies.resolve("v2.api.lenient.example");
        assert_eq!(
            lenient.matched,
            PolicyMatch::Parent("api.lenient.example".to_owned())
        );
        assert_eq!(lenient.min_level, Level::Emulated);
        assert_eq!(lenient.pacing, "fast");
    }

    const CASCADE: &str = "
defaults:
  retries: 4
  robots: report
  cooldowns: { soft_block: 3h }
domains:
  example.org:
    min_level: 1
    robots: enforce
    retries: 1
    countries: [DE, AT]
    pacing: fast
    pacing_locked: true
    cooldowns: { rate_limited: 30m, forbidden: 12h }
  shop.example.org:
    min_level: 3
    cooldowns: { forbidden: 48h }
  cart.shop.example.org:
    retries: 0
    pacing_locked: false
";

    #[test]
    fn each_field_comes_from_the_nearest_entry_that_sets_it() {
        let policies = DomainPolicies::from_yaml(CASCADE).unwrap();

        // Three levels deep: own entry, then shop, then example.org.
        let cart = policies.resolve("cart.shop.example.org");
        assert_eq!(
            cart.matched,
            PolicyMatch::Exact("cart.shop.example.org".to_owned())
        );
        assert_eq!(
            cart.inherits,
            ["cart.shop.example.org", "shop.example.org", "example.org"]
        );
        assert_eq!(cart.retries, 0); // its own
        assert!(!cart.pacing_locked); // its own, overriding the lock above
        assert_eq!(cart.min_level, Level::Human); // from shop.example.org
        assert_eq!(cart.robots, RobotsMode::Enforce); // from example.org
        assert_eq!(cart.pacing, "fast"); // from example.org
        assert_eq!(cart.pacing_source, PacingSource::Domain);
        assert_eq!(
            cart.countries,
            Some(vec!["DE".parse().unwrap(), "AT".parse().unwrap()])
        );
        assert_eq!(cart.breaker_threshold, 5); // built-in: nobody sets it

        // One level up, and a host below it that has no entry of its own.
        for host in ["shop.example.org", "eu.shop.example.org"] {
            let shop = policies.resolve(host);
            assert_eq!(shop.inherits, ["shop.example.org", "example.org"], "{host}");
            assert_eq!(shop.min_level, Level::Human, "{host}");
            assert_eq!(shop.retries, 1, "{host}"); // from example.org, not defaults
            assert!(shop.pacing_locked, "{host}"); // the lock covers subdomains
            assert_eq!(shop.robots, RobotsMode::Enforce, "{host}");
        }
        assert_eq!(
            policies.resolve("eu.shop.example.org").matched,
            PolicyMatch::Parent("shop.example.org".to_owned())
        );

        // A sibling only sees example.org.
        let blog = policies.resolve("blog.example.org");
        assert_eq!(blog.matched, PolicyMatch::Parent("example.org".to_owned()));
        assert_eq!(blog.inherits, ["example.org"]);
        assert_eq!(blog.min_level, Level::Emulated);
        assert_eq!(blog.retries, 1);

        // An unrelated host sees none of it.
        let other = policies.resolve("example.net");
        assert_eq!(other.matched, PolicyMatch::Defaults);
        assert_eq!(other.inherits, Vec::<String>::new());
        assert_eq!(other.retries, 4); // the defaults block
        assert_eq!(other.min_level, Level::Plain);
        assert_eq!(other.pacing, "human");
        assert_eq!(other.pacing_source, PacingSource::Default);
        assert!(!other.pacing_locked);
        assert_eq!(other.countries, None);
    }

    #[test]
    fn cooldown_classes_resolve_to_policy_durations() {
        use crate::outcome::CooldownClass;

        let defaults = Cooldowns::default();
        let minutes = |minutes: u64| Some(Duration::from_mins(minutes));

        // long: the cooldown of the outcome (spec §6.3).
        assert_eq!(
            defaults.resolve(Outcome::Forbidden, CooldownClass::Long),
            Some(Duration::from_hours(24))
        );
        assert_eq!(
            defaults.resolve(Outcome::RateLimited, CooldownClass::Long),
            Some(Duration::from_hours(2))
        );
        // short: thirty minutes by default, whatever the outcome.
        assert_eq!(
            defaults.resolve(Outcome::Forbidden, CooldownClass::Short),
            minutes(30)
        );
        assert_eq!(
            defaults.resolve(Outcome::RateLimited, CooldownClass::Short),
            minutes(30)
        );
        assert_eq!(
            defaults.resolve(Outcome::ProviderError, CooldownClass::Short),
            minutes(30)
        );
        // none: no cooldown, even for a block.
        for outcome in Outcome::ALL {
            assert_eq!(
                defaults.resolve(outcome, CooldownClass::None),
                None,
                "{outcome}"
            );
        }
        // long on an outcome the policy has no cooldown for is no cooldown.
        assert_eq!(
            defaults.resolve(Outcome::ProviderError, CooldownClass::Long),
            None
        );

        // Both are policy data, and short never exceeds the outcome's own.
        let policy = with_domain("    cooldowns: { short: 5m, rate_limited: 2m, forbidden: 1h }\n")
            .resolve("example.org");
        assert_eq!(
            policy
                .cooldowns
                .resolve(Outcome::Forbidden, CooldownClass::Short),
            minutes(5)
        );
        assert_eq!(
            policy
                .cooldowns
                .resolve(Outcome::Forbidden, CooldownClass::Long),
            minutes(60)
        );
        assert_eq!(
            policy
                .cooldowns
                .resolve(Outcome::RateLimited, CooldownClass::Short),
            minutes(2)
        );
    }

    #[test]
    fn cooldowns_cascade_per_outcome() {
        let policies = DomainPolicies::from_yaml(CASCADE).unwrap();
        let hours = |hours: u64| Some(Duration::from_hours(hours));

        let cart = policies.resolve("cart.shop.example.org").cooldowns;
        assert_eq!(cart.after(Outcome::Forbidden), hours(48)); // shop.example.org
        assert_eq!(
            cart.after(Outcome::RateLimited),
            Some(Duration::from_mins(30))
        ); // example.org
        assert_eq!(cart.after(Outcome::SoftBlock), hours(3)); // the defaults block
        assert_eq!(cart.after(Outcome::Captcha), hours(24)); // built-in

        let blog = policies.resolve("blog.example.org").cooldowns;
        assert_eq!(blog.after(Outcome::Forbidden), hours(12));
        assert_eq!(
            blog.after(Outcome::RateLimited),
            Some(Duration::from_mins(30))
        );

        let other = policies.resolve("example.net").cooldowns;
        assert_eq!(other.after(Outcome::Forbidden), hours(24));
        assert_eq!(other.after(Outcome::RateLimited), hours(2));
        assert_eq!(other.after(Outcome::SoftBlock), hours(3));
    }

    #[test]
    fn a_lock_on_a_domain_covers_its_subdomains_until_one_lifts_it() {
        let policies = DomainPolicies::from_yaml(CASCADE).unwrap();
        let profiles = profiles();
        let human = PacingRequest::Profile("human".to_owned());

        // Locked by example.org: the request's choice is ignored.
        let locked = policies
            .resolve("eu.shop.example.org")
            .pacing_for(Some(&human), &profiles)
            .unwrap();
        assert_eq!(locked.applied, "fast");
        assert_eq!(locked.source, PacingSource::Domain);
        assert!(locked.locked && locked.request_ignored);

        // cart.shop.example.org lifts the lock for itself.
        let lifted = policies
            .resolve("cart.shop.example.org")
            .pacing_for(Some(&human), &profiles)
            .unwrap();
        assert_eq!(lifted.applied, "human");
        assert_eq!(lifted.source, PacingSource::Request);
        assert!(!lifted.locked && !lifted.request_ignored);
    }

    #[test]
    fn suffix_matching_respects_label_boundaries() {
        let policies = policies();

        // "notexample.org" ends with "example.org" as text, not as a domain.
        for host in [
            "notexample.org",
            "example.org.evil.test",
            "org",
            "lenient.example",
        ] {
            assert_eq!(
                policies.resolve(host).matched,
                PolicyMatch::Defaults,
                "{host}"
            );
        }
    }

    #[test]
    fn unknown_hosts_get_the_defaults_block() {
        let policy = policies().resolve("unknown.test");

        assert_eq!(policy.matched, PolicyMatch::Defaults);
        assert_eq!(policy.min_level, Level::Plain);
        assert_eq!(policy.pacing, "human");
        assert_eq!(policy.pacing_source, PacingSource::Default);
        assert_eq!(policy.robots, RobotsMode::Report);
        assert_eq!(policy.retries, 2);
        assert_eq!(policy.breaker_threshold, 5);
        assert_eq!(policy.breaker_pause, HumanDuration::from_mins(30));
        assert_eq!(policy.countries, None);
        assert_eq!(policy.engines, [Engine::Chromium, Engine::Firefox]);
        assert!(!policy.pacing_locked);
        assert_eq!(policy.referer_strategy, RefererStrategy::Direct);
        assert_eq!(policy.cooldowns, Cooldowns::default());
    }

    #[test]
    fn built_in_defaults_apply_when_the_file_is_empty() {
        let from_empty = DomainPolicies::from_yaml("").unwrap();
        assert_eq!(from_empty, DomainPolicies::default());
        assert!(from_empty.domains().is_empty());
        assert_eq!(from_empty.defaults(), &DomainRule::default());

        let policy = from_empty.resolve("anything.test");
        assert_eq!(policy.matched, PolicyMatch::Defaults);
        assert_eq!(policy.min_level, Level::Plain);
        assert_eq!(policy.pacing, "human");
        assert_eq!(policy.retries, 2);
        assert_eq!(policy.breaker_threshold, 5);
        assert_eq!(policy.breaker_pause.get(), Duration::from_mins(30));
    }

    #[test]
    fn defaults_block_overrides_built_ins_for_every_host() {
        let policies = DomainPolicies::from_yaml(
            "
defaults:
  min_level: 1
  robots: ignore
  retries: 0
  breaker_threshold: 9
  breaker_pause: 1h
  pacing_locked: true
  referer_strategy: none
  engines: [firefox]
  countries: [NO]
  cooldowns: { soft_block: 1h }
",
        )
        .unwrap();

        let policy = policies.resolve("anything.test");
        assert_eq!(policy.min_level, Level::Emulated);
        assert_eq!(policy.robots, RobotsMode::Ignore);
        assert_eq!(policy.retries, 0);
        assert_eq!(policy.breaker_threshold, 9);
        assert_eq!(policy.breaker_pause, HumanDuration::from_hours(1));
        assert!(policy.pacing_locked);
        assert_eq!(policy.referer_strategy, RefererStrategy::None);
        assert_eq!(policy.engines, [Engine::Firefox]);
        assert_eq!(policy.countries, Some(vec!["NO".parse().unwrap()]));
        assert_eq!(policy.cooldowns.soft_block, HumanDuration::from_hours(1));
        assert_eq!(policy.cooldowns.forbidden, HumanDuration::from_hours(24));
    }

    #[test]
    fn ip_hosts_match_exactly_or_not_at_all() {
        let policies = DomainPolicies::from_yaml(
            "domains:\n  \"203.0.113.7\": { min_level: 1 }\n  \"0.113.7\": { min_level: 3 }\n",
        );
        // "0.113.7" is read as an IPv4 address by the URL standard.
        let policies = policies.unwrap();

        assert_eq!(
            policies.resolve("203.0.113.7").matched,
            PolicyMatch::Exact("203.0.113.7".to_owned())
        );
        assert_eq!(
            policies.resolve("203.0.113.8").matched,
            PolicyMatch::Defaults
        );
        assert_eq!(
            policies.resolve("[2001:db8::1]").matched,
            PolicyMatch::Defaults
        );
    }

    #[test]
    fn internationalized_hosts_match_in_ascii_form() {
        let policies =
            DomainPolicies::from_yaml("domains:\n  \"bücher.example\": { min_level: 1 }\n")
                .unwrap();

        assert!(policies.domains().contains_key("xn--bcher-kva.example"));
        for host in [
            "bücher.example",
            "xn--bcher-kva.example",
            "www.BÜCHER.example",
        ] {
            assert_eq!(policies.resolve(host).min_level, Level::Emulated, "{host}");
        }
    }

    #[test]
    fn hosts_that_are_not_hosts_get_the_defaults() {
        let policy = policies().resolve("not a host/");

        assert_eq!(policy.matched, PolicyMatch::Defaults);
        assert_eq!(policy.host, "not a host/");
        assert_eq!(normalize_host(""), None);
        assert_eq!(normalize_host("."), None);
        assert_eq!(
            normalize_host("Example.ORG."),
            Some("example.org".to_owned())
        );
        assert_eq!(normalize_host("[::1]"), Some("[::1]".to_owned()));
    }

    #[test]
    fn cooldowns_merge_per_outcome() {
        let policy = with_domain("    cooldowns: { rate_limited: 30m, soft_block: 1h }\n")
            .resolve("example.org");

        assert_eq!(
            policy.cooldowns.after(Outcome::RateLimited),
            Some(Duration::from_mins(30))
        );
        assert_eq!(
            policy.cooldowns.after(Outcome::SoftBlock),
            Some(Duration::from_hours(1))
        );
        // Untouched outcomes keep the spec §6.3 defaults.
        assert_eq!(
            policy.cooldowns.after(Outcome::Forbidden),
            Some(Duration::from_hours(24))
        );
        assert_eq!(
            policy.cooldowns.after(Outcome::Captcha),
            Some(Duration::from_hours(24))
        );
        assert_eq!(
            policy.cooldowns.after(Outcome::ChallengeFailed),
            Some(Duration::from_hours(24))
        );
    }

    #[test]
    fn default_cooldowns_follow_the_spec_and_cover_exactly_the_blocks() {
        let cooldowns = Cooldowns::default();

        assert_eq!(
            cooldowns.after(Outcome::RateLimited),
            Some(Duration::from_hours(2))
        );
        assert_eq!(
            cooldowns.after(Outcome::SoftBlock),
            Some(Duration::from_hours(6))
        );
        for outcome in Outcome::ALL {
            assert_eq!(
                cooldowns.after(outcome).is_some(),
                outcome.is_block(),
                "{outcome}"
            );
        }
    }

    #[test]
    fn a_site_admits_its_floor_and_every_level_above_it_and_none_below() {
        let policies = policies();
        // No floor: every level, the plain client included.
        let unknown = policies.resolve("unknown.test");
        assert_eq!(unknown.min_level, Level::Plain);
        for level in Level::ALL {
            assert!(unknown.admits(level), "{level}");
        }
        // A floor of 3: only a person.
        let protected = policies.resolve("example.org");
        assert_eq!(protected.min_level, Level::Human);
        assert_eq!(
            Level::ALL.map(|level| protected.admits(level)),
            [false, false, false, true]
        );
        // A floor of 1: the persona's http client and above.
        let lenient = policies.resolve("api.lenient.example");
        assert_eq!(
            Level::ALL.map(|level| lenient.admits(level)),
            [false, true, true, true]
        );
        let two = with_domain("    min_level: 2\n").resolve("example.org");
        assert_eq!(
            Level::ALL.map(|level| two.admits(level)),
            [false, false, true, true]
        );
    }

    #[test]
    fn the_render_of_1x_is_refused_and_names_what_took_its_place() {
        for body in [
            "    render: http\n",
            "    render: browser\n",
            "    render: auto\n",
        ] {
            let error = DomainPolicies::from_yaml(&format!("domains:\n  example.org:\n{body}"))
                .unwrap_err();
            let PolicyError::InvalidField { field, reason, .. } = &error else {
                panic!("{error:?}");
            };
            assert_eq!(*field, "render");
            assert!(reason.contains("min_level"), "{reason}");
            assert!(reason.contains("level"), "{reason}");
        }
        let defaults = DomainPolicies::from_yaml("defaults:\n  render: http\n").unwrap_err();
        assert!(
            matches!(&defaults, PolicyError::InvalidField { scope, field: "render", .. } if scope == "defaults"),
            "{defaults:?}"
        );
        // Whatever was written there, even what was never a render mode.
        assert!(DomainPolicies::from_yaml("defaults:\n  render: { a: 1 }\n").is_err());
    }

    #[test]
    fn a_floor_is_a_level_and_nothing_else() {
        for refused in ["4", "-1", "\"2\"", "two", "1.5"] {
            assert!(
                DomainPolicies::from_yaml(&format!("defaults:\n  min_level: {refused}\n")).is_err(),
                "{refused}"
            );
        }
        let floor = DomainPolicies::from_yaml("defaults:\n  min_level: 2\n").unwrap();
        assert_eq!(floor.resolve("any.test").min_level, Level::Fast);
    }

    #[test]
    fn engine_and_country_filters() {
        let policies = policies();
        let restricted = policies.resolve("example.org");
        let open = policies.resolve("unknown.test");
        let firefox_only = with_domain("    engines: [firefox]\n").resolve("example.org");

        assert!(restricted.allows_country(Some("DE".parse().unwrap())));
        assert!(restricted.allows_country(Some("AT".parse().unwrap())));
        assert!(!restricted.allows_country(Some("FR".parse().unwrap())));
        assert!(!restricted.allows_country(None));
        assert!(open.allows_country(Some("FR".parse().unwrap())));
        assert!(open.allows_country(None));

        assert!(open.allows_engine(Engine::Chromium));
        assert!(open.allows_engine(Engine::Firefox));
        assert!(firefox_only.allows_engine(Engine::Firefox));
        assert!(!firefox_only.allows_engine(Engine::Chromium));
    }

    #[test]
    fn pacing_resolution_order_is_request_then_domain_then_default() {
        let policies = policies();
        let profiles = profiles();
        let fast = PacingRequest::Profile("fast".to_owned());

        // Request wins over the domain entry.
        let decision = policies
            .resolve("example.org")
            .pacing_for(Some(&fast), &profiles)
            .unwrap();
        assert_eq!(decision.applied, "fast");
        assert_eq!(decision.source, PacingSource::Request);
        assert_eq!(&decision.profile, profiles.get("fast").unwrap());
        assert!(!decision.locked);
        assert!(!decision.request_ignored);

        // No request: the domain entry.
        let decision = policies
            .resolve("api.lenient.example")
            .pacing_for(None, &profiles)
            .unwrap();
        assert_eq!(decision.applied, "fast");
        assert_eq!(decision.source, PacingSource::Domain);

        // No request, no entry: the global default.
        let decision = policies
            .resolve("unknown.test")
            .pacing_for(None, &profiles)
            .unwrap();
        assert_eq!(decision.applied, "human");
        assert_eq!(decision.source, PacingSource::Default);
        assert_eq!(&decision.profile, profiles.default_profile());
    }

    #[test]
    fn a_request_may_supply_its_own_profile() {
        let profiles = profiles();
        let mut custom = *profiles.get("fast").unwrap();
        custom.domain_rpm = 7;

        let decision = policies()
            .resolve("unknown.test")
            .pacing_for(Some(&PacingRequest::Custom(custom)), &profiles)
            .unwrap();

        assert_eq!(decision.applied, "custom");
        assert_eq!(decision.source, PacingSource::Request);
        assert_eq!(decision.profile.domain_rpm, 7);

        custom.domain_rpm = 0;
        let error = policies()
            .resolve("unknown.test")
            .pacing_for(Some(&PacingRequest::Custom(custom)), &profiles)
            .unwrap_err();
        assert!(matches!(
            error,
            PacingError::InvalidField {
                field: "domain_rpm",
                ..
            }
        ));
    }

    #[test]
    fn a_locked_policy_ignores_the_request_and_says_so() {
        let profiles = profiles();
        let locked =
            with_domain("    pacing: human\n    pacing_locked: true\n").resolve("example.org");
        assert!(locked.pacing_locked);

        for request in [
            PacingRequest::Profile("fast".to_owned()),
            // Even a request that would be invalid is ignored, not rejected.
            PacingRequest::Profile("no-such-profile".to_owned()),
            PacingRequest::Custom(*profiles.get("fast").unwrap()),
        ] {
            let decision = locked.pacing_for(Some(&request), &profiles).unwrap();
            assert_eq!(decision.applied, "human");
            assert_eq!(decision.source, PacingSource::Domain);
            assert_eq!(&decision.profile, profiles.get("human").unwrap());
            assert!(decision.locked);
            assert!(decision.request_ignored);
        }

        // Nothing was asked for, so nothing was ignored.
        let decision = locked.pacing_for(None, &profiles).unwrap();
        assert!(decision.locked);
        assert!(!decision.request_ignored);
    }

    #[test]
    fn a_lock_in_defaults_applies_to_unknown_hosts() {
        let policies = DomainPolicies::from_yaml("defaults:\n  pacing_locked: true\n").unwrap();
        let fast = PacingRequest::Profile("fast".to_owned());

        let decision = policies
            .resolve("unknown.test")
            .pacing_for(Some(&fast), &profiles())
            .unwrap();

        assert_eq!(decision.applied, "human");
        assert_eq!(decision.source, PacingSource::Default);
        assert!(decision.request_ignored);
    }

    #[test]
    fn unknown_profiles_are_errors_when_not_locked() {
        let profiles = profiles();
        let warp = PacingRequest::Profile("warp".to_owned());

        assert_eq!(
            policies()
                .resolve("unknown.test")
                .pacing_for(Some(&warp), &profiles),
            Err(PacingError::UnknownProfile("warp".to_owned()))
        );

        // A policy that names a profile missing from the set is caught when
        // the files are loaded together, and here as a second line of defence.
        let stale = with_domain("    pacing: slow\n");
        assert_eq!(
            stale.check_pacing(&profiles),
            Err(PolicyError::UnknownPacing {
                scope: "example.org".to_owned(),
                profile: "slow".to_owned()
            })
        );
        assert_eq!(
            stale.resolve("example.org").pacing_for(None, &profiles),
            Err(PacingError::UnknownProfile("slow".to_owned()))
        );

        let bad_default = DomainPolicies::from_yaml("defaults:\n  pacing: slow\n").unwrap();
        assert_eq!(
            bad_default.check_pacing(&profiles),
            Err(PolicyError::UnknownPacing {
                scope: "defaults".to_owned(),
                profile: "slow".to_owned()
            })
        );
        assert_eq!(policies().check_pacing(&profiles), Ok(()));
    }

    #[test]
    fn invalid_documents_are_parse_errors() {
        for text in [
            "defaults: [",
            "defaults:\n  robots: maybe\n",
            "defaults:\n  breaker_pause: soon\n",
            "defaults:\n  retries: -1\n",
            "defaults:\n  colour: blue\n",
            "domains:\n  example.org:\n    engines: [netscape]\n",
            "domains:\n  example.org:\n    countries: [Germany]\n",
            "domains:\n  example.org:\n    cooldowns: { timeout: 1h }\n",
            "domains:\n  example.org:\n    referer_strategy: carrier_pigeon\n",
            "domain:\n  example.org: {}\n",
        ] {
            assert!(
                matches!(DomainPolicies::from_yaml(text), Err(PolicyError::Parse(_))),
                "{text:?}"
            );
        }
    }

    fn invalid_field(body: &str) -> &'static str {
        match DomainPolicies::from_yaml(&format!("domains:\n  example.org:\n{body}")) {
            Err(PolicyError::InvalidField { scope, field, .. }) => {
                assert_eq!(scope, "example.org");
                field
            }
            other => panic!("expected an invalid field, got {other:?}"),
        }
    }

    #[test]
    fn out_of_range_fields_are_rejected() {
        assert_eq!(invalid_field("    countries: []\n"), "countries");
        assert_eq!(invalid_field("    engines: []\n"), "engines");
        assert_eq!(
            invalid_field("    engines: [firefox, firefox]\n"),
            "engines"
        );
        assert_eq!(invalid_field("    retries: 11\n"), "retries");
        assert_eq!(
            invalid_field("    breaker_threshold: 0\n"),
            "breaker_threshold"
        );
        assert_eq!(invalid_field("    breaker_pause: 0s\n"), "breaker_pause");
        assert_eq!(invalid_field("    pacing: \"\"\n"), "pacing");

        assert!(matches!(
            DomainPolicies::from_yaml("defaults:\n  retries: 99\n"),
            Err(PolicyError::InvalidField { scope, field: "retries", .. }) if scope == "defaults"
        ));
    }

    #[test]
    fn domain_keys_must_be_plain_hosts() {
        for key in [
            "*.example.org",
            "https://example.org",
            "example.org/path",
            "exa mple.org",
            "",
        ] {
            let result = DomainPolicies::new(
                DomainRule::default(),
                BTreeMap::from([(key.to_owned(), DomainRule::default())]),
            );
            assert!(
                matches!(&result, Err(PolicyError::InvalidDomain { domain, .. }) if domain == key),
                "{key:?} gave {result:?}"
            );
        }
    }

    #[test]
    fn keys_that_normalize_to_the_same_host_are_duplicates() {
        let result = DomainPolicies::new(
            DomainRule::default(),
            BTreeMap::from([
                ("Example.org".to_owned(), DomainRule::default()),
                ("example.org.".to_owned(), DomainRule::default()),
            ]),
        );

        assert_eq!(
            result,
            Err(PolicyError::DuplicateDomain("example.org".to_owned()))
        );
    }

    #[test]
    fn effective_policy_serializes_for_the_preview_endpoint() {
        let json = serde_json::to_value(policies().resolve("www.example.org")).unwrap();

        assert_eq!(json["host"], "www.example.org");
        assert_eq!(
            json["matched"],
            serde_json::json!({ "kind": "parent", "domain": "example.org" })
        );
        assert_eq!(json["min_level"], 3);
        assert_eq!(json["countries"], serde_json::json!(["DE", "AT"]));
        assert_eq!(json["breaker_pause"], "30m");
        assert_eq!(json["cooldowns"]["rate_limited"], "2h");
        assert_eq!(json["pacing_source"], "domain");
        assert_eq!(json["inherits"], serde_json::json!(["example.org"]));
        assert_eq!(json["cooldowns"]["short"], "30m");

        let defaults = serde_json::to_value(policies().resolve("unknown.test")).unwrap();
        assert_eq!(
            defaults["matched"],
            serde_json::json!({ "kind": "defaults" })
        );

        // A written entry serializes back without its unset fields.
        let rule = serde_json::to_value(&policies().domains()["api.lenient.example"]).unwrap();
        assert_eq!(
            rule,
            serde_json::json!({ "min_level": 1, "pacing": "fast" })
        );
    }

    #[test]
    fn the_shortest_dwell_and_the_behavior_lock_cascade_like_every_field() {
        let unset = DomainPolicies::default().resolve("example.org");
        assert_eq!(unset.min_dwell, HumanDuration::from_secs(2));
        assert_eq!(unset.min_dwell.get(), super::DEFAULT_MIN_DWELL);
        assert!(!unset.behavior_locked);

        let policies = DomainPolicies::from_yaml(
            "defaults:\n  min_dwell: 3s\ndomains:\n  example.org:\n    behavior_locked: true\n  \
             shop.example.org:\n    min_dwell: 6s\n  open.example.org:\n    behavior_locked: false\n",
        )
        .unwrap();
        let site = policies.resolve("www.example.org");
        assert_eq!(site.min_dwell, HumanDuration::from_secs(3));
        assert!(site.behavior_locked);
        // The nearest entry that sets a field decides it; the lock above
        // still covers the subdomain that only changes the dwell.
        let shop = policies.resolve("shop.example.org");
        assert_eq!(shop.min_dwell, HumanDuration::from_secs(6));
        assert!(shop.behavior_locked);
        assert!(!policies.resolve("open.example.org").behavior_locked);
        let other = policies.resolve("other.test");
        assert_eq!(other.min_dwell, HumanDuration::from_secs(3));
        assert!(!other.behavior_locked);

        let json = serde_json::to_value(&shop).unwrap();
        assert_eq!(json["min_dwell"], "6s");
        assert_eq!(json["behavior_locked"], true);
    }

    #[test]
    fn driver_input_is_off_unless_a_policy_turns_it_on() {
        assert!(
            !DomainPolicies::default()
                .resolve("example.org")
                .driver_input
        );

        let policies = DomainPolicies::from_yaml(
            "domains:\n  lenient.example:\n    driver_input: true\n  \
             strict.lenient.example:\n    driver_input: false\n",
        )
        .unwrap();
        assert!(policies.resolve("lenient.example").driver_input);
        // It cascades like every field: a subdomain inherits it, and the
        // nearest entry that sets it decides.
        assert!(policies.resolve("api.lenient.example").driver_input);
        assert!(!policies.resolve("strict.lenient.example").driver_input);
        assert!(!policies.resolve("other.example").driver_input);
        let everywhere = DomainPolicies::from_yaml("defaults:\n  driver_input: true\n").unwrap();
        assert!(everywhere.resolve("example.org").driver_input);
        assert_eq!(
            serde_json::to_value(everywhere.resolve("example.org")).unwrap()["driver_input"],
            true
        );
    }

    #[test]
    fn a_dwell_floor_that_is_no_floor_or_no_dwell_is_refused() {
        for (written, valid) in [
            ("500ms", true),
            ("45s", true),
            ("499ms", false),
            ("0s", false),
            ("46s", false),
            ("2h", false),
        ] {
            let result = DomainPolicies::from_yaml(&format!("defaults:\n  min_dwell: {written}\n"));
            assert_eq!(result.is_ok(), valid, "{written}: {result:?}");
            if !valid {
                assert!(
                    matches!(
                        &result,
                        Err(PolicyError::InvalidField { scope, field: "min_dwell", .. })
                            if scope == "defaults"
                    ),
                    "{written}: {result:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod cache_tests;
