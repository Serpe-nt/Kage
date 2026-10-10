//! The locale and the languages of a persona (spec §6.1 step 1, §6.2).
//!
//! A persona speaks the list its browser ships with for the main language
//! of the country its address is in, not a list built from the country
//! code. The difference is what a real installation looks like:
//!
//! - Chrome takes its language list from its interface language, and its
//!   interface has one German, one French, one Italian. Measured on the
//!   pinned Chrome 155 with nothing driving it and a fresh profile, a
//!   system set to `de_AT` or `de_CH` gives `de-DE, de, en-US, en`, exactly
//!   as `de_DE` does; `fr_BE` and `fr_CA` give `fr-FR, fr, en-US, en`;
//!   `en_GB`, `en_IE` and `en_CA` give `en-GB, en-US, en`; `es_ES` gives
//!   `es-ES, es` with no English at all (milestone M6,
//!   `m6-evidence/experiments/langs.mjs`).
//! - Firefox is installed as a localized build with its own list: the
//!   German one has `de, en-US, en`, the French one `fr, fr-FR, en-US, en`,
//!   the British one `en-GB, en`. These are the values of Mozilla's
//!   localization files as known; no localized build was measured.
//!
//! English as a secondary language is therefore whatever the browser itself
//! puts there, with the weights the browser itself sends, and a bare `en`
//! is never appended to a list that has none.
//!
//! The locale tag carries the country (`de-AT`): it decides the keyboard
//! layout of the persona's display on both engines and the regional
//! formats in Firefox. The list carries the language.
//!
//! Countries with more than one language have a default and overrides by
//! the region the geo lookup names. A country without an entry falls back
//! to English (United States) with a locale of the country, and the caller
//! records a health event: nothing fails silently.

use crate::domain::{CountryCode, Engine};

/// A language a browser ships an interface or a localized build for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    German,
    French,
    Italian,
    /// English outside the United States.
    BritishEnglish,
    CanadianEnglish,
    AmericanEnglish,
    Dutch,
    Spanish,
    Polish,
    Portuguese,
    Romanian,
    Swedish,
}

impl Language {
    /// The language subtag of the locale tag.
    const fn subtag(self) -> &'static str {
        match self {
            Self::German => "de",
            Self::French => "fr",
            Self::Italian => "it",
            Self::BritishEnglish | Self::CanadianEnglish | Self::AmericanEnglish => "en",
            Self::Dutch => "nl",
            Self::Spanish => "es",
            Self::Polish => "pl",
            Self::Portuguese => "pt",
            Self::Romanian => "ro",
            Self::Swedish => "sv",
        }
    }

    /// What Chrome 155 on Linux chooses by itself for a system in this
    /// language (measured).
    const fn chrome(self) -> &'static [&'static str] {
        match self {
            Self::German => &["de-DE", "de", "en-US", "en"],
            Self::French => &["fr-FR", "fr", "en-US", "en"],
            Self::Italian => &["it-IT", "it", "en-US", "en"],
            Self::BritishEnglish | Self::CanadianEnglish => &["en-GB", "en-US", "en"],
            Self::AmericanEnglish => &["en-US", "en"],
            Self::Dutch => &["nl-NL", "nl", "en-US", "en"],
            Self::Spanish => &["es-ES", "es"],
            Self::Polish => &["pl-PL", "pl", "en-US", "en"],
            Self::Portuguese => &["pt-PT", "pt", "en-US", "en"],
            Self::Romanian => &["ro-RO", "ro", "en-US", "en"],
            Self::Swedish => &["sv-SE", "sv", "en-US", "en"],
        }
    }

    /// What the Firefox build of this language ships as its list.
    const fn firefox(self) -> &'static [&'static str] {
        match self {
            Self::German => &["de", "en-US", "en"],
            Self::French => &["fr", "fr-FR", "en-US", "en"],
            Self::Italian => &["it-IT", "it", "en-US", "en"],
            Self::BritishEnglish => &["en-GB", "en"],
            Self::CanadianEnglish => &["en-CA", "en-US", "en"],
            Self::AmericanEnglish => &["en-US", "en"],
            Self::Dutch => &["nl", "en-US", "en"],
            Self::Spanish => &["es-ES", "es", "en-US", "en"],
            Self::Polish => &["pl", "en-US", "en"],
            Self::Portuguese => &["pt-PT", "pt", "en"],
            Self::Romanian => &["ro-RO", "ro", "en-US", "en-GB", "en"],
            Self::Swedish => &["sv-SE", "sv", "en-US", "en"],
        }
    }
}

