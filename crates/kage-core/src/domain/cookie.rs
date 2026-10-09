//! Cookies of the http path (spec §6.4, §9).
//!
//! One [`HttpCookie`] is one cookie as a browser stores it. The http path
//! keeps them per proxy, and so per persona, in `http_cookies`; the browser
//! path exchanges the same fields with the sidecar when state moves between
//! the two paths (milestone M6).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::macros::string_enum;

string_enum! {
    /// The `SameSite` attribute of a cookie.
    pub enum CookieSameSite {
        /// Sent only with requests from the cookie's own site.
        Strict => "strict",
        /// Also sent with a top-level navigation from another site.
        Lax => "lax",
        /// Sent with every request; needs `Secure`.
        None => "none",
    }
}

/// One stored cookie.
///
/// Two cookies are the same cookie when `domain`, `host_only`, `path` and
/// `name` are equal; storing one replaces the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpCookie {
    /// The host that set the cookie, or its `Domain` attribute. Lowercase,
    /// without a leading dot.
    pub domain: String,
    /// Whether the cookie had no `Domain` attribute and is therefore sent to
    /// exactly `domain` and not to its subdomains.
    pub host_only: bool,
    /// The path the cookie is sent under.
    pub path: String,
    /// The cookie's name.
    pub name: String,
    /// The cookie's value.
    pub value: String,
    /// Sent only to secure origins.
    pub secure: bool,
    /// Hidden from scripts. The http path runs none, so this only matters
    /// when the cookie moves to the browser.
    pub http_only: bool,
    /// The `SameSite` attribute; `None` when the cookie did not carry one,
    /// which browsers treat as `Lax`.
    #[serde(default)]
    pub same_site: Option<CookieSameSite>,
    /// When the cookie expires; `None` for a session cookie.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// When the cookie was first stored. Older cookies are sent first.
    pub created_at: DateTime<Utc>,
    /// When the cookie was last sent or stored.
    pub last_used_at: DateTime<Utc>,
}

impl HttpCookie {
    /// Whether the cookie has no expiry of its own and lives as long as the
    /// browsing session.
    pub const fn is_session(&self) -> bool {
        self.expires_at.is_none()
    }

    /// Whether the cookie has expired at `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|expires| expires <= now)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};

    use super::{CookieSameSite, HttpCookie};

    #[test]
    fn a_cookie_knows_whether_it_is_a_session_cookie_and_whether_it_expired() {
        let now = Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap();
        let mut cookie = HttpCookie {
            domain: "example.org".to_owned(),
            host_only: true,
            path: "/".to_owned(),
            name: "sid".to_owned(),
            value: "abc".to_owned(),
            secure: true,
            http_only: true,
            same_site: Some(CookieSameSite::Lax),
            expires_at: None,
            created_at: now,
            last_used_at: now,
        };

        assert!(cookie.is_session());
        assert!(!cookie.is_expired(now + Duration::days(4000)));

        cookie.expires_at = Some(now + Duration::hours(1));
        assert!(!cookie.is_session());
        assert!(!cookie.is_expired(now));
        assert!(cookie.is_expired(now + Duration::hours(1)));

        let json = serde_json::to_value(&cookie).unwrap();
        assert_eq!(json["same_site"], "lax");
        assert_eq!(serde_json::from_value::<HttpCookie>(json).unwrap(), cookie);
        assert_eq!(
            "none".parse::<CookieSameSite>().unwrap(),
            CookieSameSite::None
        );
        assert_eq!(CookieSameSite::Strict.as_str(), "strict");
    }
}
