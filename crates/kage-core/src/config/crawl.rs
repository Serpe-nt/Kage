//! Configuration of crawls (spec §13).

use serde::{Deserialize, Serialize};

/// The deepest a crawl may ever be allowed to go.
pub const MAX_CRAWL_DEPTH: u32 = 32;

/// The most seeds a crawl may ever be allowed to have.
pub const MAX_CRAWL_SEEDS: u32 = 1000;

/// The most entries back in its tab a visitor may ever be sent: the limit
/// of the sidecar contract.
pub const MAX_BACK_LIMIT: u32 = 10;

/// The most bits two pages may ever differ in and still be called
/// near-duplicates: beyond a quarter of the fingerprint, unrelated pages
/// begin to be flagged.
pub const MAX_NEAR_DUPLICATE_BITS: u32 = 16;

/// Endings of addresses that are files and not pages: a reader's browser
/// would save them or hand them to another program.
pub const DEFAULT_SKIPPED_EXTENSIONS: [&str; 44] = [
    "7z", "apk", "avi", "bin", "bmp", "bz2", "css", "csv", "dmg", "doc", "docx", "epub", "exe",
    "gif", "gz", "ico", "iso", "jpeg", "jpg", "js", "json", "mov", "mp3", "mp4", "mpeg", "msi",
    "ods", "odt", "ogg", "pdf", "png", "ppt", "pptx", "rar", "svg", "tar", "tgz", "wav", "webm",
    "webp", "xls", "xlsx", "xml", "zip",
];

/// Crawls: jobs that read a site by following its links (spec §13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrawlConfig {
    /// How far from its seeds a crawl goes when it does not say.
    #[serde(default = "default_max_depth")]
    pub default_max_depth: u32,
    /// How many pages a crawl asks for when it does not say. A crawl may
    /// ask for as many as a job has items (`jobs.max_items`).
    #[serde(default = "default_max_pages")]
    pub default_max_pages: u32,
    /// Most seeds one crawl may have.
    #[serde(default = "default_max_seeds")]
    pub max_seeds: u32,
    /// Most addresses one crawl keeps in its frontier. What a crawl finds
    /// beyond them is counted and forgotten.
    #[serde(default = "default_max_frontier")]
    pub max_frontier: u32,
    /// How many entries of its tab a visitor goes back, at most, to click
    /// a link on a page it read earlier; 0 for the open page only.
    #[serde(default = "default_back_limit")]
    pub back_limit: u32,
    /// Two pages whose fingerprints differ in at most this many of their
    /// 64 bits are flagged as near-duplicates.
    #[serde(default = "default_near_duplicate_bits")]
    pub near_duplicate_bits: u32,
    /// Most sitemap files a crawl with `use_sitemaps` reads per site: the
    /// one at `/sitemap.xml` and those an index names.
    #[serde(default = "default_max_sitemaps")]
    pub max_sitemaps: u32,
    /// Most addresses read from one sitemap file.
    #[serde(default = "default_max_sitemap_urls")]
    pub max_sitemap_urls: u32,
    /// Endings of addresses that are files and not pages, without the
    /// dot. A link to one is counted as `not_a_page` and never asked for.
    #[serde(default = "default_skipped_extensions")]
    pub skipped_extensions: Vec<String>,
}

const fn default_max_depth() -> u32 {
    3
}

const fn default_max_pages() -> u32 {
    100
}

const fn default_max_seeds() -> u32 {
    100
}

const fn default_max_frontier() -> u32 {
    50_000
}

const fn default_back_limit() -> u32 {
    3
}

const fn default_near_duplicate_bits() -> u32 {
    6
}

const fn default_max_sitemaps() -> u32 {
    10
}

const fn default_max_sitemap_urls() -> u32 {
    50_000
}

fn default_skipped_extensions() -> Vec<String> {
    DEFAULT_SKIPPED_EXTENSIONS
        .iter()
        .map(|&extension| extension.to_owned())
        .collect()
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            default_max_depth: default_max_depth(),
            default_max_pages: default_max_pages(),
            max_seeds: default_max_seeds(),
            max_frontier: default_max_frontier(),
            back_limit: default_back_limit(),
            near_duplicate_bits: default_near_duplicate_bits(),
            max_sitemaps: default_max_sitemaps(),
            max_sitemap_urls: default_max_sitemap_urls(),
            skipped_extensions: default_skipped_extensions(),
        }
    }
}