/// One country: its main language, and the regions where another one is.
struct Country {
    code: &'static str,
    language: Language,
    /// Regions, as geo lookups name them (lower case, no accents), where
    /// another language is the main one.
    regions: &'static [(&'static [&'static str], Language)],
}

const fn country(code: &'static str, language: Language) -> Country {
    Country {
        code,
        language,
        regions: &[],
    }
}

/// The countries Webshare stocked in Europe and North America on
/// 2026-10-09 (AT, BE, CA, DE, FR, GB, IT, RO, SE, US), the multilingual
/// neighbours of the pool's countries (CH, LU), and the countries whose
/// Chrome list was measured (ES, IE, NL, PL, PT).
const COUNTRIES: &[Country] = &[
    country("AT", Language::German),
    Country {
        code: "BE",
        language: Language::Dutch,
        regions: &[(
            &["brussels", "bruxelles", "wallonia", "wallonie"],
            Language::French,
        )],
    },
    Country {
        code: "CA",
        language: Language::CanadianEnglish,
        regions: &[(&["quebec"], Language::French)],
    },
    Country {
        code: "CH",
        language: Language::German,
        regions: &[
            (
                &[
                    "geneva",
                    "geneve",
                    "vaud",
                    "neuchatel",
                    "jura",
                    "fribourg",
                    "valais",
                ],
                Language::French,
            ),
            (&["ticino"], Language::Italian),
        ],
    },
    country("DE", Language::German),
    country("ES", Language::Spanish),
    country("FR", Language::French),
    country("GB", Language::BritishEnglish),
    country("IE", Language::BritishEnglish),
    country("IT", Language::Italian),
    country("LU", Language::French),
    country("NL", Language::Dutch),
    country("PL", Language::Polish),
    country("PT", Language::Portuguese),
    country("RO", Language::Romanian),
    country("SE", Language::Swedish),
    country("US", Language::AmericanEnglish),
];

/// The locale and languages of a persona.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonaLocale {
    /// The locale tag, with the country as its region: `de-AT`.
    pub locale: String,
    /// `navigator.languages` and `Accept-Language`, most preferred first.
    pub languages: Vec<String>,
    /// Whether the country has an entry in the table. `false` is the
    /// English fallback, which the caller reports.
    pub known: bool,
}

/// Lower case without the accents region names carry.
fn fold(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ä' | 'À' | 'Á' | 'Â' | 'Ä' => 'a',
            'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => 'i',
            'ò' | 'ó' | 'ô' | 'ö' | 'Ò' | 'Ó' | 'Ô' | 'Ö' => 'o',
            'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => 'u',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

/// The locale and the language list of a persona of `engine` whose address
/// is in `country`, in the region the geo lookup named, if any.
pub fn resolve(
    country: Option<CountryCode>,
    region: Option<&str>,
    engine: Engine,
) -> PersonaLocale {
    let list = |language: Language| -> Vec<String> {
        match engine {
            Engine::Chromium => language.chrome(),
            Engine::Firefox => language.firefox(),
        }
        .iter()
        .map(|&tag| tag.to_owned())
        .collect()
    };
    let Some(code) = country else {
        return PersonaLocale {
            locale: "en-US".to_owned(),
            languages: list(Language::AmericanEnglish),
            known: false,
        };
    };
    let Some(entry) = COUNTRIES.iter().find(|entry| entry.code == code.as_str()) else {
        return PersonaLocale {
            locale: format!("en-{code}"),
            languages: list(Language::AmericanEnglish),
            known: false,
        };
    };
    let region = region.map(fold);
    let language = region
        .as_deref()
        .and_then(|region| {
            entry
                .regions
                .iter()
                .find(|(names, _)| names.iter().any(|name| region.contains(name)))
                .map(|(_, language)| *language)
        })
        .unwrap_or(entry.language);
    PersonaLocale {
        locale: format!("{}-{code}", language.subtag()),
        languages: list(language),
        known: true,
    }
}

/// The language subtag of a locale tag: `de` of `de-AT`.
pub fn language_of(locale: &str) -> &str {
    locale.split('-').next().unwrap_or(locale)
}

/// The countries the table has an entry for.
pub fn known_countries() -> Vec<&'static str> {
    COUNTRIES.iter().map(|entry| entry.code).collect()
}

#[cfg(test)]
mod tests {
    use super::{COUNTRIES, PersonaLocale, known_countries, language_of, resolve};
    use crate::domain::{CountryCode, Engine};

    fn of(country: &str, region: Option<&str>, engine: Engine) -> PersonaLocale {
        resolve(
            Some(country.parse::<CountryCode>().unwrap()),
            region,
            engine,
        )
    }

    fn languages(country: &str, engine: Engine) -> String {
        of(country, None, engine).languages.join(",")
    }

    /// The table of milestone M6's consult 4 for the pool's five countries.
    #[test]
    fn the_pools_countries_speak_what_their_browsers_ship_with() {
        for (country, locale, chromium, firefox) in [
            ("DE", "de-DE", "de-DE,de,en-US,en", "de,en-US,en"),
            ("AT", "de-AT", "de-DE,de,en-US,en", "de,en-US,en"),
            ("FR", "fr-FR", "fr-FR,fr,en-US,en", "fr,fr-FR,en-US,en"),
            ("IT", "it-IT", "it-IT,it,en-US,en", "it-IT,it,en-US,en"),
            ("GB", "en-GB", "en-GB,en-US,en", "en-GB,en"),
        ] {
            for engine in [Engine::Chromium, Engine::Firefox] {
                let resolved = of(country, Some("Anywhere"), engine);
                assert_eq!(resolved.locale, locale, "{country}");
                assert!(resolved.known, "{country}");
            }
            assert_eq!(languages(country, Engine::Chromium), chromium, "{country}");
            assert_eq!(languages(country, Engine::Firefox), firefox, "{country}");
        }
    }

