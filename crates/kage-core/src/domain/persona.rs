//! Personas: long-lived synthetic household users bound 1:1 to a proxy
//! (spec §5, §6).

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::{ClaimedOs, CountryCode, Engine};
use crate::ids::{PersonaId, ProxyId};
use crate::macros::string_enum;
use crate::outcome::Outcome;
use crate::seed::Seed;

string_enum! {
    /// Lifecycle state of a persona (spec §6.3).
    pub enum PersonaState {
        /// Serving requests.
        Active => "active",
        /// Paused: its proxy is backing off or a self-test failed.
        Cooling => "cooling",
        /// Its proxy is gone; the profile is archived, then deleted.
        Retired => "retired",
    }
}

string_enum! {
    /// Why a persona is cooling (spec §6.3). The reason decides what ends
    /// it.
    pub enum CoolingReason {
        /// Its proxy is in backoff or being replaced. Ends when the proxy
        /// is usable again.
        ProxyBackoff => "proxy_backoff",
        /// A self-test failed. Ends when a self-test passes, by schedule or
        /// by an operator's `kage persona selftest`.
        SelftestFailed => "selftest_failed",
        /// The geo record of its address moved to another country, and its
        /// locale and languages with it. Ends when a self-test passes.
        GeoChanged => "geo_changed",
    }
}

impl CoolingReason {
    /// Whether only a passing self-test ends this cooling.
    pub const fn needs_selftest(self) -> bool {
        matches!(self, Self::SelftestFailed | Self::GeoChanged)
    }
}

/// A point near the proxy's city, reported by the browser's geolocation API.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Geolocation {
    /// Degrees north.
    pub latitude: f64,
    /// Degrees east.
    pub longitude: f64,
    /// Reported accuracy radius in metres.
    pub accuracy_m: f64,
}

/// The screen a persona reports.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    /// Width in CSS pixels.
    pub width: u32,
    /// Height in CSS pixels.
    pub height: u32,
    /// Device pixel ratio.
    pub dpr: f32,
    /// Colour depth in bits.
    pub color_depth: u8,
}

/// A long-lived synthetic household user.
///
/// No identity attribute changes during a persona's lifetime (spec §6.2),
/// with one exception that follows the address and not a choice of Kage's:
/// when the observed geo record of the proxy changes, the time zone, the
/// position and, across a border, the locale and languages follow it.
#[derive(Debug, Clone, PartialEq)]
pub struct Persona {
    /// Identity.
    pub id: PersonaId,
    /// The proxy this persona lives behind; one persona per proxy among
    /// those that are not retired.
    pub proxy_id: ProxyId,
    /// Browser engine.
    pub engine: Engine,
    /// Claimed operating system.
    pub claimed_os: ClaimedOs,
    /// Major version of the engine's bundled browser at generation.
    pub browser_major: u32,
    /// The seed everything about the persona was drawn from: its
    /// fingerprint, its screen class, its position, its pacing factor, its
    /// warm-up, and in the sidecar its motor profile. It never changes.
    pub seed: Seed,
    /// The full generated fingerprint, re-applied on every session. Only
    /// the sidecar reads it.
    pub fingerprint: serde_json::Value,
    /// The `User-Agent` the persona's browser sends.
    pub user_agent: String,
    /// Logical processors the persona's browser reports, at most the
    /// engine host's.
    pub hardware_concurrency: u32,
    /// wreq-util profile name; `None` disables the http path.
    pub wreq_profile: Option<String>,
    /// The country of the persona's address: the observed one, or the
    /// provider's while no geo lookup has succeeded.
    pub country: Option<CountryCode>,
    /// Locale tag with the country as its region, for example `de-AT`.
    pub locale: String,
    /// `navigator.languages`, most preferred first: the list the persona's
    /// browser ships with for the country's language.
    pub languages: Vec<String>,
    /// IANA time zone, for example `Europe/Berlin`.
    pub timezone: String,
    /// Reported position; `None` when the geo lookup gave no coordinates.
    pub geolocation: Option<Geolocation>,
    /// Reported screen.
    pub screen: Screen,
    /// Version of the tables the persona was drawn from
    /// ([`crate::persona::TABLES_VERSION`]).
    pub tables_version: String,
    /// Browser profile directory, relative to the personas volume.
    pub profile_dir: String,
    /// Lifecycle state.
    pub state: PersonaState,
    /// Why the persona is cooling; `None` in every other state.
    pub cooling_reason: Option<CoolingReason>,
    /// Warm-up visits that were made or skipped so far.
    pub warmup_visits: u32,
    /// When the persona became ready for real work (spec §6.1 step 5).
    /// `None` while it warms up: the scheduler does not use it yet.
    pub warmed_at: Option<DateTime<Utc>>,
    /// When the persona was generated.
    pub created_at: DateTime<Utc>,
    /// When the persona was retired.
    pub retired_at: Option<DateTime<Utc>>,
    /// Why it was retired, for example `proxy_retired` or `operator`.
    pub retire_reason: Option<String>,
    /// When the profile directory of the retired persona was archived, or
    /// found to be out of reach.
    pub profile_archived_at: Option<DateTime<Utc>>,
    /// Where the archive is, relative to the personas volume. `None` after
    /// `profile_archived_at` means nothing was archived: there was no
    /// directory within reach.
    pub profile_archive: Option<String>,
    /// When the archive was deleted, after its retention.
    pub profile_deleted_at: Option<DateTime<Utc>>,
}

