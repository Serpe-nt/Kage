//! The rule for a declared identity (ADR 0013, spec §6.1): what an
//! operator may say about where an address is and what its persona
//! speaks. One rule for every declaration: an entry of the static list and
//! the host's own connection under `egress.mode: direct`.
//!
//! A declaration names a country (ISO 3166-1 alpha-2), and may name a
//! locale (BCP 47, `language-REGION` with the country as its region, which
//! overrides the country's primary language) and a time zone (IANA). Each
//! must be something a persona can be made from: a country the locale
//! tables know, or a locale whose language they know; a zone chrono-tz
//! knows. Anything else is refused, loudly, because an address in one
//! country speaking another's language is a measurable tell and a
//! declaration never falls back silently.

use crate::domain::{CountryCode, Engine};
use crate::persona::{locale, place};

/// The languages a declared locale may be in: those a browser language
/// list is known for.
pub const DECLARABLE_LANGUAGES: &str = "de, fr, it, en, nl, es, pl, pt, ro, sv";

/// Checks a declaration. `key` turns a field name into the name the
/// operator wrote it under (`egress.direct.locale`, or `entry 3: locale`).
///
/// # Errors
///
/// A sentence that names the first field that is wrong and what would be
/// right.
pub fn check(
    country: Option<CountryCode>,
    locale: Option<&str>,
    timezone: Option<&str>,
    key: &dyn Fn(&str) -> String,
) -> Result<(), String> {
    if let Some(tag) = locale {
        let Some(country) = country else {
            return Err(format!(
                "{} needs {}: a locale overrides the language of a country",
                key("locale"),
                key("country")
            ));
        };
        match region(tag) {
            Some(_) if locale::declared(tag, Engine::Chromium).is_none() => {
                return Err(format!(
                    "{}: no browser list is known for the language of {tag}; name one of \
                     {DECLARABLE_LANGUAGES}",
                    key("locale")
                ));
            }
            Some(region) if region == country.as_str() => {}
            Some(_) => {
                return Err(format!(
                    "{} must carry {} as its region, as xx-{country}",
                    key("locale"),
                    key("country")
                ));
            }
            None => {
                return Err(format!(
                    "{} is a BCP 47 tag of a language and a region, as de-AT",
                    key("locale")
                ));
            }
        }
    } else if let Some(country) = country
        && !locale::known_countries().contains(&country.as_str())
    {
        return Err(format!(
            "{}: no browser language list is known for {country} (known: {}); declare a {} \
             whose language is one of {DECLARABLE_LANGUAGES}, or leave {} out",
            key("country"),
            locale::known_countries().join(", "),
            key("locale"),
            key("country"),
        ));
    }
    if let Some(zone) = timezone {
        if country.is_none() {
            return Err(format!(
                "{} needs {}: a time zone places a declared country",
                key("timezone"),
                key("country")
            ));
        }
        if !place::is_zone(zone.trim()) || zone.trim() != zone {
            return Err(format!(
                "{}: {zone:?} is not an IANA time zone, as Europe/Vienna",
                key("timezone")
            ));
        }
    }
    Ok(())
}

/// The region of a `language-REGION` tag, upper-cased; `None` for anything
/// else.
fn region(locale: &str) -> Option<String> {
    let (language, region) = locale.split_once('-')?;
    let language_ok =
        (2..=3).contains(&language.len()) && language.bytes().all(|b| b.is_ascii_lowercase());
    let region_ok = region.len() == 2 && region.bytes().all(|b| b.is_ascii_alphabetic());
    (language_ok && region_ok).then(|| region.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::check;
    use crate::domain::CountryCode;

    /// A declaration, and what its refusal says.
    type Case<'a> = (
        Option<CountryCode>,
        Option<&'a str>,
        Option<&'a str>,
        &'a str,
    );

    #[allow(clippy::unnecessary_wraps)]
    fn country(code: &str) -> Option<CountryCode> {
        Some(code.parse().unwrap())
    }

    fn entry(field: &str) -> String {
        format!("entry 2: {field}")
    }

    #[test]
    fn a_known_country_alone_or_with_its_locale_and_zone_is_a_declaration() {
        assert_eq!(check(None, None, None, &entry), Ok(()));
        assert_eq!(check(country("AT"), None, None, &entry), Ok(()));
        for locale in ["de-AT", "en-AT", "de-at"] {
            assert_eq!(
                check(country("AT"), Some(locale), None, &entry),
                Ok(()),
                "{locale}"
            );
        }
        assert_eq!(
            check(country("US"), None, Some("America/Los_Angeles"), &entry),
            Ok(())
        );
        // A country the tables do not know is declared with a locale in a
        // language they know.
        assert_eq!(check(country("JP"), Some("en-JP"), None, &entry), Ok(()));
    }

    #[test]
    fn what_no_persona_can_be_made_from_is_refused_by_name() {
        let cases: &[Case<'_>] = &[
            (
                None,
                Some("de-AT"),
                None,
                "entry 2: locale needs entry 2: country",
            ),
            (
                country("AT"),
                Some("gsw-AT"),
                None,
                "no browser list is known",
            ),
            (
                country("AT"),
                Some("de-DE"),
                None,
                "must carry entry 2: country",
            ),
            (country("AT"), Some("de"), None, "BCP 47"),
            (country("AT"), Some("de-AUT"), None, "BCP 47"),
            (country("AT"), Some("DE-AT"), None, "BCP 47"),
            (country("AT"), Some("de_AT"), None, "BCP 47"),
            (
                country("JP"),
                None,
                None,
                "no browser language list is known for JP",
            ),
            (
                None,
                None,
                Some("Europe/Vienna"),
                "timezone needs entry 2: country",
            ),
            (
                country("AT"),
                None,
                Some("Europe/Atlantis"),
                "not an IANA time zone",
            ),
            (
                country("AT"),
                None,
                Some(" Europe/Vienna"),
                "not an IANA time zone",
            ),
            (country("AT"), None, Some(""), "not an IANA time zone"),
        ];
        for (country, locale, zone, needle) in cases {
            let message = check(*country, *locale, *zone, &entry).unwrap_err();
            assert!(message.contains(needle), "{needle}: {message}");
        }
        // The way out is named.
        let japan = check(country("JP"), None, None, &entry).unwrap_err();
        assert!(japan.contains("AT, BE"), "{japan}");
        assert!(japan.contains("or leave entry 2: country out"), "{japan}");
    }
}
