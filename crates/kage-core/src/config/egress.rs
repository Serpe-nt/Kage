//! Configuration of egress (spec §25, ADR 0012, decision D50): which
//! connection carries the traffic, where level 0 leaves from, and the
//! persona that stands for the host's own connection.
//!
//! There is no mixed mode. Under `proxies` nothing leaves the host's own
//! address; under `direct` nothing goes through the pool. A visitor that
//! moves from a residential address to the host's has shown a site a bot,
//! so the two are never used for one site by turns, and the configuration
//! refuses both crossings of `egress.plain`.

use serde::{Deserialize, Serialize};

use crate::domain::{CountryCode, Engine, Exit};
use crate::macros::string_enum;
use crate::policy::normalize_host;

/// The most personas that may stand for the host's own connection: a few
/// households behind one address, never a pool.
pub const MAX_DIRECT_PERSONAS: u32 = 4;

/// The most names `egress.direct.private_names` may hold.
pub const MAX_PRIVATE_NAMES: usize = 64;

/// The longest `User-Agent` an operator may give the plain client.
pub const MAX_PLAIN_USER_AGENT: usize = 256;

/// The `User-Agent` of the plain client unless the operator or a request
/// says otherwise: a client that says who it is.
pub const DEFAULT_PLAIN_USER_AGENT: &str = concat!("kage/", env!("CARGO_PKG_VERSION"));

string_enum! {
    /// Which connection carries the traffic of every persona.
    #[derive(Default)]
    pub enum EgressMode {
        /// The proxy pool, as in 1.0: one persona per proxy.
        #[default]
        Proxies => "proxies",
        /// The host's own connection, with the personas of
        /// `egress.direct.personas` standing for it.
        Direct => "direct",
    }
}

/// Egress: the operator's choice, global.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressConfig {
    /// `proxies` (the default) or `direct`.
    #[serde(default)]
    pub mode: EgressMode,
    /// Where a level-0 request leaves from: `pool` under `proxies` (the
    /// site's one plain exit, its scout's proxy) and `host` under
    /// `direct`. Unset, the one the mode implies; set, it must be that one.
    #[serde(default)]
    pub plain: Option<Exit>,
    /// The `User-Agent` of a level-0 request that names none; unset,
    /// `kage/<version>`. A deployment may give its own, for example with a
    /// contact address: `kage/2.0.0 (+https://example.org/bot)`.
    #[serde(default)]
    pub plain_user_agent: Option<String>,
    /// The personas of the host's own connection, under `direct`.
    #[serde(default)]
    pub direct: DirectEgressConfig,
}

/// The persona, or few personas, that stand for the host's own connection
/// under `egress.mode: direct`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectEgressConfig {
    /// How many personas stand for the host (1 to 4).
    #[serde(default = "default_direct_personas")]
    pub personas: u32,
    /// The country the host persona claims. Declared values win; a geo
    /// lookup from the host then only verifies them. Unset, the first
    /// lookup that answers decides.
    #[serde(default)]
    pub country: Option<CountryCode>,
    /// The persona's locale, a BCP 47 tag with the country as its region
    /// (`de-AT`); it overrides the country's primary language. Needs
    /// `country`.
    #[serde(default)]
    pub locale: Option<String>,
    /// The engines the host personas may be drawn on; unset, the mix of
    /// `personas.mix`.
    #[serde(default)]
    pub engines: Option<Vec<Engine>>,
    /// Exact host names that may resolve to a private or loopback address
    /// under `direct` (the operator's own network). Link-local, metadata
    /// and multicast addresses are refused for every name.
    #[serde(default)]
    pub private_names: Vec<String>,
}

const fn default_direct_personas() -> u32 {
    1
}

impl Default for DirectEgressConfig {
    fn default() -> Self {
        Self {
            personas: default_direct_personas(),
            country: None,
            locale: None,
            engines: None,
            private_names: Vec::new(),
        }
    }
}

impl EgressConfig {
    /// Where level 0 leaves from: as set, or the one the mode implies.
    pub fn plain_exit(&self) -> Exit {
        self.plain.unwrap_or(match self.mode {
            EgressMode::Proxies => Exit::Pool,
            EgressMode::Direct => Exit::Host,
        })
    }