impl Persona {
    /// Whether this persona may use the http path (spec §8.2).
    pub const fn http_path_enabled(&self) -> bool {
        self.wreq_profile.is_some()
    }

    /// Whether the scheduler may give this persona real work: it is active
    /// and its warm-up is over (spec §6.1 step 5, §8.2).
    pub const fn is_ready(&self) -> bool {
        matches!(self.state, PersonaState::Active) && self.warmed_at.is_some()
    }

    /// Whether the persona still exists: every state but `retired`.
    pub const fn is_live(&self) -> bool {
        !matches!(self.state, PersonaState::Retired)
    }
}

/// How long the window of [`PersonaDomainState::block_count_24h`] is.
const BLOCK_WINDOW: Duration = Duration::hours(24);

/// What one persona has done on one domain: cooldowns and pacing counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaDomainState {
    /// The persona.
    pub persona_id: PersonaId,
    /// The domain (host) the counters refer to.
    pub domain: String,
    /// Set on a block; the persona is ineligible for the domain until then.
    pub cooldown_until: Option<DateTime<Utc>>,
    /// Requests made on `counter_day`.
    pub requests_today: u32,
    /// The calendar day, in the persona's time zone, that `requests_today`
    /// counts. The daily cap resets at the persona's local midnight.
    pub counter_day: Option<NaiveDate>,
    /// When the persona last requested this domain.
    pub last_request_at: Option<DateTime<Utc>>,
    /// Outcome of the most recent attempt.
    pub last_outcome: Option<Outcome>,
    /// Blocks in the 24 hours that began at `block_window_started_at`.
    pub block_count_24h: u32,
    /// When the window of `block_count_24h` began: the first block after
    /// 24 hours without the count moving on.
    pub block_window_started_at: Option<DateTime<Utc>>,
}

impl PersonaDomainState {
    /// A persona that has never visited the domain.
    pub fn new(persona_id: PersonaId, domain: impl Into<String>) -> Self {
        Self {
            persona_id,
            domain: domain.into(),
            cooldown_until: None,
            requests_today: 0,
            counter_day: None,
            last_request_at: None,
            last_outcome: None,
            block_count_24h: 0,
            block_window_started_at: None,
        }
    }

    /// Whether the persona is cooling down for this domain at `now`.
    pub fn is_cooling(&self, now: DateTime<Utc>) -> bool {
        self.cooldown_until.is_some_and(|until| until > now)
    }

    /// Requests counted for `day`; zero when the counter is for another day.
    pub fn requests_on(&self, day: NaiveDate) -> u32 {
        if self.counter_day == Some(day) {
            self.requests_today
        } else {
            0
        }
    }

    /// Whether the persona has used up its daily cap for this domain on
    /// `day`, the calendar day in its own time zone. It is eligible again
    /// at its local midnight (spec §6.3).
    pub fn cap_reached(&self, day: NaiveDate, daily_cap: u32) -> bool {
        self.requests_on(day) >= daily_cap
    }

    /// Blocks in the 24 hours up to `now`; zero when the window is over.
    pub fn blocks_in_window(&self, now: DateTime<Utc>) -> u32 {
        match self.block_window_started_at {
            Some(started) if now.signed_duration_since(started) < BLOCK_WINDOW => {
                self.block_count_24h
            }
            _ => 0,
        }
    }

    /// Counts one request at `now`, on `day` of the persona's own time
    /// zone. A request on a new day starts the count again.
    pub fn record_request(&mut self, now: DateTime<Utc>, day: NaiveDate) {
        self.requests_today = self.requests_on(day).saturating_add(1);
        self.counter_day = Some(day);
        self.last_request_at = Some(now);
    }