    #[test]
    fn an_austrian_chrome_is_a_german_one_in_its_languages_only() {
        let austria = of("AT", Some("Vienna"), Engine::Chromium);
        let germany = of("DE", Some("Berlin"), Engine::Chromium);

        assert_eq!(austria.languages, germany.languages);
        // The country is in the locale: the keyboard and the formats.
        assert_eq!(austria.locale, "de-AT");
        assert_eq!(germany.locale, "de-DE");
    }

    #[test]
    fn a_region_decides_in_a_country_of_several_languages() {
        for (country, region, locale) in [
            ("CH", None, "de-CH"),
            ("CH", Some("Zurich"), "de-CH"),
            ("CH", Some("Geneva"), "fr-CH"),
            ("CH", Some("Genève"), "fr-CH"),
            ("CH", Some("Canton of Vaud"), "fr-CH"),
            ("CH", Some("Neuchâtel"), "fr-CH"),
            ("CH", Some("Ticino"), "it-CH"),
            ("BE", None, "nl-BE"),
            ("BE", Some("Flanders"), "nl-BE"),
            ("BE", Some("Brussels Capital"), "fr-BE"),
            ("BE", Some("Wallonia"), "fr-BE"),
            ("CA", Some("Ontario"), "en-CA"),
            ("CA", Some("Quebec"), "fr-CA"),
            ("CA", Some("Québec"), "fr-CA"),
            ("LU", Some("Luxembourg"), "fr-LU"),
        ] {
            assert_eq!(
                of(country, region, Engine::Firefox).locale,
                locale,
                "{country} {region:?}"
            );
        }
        // The list is the language's, whatever the country.
        assert_eq!(
            of("CH", Some("Geneva"), Engine::Chromium).languages,
            of("FR", None, Engine::Chromium).languages
        );
        assert_eq!(
            of("CA", Some("Quebec"), Engine::Firefox).languages,
            ["fr", "fr-FR", "en-US", "en"]
        );
        assert_eq!(
            of("CA", Some("Ontario"), Engine::Firefox).languages,
            ["en-CA", "en-US", "en"]
        );
        // Chrome has one English outside the United States.
        assert_eq!(
            of("CA", Some("Ontario"), Engine::Chromium).languages,
            ["en-GB", "en-US", "en"]
        );
    }

    #[test]
    fn english_is_never_added_to_a_list_that_ships_without_it() {
        assert_eq!(languages("ES", Engine::Chromium), "es-ES,es");
        // No table entry ends in a language the browser does not ship
        // there, and none is empty or repeats a tag.
        for entry in COUNTRIES {
            for engine in [Engine::Chromium, Engine::Firefox] {
                let resolved = of(entry.code, None, engine);
                assert!(!resolved.languages.is_empty(), "{}", entry.code);
                let mut unique = resolved.languages.clone();
                unique.sort();
                unique.dedup();
                assert_eq!(unique.len(), resolved.languages.len(), "{}", entry.code);
                // The first language is the locale's language.
                assert_eq!(
                    language_of(&resolved.languages[0]),
                    language_of(&resolved.locale),
                    "{}",
                    entry.code
                );
                // Every locale has a region: the Firefox engine needs one.
                assert_eq!(
                    resolved.locale.split('-').nth(1),
                    Some(entry.code),
                    "{}",
                    entry.code
                );
            }
        }
    }

    #[test]
    fn a_country_without_an_entry_falls_back_to_english_and_says_so() {
        let unknown = of("JP", Some("Tokyo"), Engine::Chromium);
        assert_eq!(
            unknown,
            PersonaLocale {
                locale: "en-JP".to_owned(),
                languages: vec!["en-US".to_owned(), "en".to_owned()],
                known: false,
            }
        );
        let nowhere = resolve(None, None, Engine::Firefox);
        assert_eq!(nowhere.locale, "en-US");
        assert_eq!(nowhere.languages, ["en-US", "en"]);
        assert!(!nowhere.known);
    }

    #[test]
    fn the_table_covers_what_the_provider_stocks_near_the_pool() {
        let known = known_countries();
        // Webshare's stock in Europe and North America on 2026-10-09.
        for country in ["AT", "BE", "CA", "DE", "FR", "GB", "IT", "RO", "SE", "US"] {
            assert!(known.contains(&country), "{country}");
        }
        let mut sorted = known.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), known.len());
        assert_eq!(language_of("de-AT"), "de");
        assert_eq!(language_of("en"), "en");
    }
}
