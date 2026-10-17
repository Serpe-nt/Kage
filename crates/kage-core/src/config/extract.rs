//! Configuration of extraction, of `robots.txt` and of the response cache
//! (spec §14, §15).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::duration::HumanDuration;

/// The most bytes of a `robots.txt` that may ever be read.
pub const MAX_ROBOTS_BYTES: u64 = 4 * 1024 * 1024;

/// The most product tokens Kage may answer to in a `robots.txt`.
pub const MAX_ROBOTS_AGENTS: usize = 16;

/// The longest `robots.scout_quarantine` may be: ten years, which is
/// never for a pool.
pub const MAX_SCOUT_QUARANTINE: Duration = Duration::from_hours(3650 * 24);

/// Extraction: what `include: ["extract"]` returns (spec §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractConfig {
    /// A page whose main content has fewer words than this gave its reader
    /// next to nothing: `thin_content` in its result.
    #[serde(default = "default_thin_words")]
    pub thin_words: u32,
}

const fn default_thin_words() -> u32 {
    80
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            thin_words: default_thin_words(),
        }
    }
}

/// `robots.txt`: how long an answer is kept, and whom Kage answers to
/// (spec §14). Whether a site's file is asked for at all is its policy's
/// (`robots: ignore | report | enforce`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RobotsConfig {
    /// How long a site's `robots.txt` is kept before it is asked for
    /// again, when a request needs it. Moved by up to a tenth per site so
    /// that the requests fall on no grid.
    #[serde(default = "default_ttl")]
    pub ttl: HumanDuration,
    /// How long a site is left alone after its `robots.txt` could not be
    /// had (a server error, a timeout, a block).
    #[serde(default = "default_error_ttl")]
    pub error_ttl: HumanDuration,
    /// How much of a `robots.txt` is read, in bytes; what a file has beyond
    /// is ignored. Google reads 500 kibibytes.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    /// The product tokens Kage answers to in a `robots.txt`, besides `*`:
    /// a group that names one of them applies in place of the group for
    /// everybody. Letters, `-` and `_`.
    #[serde(default = "default_agents")]
    pub agents: Vec<String>,
    /// How long a persona that read a page of a site is kept from being
    /// its scout: one address that both read the site and asked it for
    /// its `robots.txt` is the price of a site that would otherwise never
    /// have a scout again once every persona has read it.
    #[serde(default = "default_scout_quarantine")]
    pub scout_quarantine: HumanDuration,
}

const fn default_ttl() -> HumanDuration {
    HumanDuration::from_hours(24)
}

const fn default_error_ttl() -> HumanDuration {
    HumanDuration::from_hours(1)
}

const fn default_max_bytes() -> u64 {
    512 * 1024
}

fn default_agents() -> Vec<String> {
    vec!["kage".to_owned()]
}

const fn default_scout_quarantine() -> HumanDuration {
    HumanDuration::from_hours(30 * 24)
}

impl Default for RobotsConfig {
    fn default() -> Self {
        Self {
            ttl: default_ttl(),
            error_ttl: default_error_ttl(),
            max_bytes: default_max_bytes(),
            agents: default_agents(),
            scout_quarantine: default_scout_quarantine(),
        }
    }
}

/// The response cache (spec §15). Whether a site's pages are served from
/// it, to whom and for how long is its policy's (`cache`); this is the
/// bound no policy goes beyond.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// The longest a result may be served from the cache, whatever a
    /// policy or a request says. Never longer than results are kept
    /// (`artifacts.retention_days`).
    #[serde(default = "default_max_ttl")]
    pub max_ttl: HumanDuration,
}

const fn default_max_ttl() -> HumanDuration {
    HumanDuration::from_hours(24)
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_ttl: default_max_ttl(),
        }
    }
}