    /// Records how an attempt ended. A block counts toward the 24-hour
    /// window and, with a `cooldown`, keeps the persona away from the
    /// domain until it is over; a cooldown already running is never
    /// shortened. A success leaves a running cooldown alone: only time
    /// ends it.
    pub fn record_outcome(
        &mut self,
        outcome: Outcome,
        now: DateTime<Utc>,
        cooldown: Option<Duration>,
    ) {
        self.last_outcome = Some(outcome);
        if !outcome.is_block() {
            return;
        }
        self.block_count_24h = self.blocks_in_window(now).saturating_add(1);
        if self.block_count_24h == 1 {
            self.block_window_started_at = Some(now);
        }
        if let Some(cooldown) = cooldown {
            let until = now + cooldown;
            if self.cooldown_until.is_none_or(|current| current < until) {
                self.cooldown_until = Some(until);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDate, Utc};

    use super::{CoolingReason, Geolocation, Persona, PersonaDomainState, PersonaState, Screen};
    use crate::domain::{ClaimedOs, Engine};
    use crate::ids::{PersonaId, ProxyId};
    use crate::macros::test_support::assert_string_enum;
    use crate::outcome::Outcome;
    use crate::persona;

    fn persona(wreq_profile: Option<&str>) -> Persona {
        let id = PersonaId::new();
        Persona {
            id,
            proxy_id: ProxyId::new(),
            engine: Engine::Chromium,
            claimed_os: ClaimedOs::Linux,
            browser_major: 149,
            seed: persona::seed_for(id),
            fingerprint: serde_json::json!({ "schema": 1 }),
            user_agent: "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                         Chrome/149.0.0.0 Safari/537.36"
                .to_owned(),
            hardware_concurrency: 8,
            wreq_profile: wreq_profile.map(str::to_owned),
            country: Some("DE".parse().unwrap()),
            locale: "de-DE".to_owned(),
            languages: vec![
                "de-DE".to_owned(),
                "de".to_owned(),
                "en-US".to_owned(),
                "en".to_owned(),
            ],
            timezone: "Europe/Berlin".to_owned(),
            geolocation: Some(Geolocation {
                latitude: 52.52,
                longitude: 13.405,
                accuracy_m: 35.0,
            }),
            screen: Screen {
                width: 1920,
                height: 1080,
                dpr: 1.0,
                color_depth: 24,
            },
            tables_version: persona::TABLES_VERSION.to_owned(),
            profile_dir: persona::profile_dir(Engine::Chromium, id),
            state: PersonaState::Active,
            cooling_reason: None,
            warmup_visits: 0,
            warmed_at: None,
            created_at: Utc::now(),
            retired_at: None,
            retire_reason: None,
            profile_archived_at: None,
            profile_archive: None,
            profile_deleted_at: None,
        }
    }

    #[test]
    fn state_names_are_the_spec_names() {
        assert_string_enum(
            PersonaState::ALL,
            PersonaState::as_str,
            &["active", "cooling", "retired"],
        );
        assert_string_enum(
            CoolingReason::ALL,
            CoolingReason::as_str,
            &["proxy_backoff", "selftest_failed", "geo_changed"],
        );
        assert!(!CoolingReason::ProxyBackoff.needs_selftest());
        assert!(CoolingReason::SelftestFailed.needs_selftest());
        assert!(CoolingReason::GeoChanged.needs_selftest());
    }

    #[test]
    fn http_path_needs_a_wreq_profile() {
        assert!(persona(Some("chrome_149")).http_path_enabled());
        assert!(!persona(None).http_path_enabled());
    }

    #[test]
    fn a_persona_is_ready_for_work_once_it_is_active_and_warmed_up() {
        let mut persona = persona(None);
        assert!(persona.is_live());
        // Generated a moment ago: still warming up.
        assert!(!persona.is_ready());

        persona.warmed_at = Some(Utc::now());
        assert!(persona.is_ready());

        persona.state = PersonaState::Cooling;
        persona.cooling_reason = Some(CoolingReason::ProxyBackoff);
        assert!(!persona.is_ready());
        assert!(persona.is_live());

        persona.state = PersonaState::Retired;
        assert!(!persona.is_ready());
        assert!(!persona.is_live());
    }

    #[test]
    fn screen_and_geolocation_round_trip_as_json() {
        let persona = persona(None);

        let screen = serde_json::to_value(persona.screen).unwrap();
        assert_eq!(screen["width"], 1920);
        assert_eq!(
            serde_json::from_value::<Screen>(screen).unwrap(),
            persona.screen
        );

        let geo = serde_json::to_value(persona.geolocation).unwrap();
        assert_eq!(
            serde_json::from_value::<Option<Geolocation>>(geo).unwrap(),
            persona.geolocation
        );
    }

    #[test]
    fn cooldown_ends_at_its_deadline() {
        let now = Utc::now();
        let mut state = PersonaDomainState::new(PersonaId::new(), "example.org");

        assert!(!state.is_cooling(now));

        state.cooldown_until = Some(now + Duration::hours(2));
        assert!(state.is_cooling(now));
        assert!(state.is_cooling(now + Duration::minutes(119)));
        assert!(!state.is_cooling(now + Duration::hours(2)));
    }

    #[test]
    fn daily_counter_only_counts_its_own_day() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 8).unwrap();
        let mut state = PersonaDomainState::new(PersonaId::new(), "example.org");

        assert_eq!(state.requests_on(today), 0);

        state.requests_today = 17;
        state.counter_day = Some(today);
        assert_eq!(state.requests_on(today), 17);
        assert_eq!(state.requests_on(today.succ_opt().unwrap()), 0);
    }