    /// The `User-Agent` of a level-0 request that names none.
    pub fn plain_user_agent(&self) -> &str {
        self.plain_user_agent
            .as_deref()
            .unwrap_or(DEFAULT_PLAIN_USER_AGENT)
    }

    /// Whether a name may resolve to a private or loopback address under
    /// `direct`.
    pub fn private_name(&self, host: &str) -> bool {
        normalize_host(host).is_some_and(|host| {
            self.direct
                .private_names
                .iter()
                .any(|name| normalize_host(name).as_deref() == Some(host.as_str()))
        })
    }
}

/// Checks the section.
///
/// # Errors
///
/// A sentence that names the first key that is wrong.
pub(super) fn validate(egress: &EgressConfig) -> Result<(), String> {
    match (egress.mode, egress.plain) {
        (EgressMode::Proxies, Some(Exit::Host)) => {
            return Err(
                "egress.plain host is refused under egress.mode proxies: under proxies nothing \
                 leaves the host's own address (ADR 0012); set egress.mode direct for that"
                    .to_owned(),
            );
        }
        (EgressMode::Direct, Some(Exit::Pool)) => {
            return Err(
                "egress.plain pool is refused under egress.mode direct: the pool and the host's \
                 own address would be used for one site by turns (ADR 0012)"
                    .to_owned(),
            );
        }
        _ => {}
    }
    if let Some(agent) = &egress.plain_user_agent {
        let printable = agent.bytes().all(|byte| (0x20..0x7f).contains(&byte));
        if agent.trim().is_empty() || agent.len() > MAX_PLAIN_USER_AGENT || !printable {
            return Err(format!(
                "egress.plain_user_agent is 1 to {MAX_PLAIN_USER_AGENT} printable ASCII \
                 characters"
            ));
        }
    }
    let direct = &egress.direct;
    if direct.personas == 0 || direct.personas > MAX_DIRECT_PERSONAS {
        return Err(format!(
            "egress.direct.personas must be between 1 and {MAX_DIRECT_PERSONAS}"
        ));
    }
    if let Some(locale) = &direct.locale {
        let Some(country) = direct.country else {
            return Err(
                "egress.direct.locale needs egress.direct.country: a locale overrides the \
                 language of a country"
                    .to_owned(),
            );
        };
        match locale_region(locale) {
            Some(_) if crate::persona::locale::declared(locale, Engine::Chromium).is_none() => {
                return Err(format!(
                    "egress.direct.locale: no browser list is known for the language of \
                     {locale}; name one of de, fr, it, en, nl, es, pl, pt, ro, sv"
                ));
            }
            Some(region) if region == country.as_str() => {}
            Some(_) => {
                return Err(format!(
                    "egress.direct.locale must carry egress.direct.country as its region, as \
                     xx-{country}"
                ));
            }
            None => {
                return Err(
                    "egress.direct.locale is a BCP 47 tag of a language and a region, as de-AT"
                        .to_owned(),
                );
            }
        }
    }
    if let Some(engines) = &direct.engines {
        let mut sorted = engines.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if engines.is_empty() || sorted.len() != engines.len() {
            return Err(
                "egress.direct.engines names at least one engine, each once; omit it for the \
                 mix of personas.mix"
                    .to_owned(),
            );
        }
    }
    if direct.private_names.len() > MAX_PRIVATE_NAMES {
        return Err(format!(
            "egress.direct.private_names holds at most {MAX_PRIVATE_NAMES} names"
        ));
    }
    for name in &direct.private_names {
        // A scheme and a port bring a colon, and so does an IPv6 address,
        // which the host parser would take; a path is no host to it.
        if name.contains(':')
            || normalize_host(name).is_none()
            || name.parse::<std::net::IpAddr>().is_ok()
        {
            return Err(format!(
                "egress.direct.private_names: {name:?} is not a host name; write the name only, \
                 without a scheme, a port or a path"
            ));
        }
    }
    Ok(())
}