/// Checks the crawl section against the job limits it lives inside.
///
/// # Errors
///
/// A sentence that names the first key that is out of range.
pub(super) fn validate(crawl: &CrawlConfig, max_items: u32) -> Result<(), String> {
    if crawl.default_max_depth > MAX_CRAWL_DEPTH {
        return Err(format!(
            "crawl.default_max_depth must be at most {MAX_CRAWL_DEPTH}"
        ));
    }
    if crawl.default_max_pages == 0 || crawl.default_max_pages > max_items {
        return Err(format!(
            "crawl.default_max_pages must be between 1 and jobs.max_items ({max_items})"
        ));
    }
    if crawl.max_seeds == 0 || crawl.max_seeds > MAX_CRAWL_SEEDS {
        return Err(format!(
            "crawl.max_seeds must be between 1 and {MAX_CRAWL_SEEDS}"
        ));
    }
    if crawl.max_frontier < crawl.default_max_pages {
        return Err(
            "crawl.max_frontier must be at least crawl.default_max_pages: a crawl cannot read \
             more addresses than it keeps"
                .to_owned(),
        );
    }
    if crawl.back_limit > MAX_BACK_LIMIT {
        return Err(format!("crawl.back_limit must be at most {MAX_BACK_LIMIT}"));
    }
    if crawl.near_duplicate_bits > MAX_NEAR_DUPLICATE_BITS {
        return Err(format!(
            "crawl.near_duplicate_bits must be at most {MAX_NEAR_DUPLICATE_BITS}"
        ));
    }
    if crawl.max_sitemaps == 0 {
        return Err("crawl.max_sitemaps must be at least 1".to_owned());
    }
    if crawl.max_sitemap_urls == 0 {
        return Err("crawl.max_sitemap_urls must be at least 1".to_owned());
    }
    if let Some(extension) = crawl.skipped_extensions.iter().find(|extension| {
        extension.is_empty()
            || extension.len() > 16
            || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    }) {
        return Err(format!(
            "crawl.skipped_extensions holds {extension:?}: an ending is 1 to 16 letters and \
             digits, without the dot"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CrawlConfig, DEFAULT_SKIPPED_EXTENSIONS, validate};

    #[test]
    fn the_defaults_are_what_the_spec_says_and_valid() {
        let crawl = CrawlConfig::default();
        assert_eq!(
            (
                crawl.default_max_depth,
                crawl.default_max_pages,
                crawl.max_seeds
            ),
            (3, 100, 100)
        );
        assert_eq!(
            (
                crawl.max_frontier,
                crawl.back_limit,
                crawl.near_duplicate_bits
            ),
            (50_000, 3, 6)
        );
        assert_eq!((crawl.max_sitemaps, crawl.max_sitemap_urls), (10, 50_000));
        assert_eq!(crawl.skipped_extensions, DEFAULT_SKIPPED_EXTENSIONS);
        assert_eq!(validate(&crawl, 10_000), Ok(()));
        // An empty section is the defaults, and an unknown key is refused.
        let empty: CrawlConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, crawl);
        assert!(serde_json::from_str::<CrawlConfig>(r#"{"depth":3}"#).is_err());
        // The endings are sorted and each is there once.
        let mut sorted = DEFAULT_SKIPPED_EXTENSIONS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, DEFAULT_SKIPPED_EXTENSIONS);
    }

    #[test]
    fn every_key_out_of_range_is_named() {
        type Change = fn(&mut CrawlConfig);
        let with = |change: fn(&mut CrawlConfig)| {
            let mut crawl = CrawlConfig::default();
            change(&mut crawl);
            validate(&crawl, 10_000).unwrap_err()
        };
        let cases: [(Change, &str); 13] = [
            (
                |crawl| crawl.default_max_depth = 33,
                "crawl.default_max_depth",
            ),
            (
                |crawl| crawl.default_max_pages = 0,
                "crawl.default_max_pages",
            ),
            (
                |crawl| crawl.default_max_pages = 10_001,
                "crawl.default_max_pages",
            ),
            (|crawl| crawl.max_seeds = 0, "crawl.max_seeds"),
            (|crawl| crawl.max_seeds = 1001, "crawl.max_seeds"),
            (|crawl| crawl.max_frontier = 99, "crawl.max_frontier"),
            (|crawl| crawl.back_limit = 11, "crawl.back_limit"),
            (
                |crawl| crawl.near_duplicate_bits = 17,
                "crawl.near_duplicate_bits",
            ),
            (|crawl| crawl.max_sitemaps = 0, "crawl.max_sitemaps"),
            (|crawl| crawl.max_sitemap_urls = 0, "crawl.max_sitemap_urls"),
            (
                |crawl| crawl.skipped_extensions.push(".pdf".to_owned()),
                "crawl.skipped_extensions",
            ),
            (
                |crawl| crawl.skipped_extensions.push(String::new()),
                "crawl.skipped_extensions",
            ),
            (
                |crawl| crawl.skipped_extensions.push("a".repeat(17)),
                "crawl.skipped_extensions",
            ),
        ];
        for (change, key) in cases {
            let said = with(change);
            assert!(said.starts_with(key), "{said}");
        }
        // The edges are inside.
        let edge = |change: fn(&mut CrawlConfig)| {
            let mut crawl = CrawlConfig::default();
            change(&mut crawl);
            validate(&crawl, 10_000)
        };
        assert_eq!(edge(|crawl| crawl.default_max_depth = 32), Ok(()));
        assert_eq!(edge(|crawl| crawl.default_max_depth = 0), Ok(()));
        assert_eq!(edge(|crawl| crawl.default_max_pages = 10_000), Ok(()));
        assert_eq!(edge(|crawl| crawl.max_seeds = 1000), Ok(()));
        assert_eq!(edge(|crawl| crawl.max_frontier = 100), Ok(()));
        assert_eq!(edge(|crawl| crawl.back_limit = 10), Ok(()));
        assert_eq!(edge(|crawl| crawl.back_limit = 0), Ok(()));
        assert_eq!(edge(|crawl| crawl.near_duplicate_bits = 16), Ok(()));
        assert_eq!(edge(|crawl| crawl.near_duplicate_bits = 0), Ok(()));
        assert_eq!(edge(|crawl| crawl.skipped_extensions.clear()), Ok(()));
    }
}