    #[test]
    fn requests_count_per_local_day_up_to_the_cap() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 8).unwrap();
        let tomorrow = today.succ_opt().unwrap();
        let now = Utc::now();
        let mut state = PersonaDomainState::new(PersonaId::new(), "example.org");

        for _ in 0..3 {
            state.record_request(now, today);
        }
        assert_eq!(state.requests_on(today), 3);
        assert_eq!(state.last_request_at, Some(now));
        assert!(!state.cap_reached(today, 4));
        assert!(state.cap_reached(today, 3));
        // Local midnight: the persona is eligible again and counts anew.
        assert!(!state.cap_reached(tomorrow, 3));
        let later = now + Duration::hours(9);
        state.record_request(later, tomorrow);
        assert_eq!(state.requests_on(tomorrow), 1);
        assert_eq!(state.requests_on(today), 0);
        assert_eq!(state.last_request_at, Some(later));
    }

    #[test]
    fn a_block_cools_the_persona_and_counts_for_a_day() {
        let now = Utc::now();
        let mut state = PersonaDomainState::new(PersonaId::new(), "example.org");

        // A page that loaded changes nothing but the last outcome.
        state.record_outcome(Outcome::Ok, now, None);
        assert_eq!(state.last_outcome, Some(Outcome::Ok));
        assert_eq!(state.blocks_in_window(now), 0);
        assert!(!state.is_cooling(now));

        state.record_outcome(Outcome::RateLimited, now, Some(Duration::hours(2)));
        assert_eq!(state.block_count_24h, 1);
        assert_eq!(state.block_window_started_at, Some(now));
        assert_eq!(state.cooldown_until, Some(now + Duration::hours(2)));

        // A second, worse block an hour later: counted, and the longer
        // cooldown wins.
        let hour = now + Duration::hours(1);
        state.record_outcome(Outcome::Forbidden, hour, Some(Duration::hours(24)));
        assert_eq!(state.blocks_in_window(hour), 2);
        assert_eq!(state.block_window_started_at, Some(now));
        assert_eq!(state.cooldown_until, Some(hour + Duration::hours(24)));

        // A shorter cooldown never shortens a running one, and a block
        // without a cooldown still counts.
        let two = now + Duration::hours(2);
        state.record_outcome(Outcome::RateLimited, two, Some(Duration::minutes(30)));
        state.record_outcome(Outcome::SoftBlock, two, None);
        assert_eq!(state.blocks_in_window(two), 4);
        assert_eq!(state.cooldown_until, Some(hour + Duration::hours(24)));

        // A success in between does not end the cooldown.
        state.record_outcome(Outcome::Ok, two, None);
        assert!(state.is_cooling(two));
        assert_eq!(state.last_outcome, Some(Outcome::Ok));

        // A day after the window began it is over; the next block opens a
        // new one.
        let next_day = now + Duration::hours(24);
        assert_eq!(state.blocks_in_window(next_day), 0);
        state.record_outcome(Outcome::Captcha, next_day, Some(Duration::hours(24)));
        assert_eq!(state.block_count_24h, 1);
        assert_eq!(state.block_window_started_at, Some(next_day));
    }
}
