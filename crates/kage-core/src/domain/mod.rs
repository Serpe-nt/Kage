//! The domain model (spec §5): the entities Kage stores and reasons about.
//!
//! Timestamps are UTC and ids are UUIDv7 ([`crate::ids`]). Enum values are
//! stable lowercase names shared by the API, the policy files and the store.

mod access;
mod bandwidth;
mod cookie;
mod fetch;
mod health;
mod job;
mod level;
mod persona;
mod proxy;
mod session;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use access::{ApiKey, AuditActor, AuditEvent, AuditOutcome, KeyLimits, Scope};
pub use bandwidth::{BandwidthUsage, IDLE_DOMAIN, hour_of};
pub use cookie::{CookieSameSite, HttpCookie};
pub use fetch::{
    Artifact, ArtifactEncoding, ArtifactKind, ArtifactRef, Attempt, BehaviorDone, ConsoleEntry,
    ContentHash, Exit, FetchRecord, Header, InvalidContentHash, MAX_CONSOLE_ENTRIES,
    MAX_CONSOLE_TEXT_BYTES, PacingApplied, PersonaSummary, REDACTED_COOKIE, RequestRecord,
    RobotsVerdict, Timing, Visit, redact_set_cookie,
};
pub use health::{BreakerState, DomainBreaker, HealthEvent, SelftestRun, SubjectKind};
pub use job::{
    DeliveryState, FrontierEntry, FrontierSource, FrontierState, Job, JobItem, JobItemState,
    JobKind, JobOrigin, JobState, WebhookAttempt, WebhookDelivery, WebhookEvent,
};
pub use level::{InvalidLevel, Level};
pub use persona::{CoolingReason, Geolocation, Persona, PersonaDomainState, PersonaState, Screen};
pub use proxy::{
    Declaration, GeoRecord, HealthFailure, Proxy, ProxyEndpoint, ProxyHealth, ProxyProviderKind,
    ProxyReplacement, ProxyScheme, ProxyState, ReplaceReason,
};
pub use session::{InvalidSessionId, Session, SessionId, SessionLimits};

use crate::macros::string_enum;

string_enum! {
    /// A browser implementation in the sidecar (spec §2).
    pub enum Engine {
        /// patchright driving real Chrome.
        Chromium => "chromium",
        /// Camoufox.
        Firefox => "firefox",
    }
}

string_enum! {
    /// The operating system a persona claims to run.
    pub enum ClaimedOs {
        /// Microsoft Windows.
        Windows => "windows",
        /// Apple macOS.
        Macos => "macos",
        /// Linux.
        Linux => "linux",
    }
}

string_enum! {
    /// The path an attempt actually took.
    pub enum Render {
        /// wreq, no JavaScript.
        Http => "http",
        /// The browser sidecar.
        Browser => "browser",
    }
}

string_enum! {
    /// The path a request asked for, as stored: since 2.0 the one its level
    /// implies (`http` or `browser`); `auto`, which let the domain's policy
    /// decide, only in rows of 1.x (spec §8.1, §25).
    pub enum RenderMode {
        /// Let the domain policy decide.
        Auto => "auto",
        /// The http path.
        Http => "http",
        /// The browser path.
        Browser => "browser",
    }
}

impl RenderMode {
    /// The concrete path, or `None` for `auto`.
    pub const fn fixed(self) -> Option<Render> {
        match self {
            Self::Auto => None,
            Self::Http => Some(Render::Http),
            Self::Browser => Some(Render::Browser),
        }
    }
}

impl From<Render> for RenderMode {
    fn from(render: Render) -> Self {
        match render {
            Render::Http => Self::Http,
            Render::Browser => Self::Browser,
        }
    }
}

/// An ISO 3166-1 alpha-2 country code, stored in upper case.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CountryCode([u8; 2]);

/// A string that is not two ASCII letters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a two-letter country code")]
pub struct InvalidCountryCode(pub String);

impl CountryCode {
    /// Parses a code such as `DE`; lower case is accepted and upper-cased.
    ///
    /// # Errors
    ///
    /// Fails when the text is not exactly two ASCII letters.
    pub fn new(text: &str) -> Result<Self, InvalidCountryCode> {
        match text.as_bytes() {
            [a, b] if a.is_ascii_alphabetic() && b.is_ascii_alphabetic() => {
                Ok(Self([a.to_ascii_uppercase(), b.to_ascii_uppercase()]))
            }
            _ => Err(InvalidCountryCode(text.to_owned())),
        }
    }

    /// The code as text, for example `DE`.
    pub fn as_str(&self) -> &str {
        // Both bytes are ASCII letters by construction.
        std::str::from_utf8(&self.0).unwrap_or("??")
    }
}

impl fmt::Display for CountryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for CountryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CountryCode({})", self.as_str())
    }
}

impl FromStr for CountryCode {
    type Err = InvalidCountryCode;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl Serialize for CountryCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CountryCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::new(&text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaimedOs, CountryCode, Engine, InvalidCountryCode, Render, RenderMode};
    use crate::macros::test_support::assert_string_enum;

    #[test]
    fn enum_names_are_the_spec_names() {
        assert_string_enum(Engine::ALL, Engine::as_str, &["chromium", "firefox"]);
        assert_string_enum(
            ClaimedOs::ALL,
            ClaimedOs::as_str,
            &["windows", "macos", "linux"],
        );
        assert_string_enum(Render::ALL, Render::as_str, &["http", "browser"]);
        assert_string_enum(
            RenderMode::ALL,
            RenderMode::as_str,
            &["auto", "http", "browser"],
        );
    }

    #[test]
    fn render_mode_and_render_convert() {
        assert_eq!(RenderMode::Auto.fixed(), None);
        assert_eq!(RenderMode::Http.fixed(), Some(Render::Http));
        assert_eq!(RenderMode::Browser.fixed(), Some(Render::Browser));
        assert_eq!(RenderMode::from(Render::Http), RenderMode::Http);
        assert_eq!(RenderMode::from(Render::Browser), RenderMode::Browser);
    }

    #[test]
    fn country_codes_are_normalized_to_upper_case() {
        let code = CountryCode::new("de").unwrap();

        assert_eq!(code.as_str(), "DE");
        assert_eq!(code.to_string(), "DE");
        assert_eq!(format!("{code:?}"), "CountryCode(DE)");
        assert_eq!("AT".parse::<CountryCode>().unwrap().as_str(), "AT");
        assert_eq!(serde_json::to_string(&code).unwrap(), "\"DE\"");
        assert_eq!(
            serde_json::from_str::<CountryCode>("\"no\"")
                .unwrap()
                .as_str(),
            "NO"
        );
    }

    #[test]
    fn malformed_country_codes_are_rejected() {
        for bad in ["", "D", "DEU", "D1", "ÄB", " DE"] {
            assert_eq!(
                CountryCode::new(bad),
                Err(InvalidCountryCode(bad.to_owned())),
                "{bad:?}"
            );
        }
        assert!(serde_json::from_str::<CountryCode>("\"Germany\"").is_err());
    }
}
