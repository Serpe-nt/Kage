//! Which of the addresses a crawl finds it reads (spec §13).
//!
//! A [`Scope`] is made once from what the crawl was asked for and then
//! judges every address: is it on a site of the crawl, is it within reach of
//! a seed, does a `deny` pattern name it, does an `allow` pattern (when
//! there are any), is it a page at all. Every "no" has a reason, which is
//! what the crawl counts and shows.
//!
//! The patterns are regular expressions looked for anywhere in the
//! address's normalized form (`kage_core::urlnorm`): anchor them with `^`
//! and `$` to say more. They are compiled by a matcher that needs time
//! linear in the address, so a pattern cannot make a crawl hang.

use std::collections::BTreeSet;

use kage_core::urlnorm::UrlKey;
use regex::{Regex, RegexBuilder};
use url::Url;

use crate::site;

/// The most patterns of one kind a crawl may have.
pub const MAX_PATTERNS: usize = 32;

/// The longest pattern, in bytes.
pub const MAX_PATTERN_BYTES: usize = 512;

/// The largest compiled pattern, in bytes: a short pattern can ask for a
/// large machine (`a{1000}{1000}`).
const MAX_COMPILED_BYTES: usize = 1 << 20;

/// Why a crawl does not read an address it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Skip {
    /// It is on another site and the crawl stays on its own.
    OffSite,
    /// It is further from every seed than the crawl goes.
    TooDeep,
    /// A `deny` pattern names it.
    Denied,
    /// The crawl has `allow` patterns and none names it.
    NotAllowed,
    /// It is a file, not a page.
    NotAPage,
    /// The page hides the link from its readers.
    Hidden,
}

impl Skip {
    /// The name a job shows the reason by.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OffSite => "off_site",
            Self::TooDeep => "too_deep",
            Self::Denied => "denied",
            Self::NotAllowed => "not_allowed",
            Self::NotAPage => "not_a_page",
            Self::Hidden => "hidden",
        }
    }

    /// Every reason, in the order a job lists them.
    pub const ALL: [Self; 6] = [
        Self::OffSite,
        Self::TooDeep,
        Self::Denied,
        Self::NotAllowed,
        Self::NotAPage,
        Self::Hidden,
    ];
}

/// Why a scope cannot be made from what was asked for.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    /// More patterns of one kind than [`MAX_PATTERNS`].
    #[error("at most {MAX_PATTERNS} {list} patterns; there are {count}")]
    TooManyPatterns {
        /// `allow` or `deny`.
        list: &'static str,
        /// How many were given.
        count: usize,
    },
    /// A pattern is longer than [`MAX_PATTERN_BYTES`].
    #[error("{list}[{index}] is longer than {MAX_PATTERN_BYTES} bytes")]
    PatternTooLong {
        /// `allow` or `deny`.
        list: &'static str,
        /// Its place in the list.
        index: usize,
    },
    /// A pattern is no regular expression, or one too large to compile.
    #[error("{list}[{index}] is not a usable regular expression: {reason}")]
    BadPattern {
        /// `allow` or `deny`.
        list: &'static str,
        /// Its place in the list.
        index: usize,
        /// What the matcher says.
        reason: String,
    },
}

/// What a crawl was asked to stay inside.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeRequest {
    /// Patterns of addresses to read; none for every address.
    pub allow: Vec<String>,
    /// Patterns of addresses never to read.
    pub deny: Vec<String>,
    /// Whether the crawl stays on the sites of its seeds.
    pub same_site_only: bool,
    /// The furthest a page may be from a seed, in links.
    pub max_depth: u32,
    /// Endings of addresses that are no pages, without the dot.
    pub skipped_extensions: Vec<String>,
}

/// The judge of a crawl's addresses.
#[derive(Debug, Clone)]
pub struct Scope {
    allow: Vec<Regex>,
    deny: Vec<Regex>,
    sites: Option<BTreeSet<String>>,
    max_depth: u32,
    skipped_extensions: BTreeSet<String>,
}

fn compile(list: &'static str, patterns: &[String]) -> Result<Vec<Regex>, ScopeError> {
    if patterns.len() > MAX_PATTERNS {
        return Err(ScopeError::TooManyPatterns {
            list,
            count: patterns.len(),
        });
    }
    patterns
        .iter()
        .enumerate()
        .map(|(index, pattern)| {
            if pattern.len() > MAX_PATTERN_BYTES {
                return Err(ScopeError::PatternTooLong { list, index });
            }
            RegexBuilder::new(pattern)
                .size_limit(MAX_COMPILED_BYTES)
                .build()
                .map_err(|error| ScopeError::BadPattern {
                    list,
                    index,
                    // The first line: the rest is a drawing of the pattern.
                    reason: match &error {
                        regex::Error::CompiledTooBig(_) => {
                            "it compiles to more than a pattern may".to_owned()
                        }
                        other => other
                            .to_string()
                            .lines()
                            .last()
                            .unwrap_or_default()
                            .trim()
                            .trim_start_matches("error: ")
                            .to_owned(),
                    },
                })
        })
        .collect()
}

