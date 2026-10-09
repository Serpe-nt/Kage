//! Cookies between the sidecar contract and Kage's `http_cookies` rows
//! (spec §6.4).
//!
//! The contract's cookie has the fields of a row of `http_cookies`
//! ([`HttpCookie`]) except the two bookkeeping times, which are Kage's own:
//! a browser does not tell when it stored or last sent a cookie. A cookie
//! that comes out of the browser gets the time of the export for both; a
//! row that goes into the browser simply loses them.

use chrono::{DateTime, Utc};
use kage_core::domain::{CookieSameSite, HttpCookie};

use crate::contract::{Cookie, SameSite};

const fn same_site_to_row(same_site: SameSite) -> CookieSameSite {
    match same_site {
        SameSite::Strict => CookieSameSite::Strict,
        SameSite::Lax => CookieSameSite::Lax,
        SameSite::None => CookieSameSite::None,
    }
}

const fn same_site_to_contract(same_site: CookieSameSite) -> SameSite {
    match same_site {
        CookieSameSite::Strict => SameSite::Strict,
        CookieSameSite::Lax => SameSite::Lax,
        CookieSameSite::None => SameSite::None,
    }
}

/// A cookie the sidecar exported, as a row of `http_cookies`.
///
/// `now` becomes the row's `created_at` and `last_used_at`: the moment Kage
/// learned of the cookie.
pub fn to_row(cookie: Cookie, now: DateTime<Utc>) -> HttpCookie {
    HttpCookie {
        domain: cookie.domain.trim_start_matches('.').to_ascii_lowercase(),
        host_only: cookie.host_only,
        path: cookie.path,
        name: cookie.name,
        value: cookie.value,
        secure: cookie.secure,
        http_only: cookie.http_only,
        same_site: cookie.same_site.map(same_site_to_row),
        expires_at: cookie.expires_at,
        created_at: now,
        last_used_at: now,
    }
}

/// A row of `http_cookies` as the sidecar takes it for an import.
pub fn from_row(row: &HttpCookie) -> Cookie {
    Cookie {
        domain: row.domain.clone(),
        host_only: row.host_only,
        path: row.path.clone(),
        name: row.name.clone(),
        value: row.value.clone(),
        secure: row.secure,
        http_only: row.http_only,
        same_site: row.same_site.map(same_site_to_contract),
        expires_at: row.expires_at,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use kage_core::domain::{CookieSameSite, HttpCookie};

    use super::{from_row, to_row};
    use crate::contract::{Cookie, SameSite};

    fn row() -> HttpCookie {
        let stored = Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap();
        HttpCookie {
            domain: "example.org".to_owned(),
            host_only: false,
            path: "/".to_owned(),
            name: "sid".to_owned(),
            value: "abc".to_owned(),
            secure: true,
            http_only: true,
            same_site: Some(CookieSameSite::Lax),
            expires_at: Some(Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap()),
            created_at: stored,
            last_used_at: stored,
        }
    }

    #[test]
    fn a_row_goes_into_the_browser_and_comes_back_the_same_cookie() {
        let now = Utc.with_ymd_and_hms(2026, 10, 9, 12, 0, 0).unwrap();
        let original = row();
        let back = to_row(from_row(&original), now);
        // Everything that makes the cookie is unchanged ...
        assert_eq!(
            HttpCookie {
                created_at: original.created_at,
                last_used_at: original.last_used_at,
                ..back.clone()
            },
            original
        );
        // ... and the bookkeeping times are the time of the export.
        assert_eq!((back.created_at, back.last_used_at), (now, now));
    }

    #[test]
    fn every_same_site_value_and_its_absence_survive_both_ways() {
        let now = Utc::now();
        for (row_value, contract_value) in [
            (Some(CookieSameSite::Strict), Some(SameSite::Strict)),
            (Some(CookieSameSite::Lax), Some(SameSite::Lax)),
            (Some(CookieSameSite::None), Some(SameSite::None)),
            (None, None),
        ] {
            let cookie = from_row(&HttpCookie {
                same_site: row_value,
                ..row()
            });
            assert_eq!(cookie.same_site, contract_value);
            assert_eq!(to_row(cookie, now).same_site, row_value);
        }
    }

    #[test]
    fn a_session_cookie_stays_one_and_a_dotted_or_upper_case_domain_is_normalized() {
        let now = Utc::now();
        let exported = Cookie {
            domain: ".Example.ORG".to_owned(),
            host_only: true,
            path: "/account".to_owned(),
            name: "theme".to_owned(),
            value: String::new(),
            secure: false,
            http_only: false,
            same_site: None,
            expires_at: None,
        };
        let stored = to_row(exported, now);
        assert_eq!(stored.domain, "example.org");
        assert!(stored.is_session());
        assert!(stored.host_only);
        assert_eq!(stored.path, "/account");
        assert_eq!(from_row(&stored).expires_at, None);
    }

    #[test]
    fn the_contract_writes_a_cookie_with_the_field_names_of_the_table() {
        let json = serde_json::to_value(from_row(&row())).unwrap();
        let mut names: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "domain",
                "expires_at",
                "host_only",
                "http_only",
                "name",
                "path",
                "same_site",
                "secure",
                "value"
            ]
        );
        assert_eq!(json["same_site"], "lax");
        assert_eq!(json["expires_at"], "2027-01-01T00:00:00Z");
    }
}
