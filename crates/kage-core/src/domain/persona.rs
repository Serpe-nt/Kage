//! Personas: long-lived synthetic household users bound 1:1 to a proxy
//! (spec §5, §6).

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::{ClaimedOs, Engine};
use crate::ids::{PersonaId, ProxyId};
use crate::macros::string_enum;
use crate::outcome::Outcome;

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
/// No identity attribute changes during a persona's lifetime (spec §6.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Persona {
    /// Identity.
    pub id: PersonaId,
    /// The proxy this persona lives behind; one persona per proxy.
    pub proxy_id: ProxyId,
    /// Browser engine.
    pub engine: Engine,
    /// Claimed operating system.
    pub claimed_os: ClaimedOs,
    /// Major version of the engine's bundled browser at generation.
    pub browser_major: u32,
    /// The full generated fingerprint, re-applied on every session.
    pub fingerprint: serde_json::Value,
    /// wreq-util profile name; `None` disables the http path.
    pub wreq_profile: Option<String>,
    /// Primary locale, for example `de-DE`.
    pub locale: String,
    /// `navigator.languages`, most preferred first.
    pub languages: Vec<String>,
    /// IANA time zone, for example `Europe/Berlin`.
    pub timezone: String,
    /// Reported position; `None` when the proxy's city is unknown.
    pub geolocation: Option<Geolocation>,
    /// Reported screen.
    pub screen: Screen,
    /// Browser profile directory under the personas volume.
    pub profile_dir: String,
    /// Lifecycle state.
    pub state: PersonaState,
    /// When the persona was generated.
    pub created_at: DateTime<Utc>,
    /// When the persona was retired.
    pub retired_at: Option<DateTime<Utc>>,
}

impl Persona {
    /// Whether this persona may use the http path (spec §8.2).
    pub const fn http_path_enabled(&self) -> bool {
        self.wreq_profile.is_some()
    }
}

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
    /// Blocks in the last 24 hours.
    pub block_count_24h: u32,
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
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, NaiveDate, Utc};

    use super::{Geolocation, Persona, PersonaDomainState, PersonaState, Screen};
    use crate::domain::{ClaimedOs, Engine};
    use crate::ids::{PersonaId, ProxyId};
    use crate::macros::test_support::assert_string_enum;

    fn persona(wreq_profile: Option<&str>) -> Persona {
        Persona {
            id: PersonaId::new(),
            proxy_id: ProxyId::new(),
            engine: Engine::Chromium,
            claimed_os: ClaimedOs::Windows,
            browser_major: 141,
            fingerprint: serde_json::json!({ "navigator": {} }),
            wreq_profile: wreq_profile.map(str::to_owned),
            locale: "de-DE".to_owned(),
            languages: vec!["de-DE".to_owned(), "de".to_owned(), "en".to_owned()],
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
            profile_dir: "personas/abc".to_owned(),
            state: PersonaState::Active,
            created_at: Utc::now(),
            retired_at: None,
        }
    }

    #[test]
    fn state_names_are_the_spec_names() {
        assert_string_enum(
            PersonaState::ALL,
            PersonaState::as_str,
            &["active", "cooling", "retired"],
        );
    }

    #[test]
    fn http_path_needs_a_wreq_profile() {
        assert!(persona(Some("chrome_141")).http_path_enabled());
        assert!(!persona(None).http_path_enabled());
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
}