impl Scope {
    /// The scope of a crawl that starts at `seeds`.
    ///
    /// # Errors
    ///
    /// [`ScopeError`] for a pattern that cannot be used; it names the
    /// pattern.
    pub fn new(request: &ScopeRequest, seeds: &[Url]) -> Result<Self, ScopeError> {
        Ok(Self {
            allow: compile("allow", &request.allow)?,
            deny: compile("deny", &request.deny)?,
            sites: request
                .same_site_only
                .then(|| seeds.iter().filter_map(site::of).collect()),
            max_depth: request.max_depth,
            skipped_extensions: request
                .skipped_extensions
                .iter()
                .map(|extension| extension.trim_start_matches('.').to_ascii_lowercase())
                .filter(|extension| !extension.is_empty())
                .collect(),
        })
    }

    /// The furthest a page may be from a seed.
    pub const fn max_depth(&self) -> u32 {
        self.max_depth
    }

    /// Whether an address is on a site the crawl stays on. Always true for
    /// a crawl that may leave its seeds' sites.
    pub fn on_site(&self, url: &Url) -> bool {
        self.sites
            .as_ref()
            .is_none_or(|sites| site::of(url).is_some_and(|site| sites.contains(&site)))
    }

    /// Whether the end of an address's path says it is a file. A path
    /// without a dot in its last segment is a page.
    fn is_file(&self, url: &Url) -> bool {
        let last = url.path().rsplit('/').next().unwrap_or_default();
        last.rsplit_once('.').is_some_and(|(stem, extension)| {
            !stem.is_empty()
                && self
                    .skipped_extensions
                    .contains(&extension.to_ascii_lowercase())
        })
    }

    /// Judges an address a page links to, `depth` links from a seed. `key`
    /// is the address's normalized form, which the patterns are matched
    /// against.
    ///
    /// # Errors
    ///
    /// The reason the crawl does not read it. Of several the first in this
    /// order is given: another site, too deep, denied, not allowed, a file.
    pub fn judge(&self, url: &Url, key: &UrlKey, depth: u32) -> Result<(), Skip> {
        if !self.on_site(url) {
            return Err(Skip::OffSite);
        }
        if depth > self.max_depth {
            return Err(Skip::TooDeep);
        }
        if self
            .deny
            .iter()
            .any(|pattern| pattern.is_match(key.as_str()))
        {
            return Err(Skip::Denied);
        }
        if !self.allow.is_empty()
            && !self
                .allow
                .iter()
                .any(|pattern| pattern.is_match(key.as_str()))
        {
            return Err(Skip::NotAllowed);
        }
        if self.is_file(url) {
            return Err(Skip::NotAPage);
        }
        Ok(())
    }