/// Checks the three sections.
///
/// # Errors
///
/// A sentence that names the first key that is out of range.
pub(super) fn validate(
    extract: ExtractConfig,
    robots: &RobotsConfig,
    cache: &CacheConfig,
    retention_days: u32,
) -> Result<(), String> {
    if extract.thin_words == 0 || extract.thin_words > 10_000 {
        return Err("extract.thin_words must be between 1 and 10000".to_owned());
    }
    if robots.ttl.get() < Duration::from_mins(1) {
        return Err(
            "robots.ttl must be at least 1m: a site is asked for its robots.txt once per ttl"
                .to_owned(),
        );
    }
    if robots.error_ttl.get() < Duration::from_mins(1) {
        return Err("robots.error_ttl must be at least 1m".to_owned());
    }
    if robots.max_bytes == 0 || robots.max_bytes > MAX_ROBOTS_BYTES {
        return Err(format!(
            "robots.max_bytes must be between 1 and {MAX_ROBOTS_BYTES}"
        ));
    }
    if robots.agents.len() > MAX_ROBOTS_AGENTS {
        return Err(format!(
            "robots.agents holds more than {MAX_ROBOTS_AGENTS} tokens"
        ));
    }
    if let Some(agent) = robots.agents.iter().find(|agent| {
        agent.is_empty()
            || agent.len() > 64
            || !agent
                .bytes()
                .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'-' | b'_'))
    }) {
        return Err(format!(
            "robots.agents holds {agent:?}: a product token is 1 to 64 letters, hyphens and \
             underscores"
        ));
    }
    let quarantine = robots.scout_quarantine.get();
    if quarantine < Duration::from_hours(24) || quarantine > MAX_SCOUT_QUARANTINE {
        return Err(
            "robots.scout_quarantine must be between 1d and 3650d: a persona that read a site              is kept from scouting it for that long"
                .to_owned(),
        );
    }
    if cache.max_ttl.get() < Duration::from_secs(1) {
        return Err("cache.max_ttl must be at least 1s".to_owned());
    }
    if cache.max_ttl.get() > Duration::from_hours(u64::from(retention_days) * 24) {
        return Err(
            "cache.max_ttl must not be longer than artifacts.retention_days: an entry cannot \
             outlive the result it serves"
                .to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{CacheConfig, ExtractConfig, MAX_ROBOTS_BYTES, RobotsConfig, validate};
    use crate::duration::HumanDuration;

    type Sections = (ExtractConfig, RobotsConfig, CacheConfig);

    fn check(change: impl FnOnce(&mut Sections), retention_days: u32) -> Result<(), String> {
        let mut sections = Sections::default();
        change(&mut sections);
        validate(sections.0, &sections.1, &sections.2, retention_days)
    }

    #[test]
    fn the_defaults_are_what_the_spec_says_and_valid() {
        let (extract, robots, cache) = Sections::default();
        assert_eq!(extract.thin_words, 80);
        assert_eq!(robots.ttl.get(), Duration::from_hours(24));
        assert_eq!(robots.error_ttl.get(), Duration::from_hours(1));
        assert_eq!(robots.max_bytes, 512 * 1024);
        assert_eq!(robots.agents, ["kage"]);
        assert_eq!(robots.scout_quarantine.get(), Duration::from_hours(720));
        assert_eq!(cache.max_ttl.get(), Duration::from_hours(24));
        assert_eq!(check(|_| {}, 14), Ok(()));
        // An empty section is the defaults, and an unknown key is refused.
        assert_eq!(
            serde_json::from_str::<ExtractConfig>("{}").unwrap(),
            extract
        );
        assert_eq!(serde_json::from_str::<RobotsConfig>("{}").unwrap(), robots);
        assert_eq!(serde_json::from_str::<CacheConfig>("{}").unwrap(), cache);
        assert!(serde_json::from_str::<ExtractConfig>(r#"{"words":3}"#).is_err());
        assert!(serde_json::from_str::<RobotsConfig>(r#"{"agent":"x"}"#).is_err());
        assert!(serde_json::from_str::<CacheConfig>(r#"{"ttl":"1h"}"#).is_err());
    }

    #[test]
    fn no_configuration_reads_more_than_four_mebibytes_of_a_robots_txt() {
        // The number itself: every other test names the constant, and a
        // constant that is wrong moves with them.
        assert_eq!(MAX_ROBOTS_BYTES, 4_194_304);
        assert_eq!(
            check(|sections| sections.1.max_bytes = 4_194_304, 14),
            Ok(())
        );
        assert!(check(|sections| sections.1.max_bytes = 4_194_305, 14).is_err());
        // What is read unless said is an eighth of it.
        assert_eq!(RobotsConfig::default().max_bytes * 8, MAX_ROBOTS_BYTES);
    }

    #[test]
    fn every_key_out_of_range_is_named() {
        type Change = fn(&mut Sections);
        let cases: [(Change, &str); 15] = [
            (|sections| sections.0.thin_words = 0, "extract.thin_words"),
            (
                |sections| sections.0.thin_words = 10_001,
                "extract.thin_words",
            ),
            (
                |sections| sections.1.ttl = HumanDuration::from_secs(59),
                "robots.ttl",
            ),
            (
                |sections| sections.1.error_ttl = HumanDuration::from_secs(59),
                "robots.error_ttl",
            ),
            (|sections| sections.1.max_bytes = 0, "robots.max_bytes"),
            (
                |sections| sections.1.max_bytes = MAX_ROBOTS_BYTES + 1,
                "robots.max_bytes",
            ),
            (
                |sections| sections.1.agents = vec!["a".to_owned(); 17],
                "robots.agents",
            ),
            (
                |sections| sections.1.agents = vec![String::new()],
                "robots.agents",
            ),
            (
                |sections| sections.1.agents = vec!["kage/1.0".to_owned()],
                "robots.agents",
            ),
            (
                |sections| sections.1.agents = vec!["a".repeat(65)],
                "robots.agents",
            ),
            (
                |sections| sections.1.agents = vec!["*".to_owned()],
                "robots.agents",
            ),
            (
                |sections| sections.1.scout_quarantine = HumanDuration::from_secs(86_399),
                "robots.scout_quarantine",
            ),
            (
                // Ten years and a second, written out: the bound itself.
                |sections| {
                    sections.1.scout_quarantine = HumanDuration::new(
                        Duration::from_hours(3650 * 24) + Duration::from_secs(1),
                    );
                },
                "robots.scout_quarantine",
            ),
            (
                |sections| sections.2.max_ttl = HumanDuration::new(Duration::from_millis(999)),
                "cache.max_ttl",
            ),
            (
                |sections| sections.2.max_ttl = HumanDuration::from_secs(14 * 86_400 + 1),
                "cache.max_ttl",
            ),
        ];
        for (change, key) in cases {
            let said = check(change, 14).unwrap_err();
            assert!(said.starts_with(key), "{said}");
        }
        // The edges are inside.
        let edges: [Change; 11] = [
            |sections| sections.1.scout_quarantine = HumanDuration::from_hours(24),
            |sections| sections.1.scout_quarantine = HumanDuration::from_hours(3650 * 24),
            |sections| sections.0.thin_words = 1,
            |sections| sections.0.thin_words = 10_000,
            |sections| sections.1.ttl = HumanDuration::from_mins(1),
            |sections| sections.1.error_ttl = HumanDuration::from_mins(1),
            |sections| sections.1.max_bytes = 1,
            |sections| sections.1.max_bytes = MAX_ROBOTS_BYTES,
            |sections| sections.1.agents = vec!["a".repeat(64), "My-Bot_x".to_owned()],
            |sections| sections.1.agents = vec!["a".to_owned(); 16],
            |sections| sections.2.max_ttl = HumanDuration::from_secs(1),
        ];
        for change in edges {
            assert_eq!(check(change, 14), Ok(()));
        }
        assert_eq!(check(|sections| sections.1.agents.clear(), 14), Ok(()));
        // An entry cannot outlive the result it serves: the bound is the
        // retention of results, to the second.
        assert_eq!(
            check(
                |sections| sections.2.max_ttl = HumanDuration::from_secs(86_400),
                1
            ),
            Ok(())
        );
        assert!(
            check(
                |sections| sections.2.max_ttl = HumanDuration::from_secs(86_401),
                1
            )
            .is_err()
        );
    }
}