/// The region of a `language-REGION` tag, upper-cased; `None` for anything
/// else.
fn locale_region(locale: &str) -> Option<String> {
    let (language, region) = locale.split_once('-')?;
    let language_ok =
        (2..=3).contains(&language.len()) && language.bytes().all(|b| b.is_ascii_lowercase());
    let region_ok = region.len() == 2 && region.bytes().all(|b| b.is_ascii_alphabetic());
    (language_ok && region_ok).then(|| region.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_PLAIN_USER_AGENT, DirectEgressConfig, EgressConfig, EgressMode,
        MAX_DIRECT_PERSONAS, MAX_PRIVATE_NAMES, validate,
    };
    use crate::domain::{Engine, Exit};

    fn with(change: impl FnOnce(&mut EgressConfig)) -> Result<(), String> {
        let mut egress = EgressConfig::default();
        change(&mut egress);
        validate(&egress)
    }

    #[test]
    fn the_defaults_are_the_pool_and_one_host_persona_unused() {
        let egress = EgressConfig::default();
        assert_eq!(egress.mode, EgressMode::Proxies);
        assert_eq!(egress.plain, None);
        assert_eq!(egress.plain_exit(), Exit::Pool);
        assert_eq!(egress.plain_user_agent(), DEFAULT_PLAIN_USER_AGENT);
        assert!(DEFAULT_PLAIN_USER_AGENT.starts_with("kage/"));
        assert_eq!(egress.direct, DirectEgressConfig::default());
        assert_eq!(egress.direct.personas, 1);
        assert_eq!(validate(&egress), Ok(()));
        assert_eq!(serde_json::from_str::<EgressConfig>("{}").unwrap(), egress);
        assert_eq!(
            EgressMode::ALL
                .iter()
                .map(|mode| mode.as_str())
                .collect::<Vec<_>>(),
            ["proxies", "direct"]
        );
        assert!(serde_json::from_str::<EgressConfig>(r#"{"mode":"mixed"}"#).is_err());
        assert!(serde_json::from_str::<EgressConfig>(r#"{"plain":"proxies"}"#).is_err());
        assert!(serde_json::from_str::<EgressConfig>(r#"{"direct":{"port":1}}"#).is_err());
    }

    #[test]
    fn the_plain_exit_follows_the_mode_and_neither_crossing_is_allowed() {
        let direct = EgressConfig {
            mode: EgressMode::Direct,
            ..EgressConfig::default()
        };
        assert_eq!(direct.plain_exit(), Exit::Host);
        assert_eq!(validate(&direct), Ok(()));
        // Written out, the one the mode implies is fine.
        for (mode, plain) in [
            (EgressMode::Proxies, Exit::Pool),
            (EgressMode::Direct, Exit::Host),
        ] {
            assert_eq!(
                with(|egress| {
                    egress.mode = mode;
                    egress.plain = Some(plain);
                }),
                Ok(())
            );
        }
        let host = with(|egress| egress.plain = Some(Exit::Host)).unwrap_err();
        assert!(
            host.contains("egress.plain host is refused under egress.mode proxies"),
            "{host}"
        );
        let pool = with(|egress| {
            egress.mode = EgressMode::Direct;
            egress.plain = Some(Exit::Pool);
        })
        .unwrap_err();
        assert!(
            pool.contains("egress.plain pool is refused under egress.mode direct"),
            "{pool}"
        );
    }

    #[test]
    fn the_plain_user_agent_is_short_printable_text() {
        let own = EgressConfig {
            plain_user_agent: Some("kage/2.0.0 (+https://example.org/bot)".to_owned()),
            ..EgressConfig::default()
        };
        assert_eq!(validate(&own), Ok(()));
        assert_eq!(
            own.plain_user_agent(),
            "kage/2.0.0 (+https://example.org/bot)"
        );
        let longest = "a".repeat(256);
        assert_eq!(
            with(|egress| egress.plain_user_agent = Some(longest)),
            Ok(())
        );
        for bad in [
            "",
            "   ",
            "line\nbreak",
            "tab\there",
            "ümlaut",
            &"a".repeat(257),
        ] {
            assert!(
                with(|egress| egress.plain_user_agent = Some(bad.to_owned())).is_err(),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_host_personas_are_few_and_say_where_they_are() {
        for personas in 1..=MAX_DIRECT_PERSONAS {
            assert_eq!(with(|egress| egress.direct.personas = personas), Ok(()));
        }
        for personas in [0, MAX_DIRECT_PERSONAS + 1] {
            assert!(with(|egress| egress.direct.personas = personas).is_err());
        }
        assert_eq!(MAX_DIRECT_PERSONAS, 4);
        // A country alone, or with a locale of that country.
        assert_eq!(
            with(|egress| egress.direct.country = Some("AT".parse().unwrap())),
            Ok(())
        );
        for locale in ["de-AT", "en-AT", "de-at"] {
            assert_eq!(
                with(|egress| {
                    egress.direct.country = Some("AT".parse().unwrap());
                    egress.direct.locale = Some(locale.to_owned());
                }),
                Ok(()),
                "{locale}"
            );
        }
        // A language no browser list is known for says nothing a browser
        // could ship.
        let swiss_german = with(|egress| {
            egress.direct.country = Some("AT".parse().unwrap());
            egress.direct.locale = Some("gsw-AT".to_owned());
        })
        .unwrap_err();
        assert!(swiss_german.contains("no browser list"), "{swiss_german}");
        // A locale needs the country, and must carry it.
        let alone = with(|egress| egress.direct.locale = Some("de-AT".to_owned())).unwrap_err();
        assert!(alone.contains("needs egress.direct.country"), "{alone}");
        // A region is two letters: three are no region, not another one.
        let three = with(|egress| {
            egress.direct.country = Some("AT".parse().unwrap());
            egress.direct.locale = Some("de-AUT".to_owned());
        })
        .unwrap_err();
        assert!(three.contains("BCP 47"), "{three}");
        for locale in ["de-DE", "de", "DE-AT", "deutsch-AT", "de_AT", "de-AUT", ""] {
            assert!(
                with(|egress| {
                    egress.direct.country = Some("AT".parse().unwrap());
                    egress.direct.locale = Some(locale.to_owned());
                })
                .is_err(),
                "{locale}"
            );
        }
        // Engines: at least one, each once.
        assert_eq!(
            with(|egress| egress.direct.engines = Some(vec![Engine::Firefox])),
            Ok(())
        );
        for engines in [vec![], vec![Engine::Chromium, Engine::Chromium]] {
            assert!(with(|egress| egress.direct.engines = Some(engines)).is_err());
        }
    }

    #[test]
    fn private_names_are_names_and_only_those_names() {
        let egress = EgressConfig {
            direct: DirectEgressConfig {
                private_names: vec!["fixture.kage.test".to_owned(), "NAS.Home.Arpa.".to_owned()],
                ..DirectEgressConfig::default()
            },
            ..EgressConfig::default()
        };
        assert_eq!(validate(&egress), Ok(()));
        assert!(egress.private_name("fixture.kage.test"));
        assert!(egress.private_name("FIXTURE.kage.test."));
        assert!(egress.private_name("nas.home.arpa"));
        assert!(!egress.private_name("www.fixture.kage.test"));
        assert!(!egress.private_name("kage.test"));
        assert!(!EgressConfig::default().private_name("fixture.kage.test"));
        for bad in [
            "http://fixture.kage.test",
            "fixture.kage.test:8088",
            "fixture.kage.test/path",
            "10.0.0.5",
            "[::1]",
            "",
        ] {
            assert!(
                with(|egress| egress.direct.private_names = vec![bad.to_owned()]).is_err(),
                "{bad:?}"
            );
        }
        let many = (0..=MAX_PRIVATE_NAMES)
            .map(|n| format!("h{n}.test"))
            .collect();
        assert!(with(|egress| egress.direct.private_names = many).is_err());
        let enough = (0..MAX_PRIVATE_NAMES)
            .map(|n| format!("h{n}.test"))
            .collect();
        assert_eq!(with(|egress| egress.direct.private_names = enough), Ok(()));
    }
}