    /// Judges a seed. A seed was asked for by name: it is read whatever the
    /// `allow` patterns say and whatever its ending. Only a `deny` pattern
    /// that names it is a contradiction, which the caller is told.
    ///
    /// # Errors
    ///
    /// [`Skip::Denied`] when a `deny` pattern names the seed.
    pub fn judge_seed(&self, key: &UrlKey) -> Result<(), Skip> {
        if self
            .deny
            .iter()
            .any(|pattern| pattern.is_match(key.as_str()))
        {
            return Err(Skip::Denied);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use kage_core::config::DEFAULT_SKIPPED_EXTENSIONS;
    use kage_core::urlnorm::{NormalizeOptions, UrlKey, normalize};
    use url::Url;

    use super::{MAX_PATTERN_BYTES, MAX_PATTERNS, Scope, ScopeError, ScopeRequest, Skip};

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn key(url: &Url) -> UrlKey {
        normalize(url, &NormalizeOptions::default())
    }

    fn request() -> ScopeRequest {
        ScopeRequest {
            allow: Vec::new(),
            deny: Vec::new(),
            same_site_only: true,
            max_depth: 3,
            skipped_extensions: DEFAULT_SKIPPED_EXTENSIONS
                .iter()
                .map(|&extension| extension.to_owned())
                .collect(),
        }
    }

    fn judged(scope: &Scope, text: &str, depth: u32) -> Result<(), Skip> {
        let url = url(text);
        scope.judge(&url, &key(&url), depth)
    }

    #[test]
    fn a_crawl_stays_on_the_sites_of_its_seeds() {
        let seeds = [
            url("https://www.example.org/start"),
            url("http://shop.example.co.uk/"),
        ];
        let scope = Scope::new(&request(), &seeds).unwrap();
        for (address, verdict) in [
            ("https://www.example.org/a", Ok(())),
            // Another host of the same site, another scheme, another port.
            ("http://blog.example.org:8080/b", Ok(())),
            ("https://example.org/", Ok(())),
            ("https://shop.example.co.uk/c", Ok(())),
            ("https://example.co.uk/d", Ok(())),
            ("https://example.net/", Err(Skip::OffSite)),
            ("https://other.co.uk/", Err(Skip::OffSite)),
            ("https://example.org.evil.test/", Err(Skip::OffSite)),
            ("https://203.0.113.9/", Err(Skip::OffSite)),
        ] {
            assert_eq!(judged(&scope, address, 1), verdict, "{address}");
        }

        // A crawl that may leave follows a link to any site.
        let free = Scope::new(
            &ScopeRequest {
                same_site_only: false,
                ..request()
            },
            &seeds,
        )
        .unwrap();
        assert_eq!(judged(&free, "https://example.net/", 1), Ok(()));
        assert!(free.on_site(&url("https://anything.test/")));
        assert!(!scope.on_site(&url("https://anything.test/")));
    }

    #[test]
    fn a_page_further_than_the_depth_is_not_read() {
        let scope = Scope::new(&request(), &[url("https://example.org/")]).unwrap();
        assert_eq!(scope.max_depth(), 3);
        assert_eq!(judged(&scope, "https://example.org/a", 0), Ok(()));
        assert_eq!(judged(&scope, "https://example.org/a", 3), Ok(()));
        assert_eq!(
            judged(&scope, "https://example.org/a", 4),
            Err(Skip::TooDeep)
        );
        let shallow = Scope::new(
            &ScopeRequest {
                max_depth: 0,
                ..request()
            },
            &[url("https://example.org/")],
        )
        .unwrap();
        assert_eq!(judged(&shallow, "https://example.org/a", 0), Ok(()));
        assert_eq!(
            judged(&shallow, "https://example.org/a", 1),
            Err(Skip::TooDeep)
        );
    }

    #[test]
    fn patterns_are_looked_for_in_the_normalized_address() {
        let scope = Scope::new(
            &ScopeRequest {
                allow: vec![
                    "/articles/".to_owned(),
                    r"^https://example\.org/about$".to_owned(),
                ],
                deny: vec![
                    r"[?&]page=\d{3,}".to_owned(),
                    "/articles/drafts/".to_owned(),
                ],
                ..request()
            },
            &[url("https://example.org/")],
        )
        .unwrap();
        for (address, verdict) in [
            ("https://example.org/articles/one", Ok(())),
            ("https://example.org/about", Ok(())),
            ("https://example.org/about/team", Err(Skip::NotAllowed)),
            ("https://example.org/contact", Err(Skip::NotAllowed)),
            // Deny wins over allow.
            ("https://example.org/articles/drafts/two", Err(Skip::Denied)),
            ("https://example.org/articles/?page=100", Err(Skip::Denied)),
            ("https://example.org/articles/?page=99", Ok(())),
            // The normalized form is what is matched: the host in lower
            // case, the campaign tag gone, the query sorted, no fragment.
            ("https://EXAMPLE.org/about?utm_source=x#team", Ok(())),
            (
                "https://example.org/articles/?z=1&page=100",
                Err(Skip::Denied),
            ),
        ] {
            assert_eq!(judged(&scope, address, 1), verdict, "{address}");
        }
    }

    #[test]
    fn a_file_is_not_a_page() {
        let scope = Scope::new(&request(), &[url("https://example.org/")]).unwrap();
        for (address, verdict) in [
            ("https://example.org/report.pdf", Err(Skip::NotAPage)),
            (
                "https://example.org/files/Photo.JPG?size=large",
                Err(Skip::NotAPage),
            ),
            ("https://example.org/archive.tar.gz", Err(Skip::NotAPage)),
            ("https://example.org/page.html", Ok(())),
            ("https://example.org/page.php?file=a.pdf", Ok(())),
            ("https://example.org/v1.2/notes", Ok(())),
            ("https://example.org/release-1.0.zip/", Ok(())),
            // A name that is only an ending is no file of that kind.
            ("https://example.org/.pdf", Ok(())),
            ("https://example.org/", Ok(())),
        ] {
            assert_eq!(judged(&scope, address, 1), verdict, "{address}");
        }
        // The list is the crawl's: without one, everything is a page.
        let all = Scope::new(
            &ScopeRequest {
                skipped_extensions: vec![".PDF".to_owned(), String::new(), ".".to_owned()],
                ..request()
            },
            &[url("https://example.org/")],
        )
        .unwrap();
        assert_eq!(
            judged(&all, "https://example.org/a.pdf", 1),
            Err(Skip::NotAPage)
        );
        assert_eq!(judged(&all, "https://example.org/a.zip", 1), Ok(()));
    }

    #[test]
    fn the_first_reason_in_order_is_the_one_given() {
        let scope = Scope::new(
            &ScopeRequest {
                allow: vec!["/only/".to_owned()],
                deny: vec!["secret".to_owned()],
                ..request()
            },
            &[url("https://example.org/")],
        )
        .unwrap();
        assert_eq!(
            judged(&scope, "https://example.net/secret.pdf", 9),
            Err(Skip::OffSite)
        );
        assert_eq!(
            judged(&scope, "https://example.org/secret.pdf", 9),
            Err(Skip::TooDeep)
        );
        assert_eq!(
            judged(&scope, "https://example.org/secret.pdf", 1),
            Err(Skip::Denied)
        );
        assert_eq!(
            judged(&scope, "https://example.org/plain.pdf", 1),
            Err(Skip::NotAllowed)
        );
        assert_eq!(
            judged(&scope, "https://example.org/only/plain.pdf", 1),
            Err(Skip::NotAPage)
        );
        assert_eq!(judged(&scope, "https://example.org/only/page", 1), Ok(()));
    }

    #[test]
    fn a_seed_is_read_unless_it_is_denied() {
        let scope = Scope::new(
            &ScopeRequest {
                allow: vec!["/articles/".to_owned()],
                deny: vec!["/private/".to_owned()],
                ..request()
            },
            &[url("https://example.org/")],
        )
        .unwrap();
        // Asked for by name: no `allow` pattern has to name it, and a file
        // is read too.
        assert_eq!(scope.judge_seed(&key(&url("https://example.org/"))), Ok(()));
        assert_eq!(
            scope.judge_seed(&key(&url("https://example.org/list.xml"))),
            Ok(())
        );
        assert_eq!(
            scope.judge_seed(&key(&url("https://example.org/private/x"))),
            Err(Skip::Denied)
        );
    }

    #[test]
    fn patterns_that_cannot_be_used_are_refused_by_name() {
        let seeds = [url("https://example.org/")];
        let with = |allow: Vec<String>, deny: Vec<String>| {
            Scope::new(
                &ScopeRequest {
                    allow,
                    deny,
                    ..request()
                },
                &seeds,
            )
            .map(|_| ())
        };
        let bad = with(vec!["fine".to_owned(), "(unclosed".to_owned()], Vec::new()).unwrap_err();
        match &bad {
            ScopeError::BadPattern {
                list,
                index,
                reason,
            } => {
                assert_eq!((*list, *index), ("allow", 1));
                assert!(reason.contains("unclosed group"), "{reason}");
                assert!(!reason.contains('\n'));
            }
            other => panic!("{other:?}"),
        }
        assert!(
            bad.to_string()
                .starts_with("allow[1] is not a usable regular expression")
        );
        // What the matcher does not do in linear time, it does not do.
        assert!(matches!(
            with(Vec::new(), vec![r"(a)\1".to_owned()]),
            Err(ScopeError::BadPattern {
                list: "deny",
                index: 0,
                ..
            })
        ));
        // A short pattern that asks for a large machine.
        let large = with(Vec::new(), vec![r"\w{1000}{1000}".to_owned()]).unwrap_err();
        assert!(
            matches!(&large, ScopeError::BadPattern { reason, .. } if reason.contains("more than a pattern may")),
            "{large:?}"
        );
        assert_eq!(
            with(vec!["a".repeat(MAX_PATTERN_BYTES + 1)], Vec::new()),
            Err(ScopeError::PatternTooLong {
                list: "allow",
                index: 0
            })
        );
        assert_eq!(
            with(vec!["a".repeat(MAX_PATTERN_BYTES)], Vec::new()),
            Ok(())
        );
        assert_eq!(
            with(Vec::new(), vec!["a".to_owned(); MAX_PATTERNS + 1]),
            Err(ScopeError::TooManyPatterns {
                list: "deny",
                count: MAX_PATTERNS + 1
            })
        );
        assert_eq!(with(Vec::new(), vec!["a".to_owned(); MAX_PATTERNS]), Ok(()));
    }

    #[test]
    fn every_reason_has_a_name_of_its_own() {
        let names: Vec<&str> = Skip::ALL.iter().map(|skip| skip.as_str()).collect();
        assert_eq!(
            names,
            [
                "off_site",
                "too_deep",
                "denied",
                "not_allowed",
                "not_a_page",
                "hidden"
            ]
        );
    }
}
