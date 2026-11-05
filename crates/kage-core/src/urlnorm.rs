//! URL normalization for dedupe and cache keys (spec §13, §15).
//!
//! [`normalize`] reduces a URL to a [`UrlKey`]: scheme and host in lower
//! case, an internationalized host in its ASCII form, default port removed,
//! fragment removed, percent-encoding normalized (RFC 3986 §6.2.2: upper-case
//! hex digits, unreserved characters decoded), tracking parameters removed by
//! a configurable list, the trailing-slash policy applied, and the query
//! sorted. Two URLs with the same key are treated as the same page by the
//! crawl frontier and the response cache.
//!
//! A key is not a URL to request. The request always goes out with the URL
//! as it was discovered, minus the fragment and with its host in its normal
//! form ([`request_url`]): a sorted query string or a missing click id is a
//! URL shape the site's own links never produce, and that is a cheap tell.
//! [`UrlKey`] therefore cannot be turned back into a `Url`.
//!
//! **The normal form of a host** ([`normal_host`], [`normal_url`]; spec §7,
//! §19, F104): the name of the DNS root that ends a fully qualified name is
//! no part of it, so `example.org.` is `example.org` to everything that is
//! keyed by a host or a site (the breaker, the cooldowns, the pace, the
//! plain exit, the cache, a crawl's frontier), and a request for it goes to
//! `example.org`. A name with an empty label (two or more dots at its end,
//! one at its start, two in a row) names no host at all; the SSRF guard
//! refuses it on every road (`crate::ssrf::check_url`).

use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::macros::string_enum;

/// `href` resolved against `base` as a browser resolves it in a document
/// written in `encoding` (the URL standard's parser with an encoding): the
/// characters of a query are written in the document's output encoding
/// (UTF-8 for a UTF-16 document) and then percent-encoded, a character that
/// encoding cannot write as `%26%23`, its number and `%3B` (`&#…;`); the
/// path and everything else in UTF-8, as always. A link in a page written in
/// windows-1252 that says `?q=für` asks for `?q=f%FCr`, as both browsers
/// ask (F161). `None` when `href` is no URL.
pub fn join_in(base: &Url, href: &str, encoding: &'static encoding_rs::Encoding) -> Option<Url> {
    let encoding = encoding.output_encoding();
    if encoding == encoding_rs::UTF_8 {
        return base.join(href).ok();
    }
    let encode: &dyn Fn(&str) -> std::borrow::Cow<'_, [u8]> =
        &|text| std::borrow::Cow::Owned(encode_query(text, encoding));
    Url::options()
        .base_url(Some(base))
        .encoding_override(Some(encode))
        .parse(href)
        .ok()
}

/// The bytes of a query in `encoding`, a character it cannot write as the
/// URL standard writes it: `%26%23`, its number in base ten, `%3B`. The
/// parser percent-encodes the rest and leaves a `%` as it is.
fn encode_query(text: &str, encoding: &'static encoding_rs::Encoding) -> Vec<u8> {
    let mut encoder = encoding.new_encoder();
    let mut out = Vec::with_capacity(text.len() * 2);
    let mut rest = text;
    let mut buffer = [0_u8; 1024];
    loop {
        let (result, read, written) =
            encoder.encode_from_utf8_without_replacement(rest, &mut buffer, true);
        out.extend_from_slice(&buffer[..written]);
        rest = &rest[read..];
        match result {
            encoding_rs::EncoderResult::InputEmpty => return out,
            encoding_rs::EncoderResult::OutputFull => {}
            encoding_rs::EncoderResult::Unmappable(character) => {
                out.extend_from_slice(format!("%26%23{}%3B", u32::from(character)).as_bytes());
            }
        }
    }
}

string_enum! {
    /// What to do with a trailing slash on a path other than `/`.
    pub enum TrailingSlash {
        /// Leave the path as it is: `/a` and `/a/` are different pages.
        Keep => "keep",
        /// Remove it: `/a/` becomes `/a`.
        Strip => "strip",
        /// Add it to paths whose last segment has no file extension:
        /// `/a` becomes `/a/`, `/a.html` stays.
        Add => "add",
    }
}

/// Query parameters removed by default: campaign tags and click ids that
/// identify a visit, not a page. Names that select content on many sites
/// (`ref`, `source`, `id`, session ids) are deliberately absent.
pub const DEFAULT_TRACKING_PARAMS: [&str; 36] = [
    "utm_*",
    "gclid",
    "gclsrc",
    "dclid",
    "gbraid",
    "wbraid",
    "fbclid",
    "msclkid",
    "yclid",
    "twclid",
    "ttclid",
    "igshid",
    "mc_cid",
    "mc_eid",
    "_ga",
    "_gl",
    "_hsenc",
    "_hsmi",
    "mkt_tok",
    "li_fat_id",
    "vero_id",
    "oly_anon_id",
    "oly_enc_id",
    "s_kwcid",
    // Added in M10: click ids of shopping results, of Yandex, Pinterest,
    // Impact, Facebook's in-app browser, Rambler and Snapchat, and the
    // visitor cookies HubSpot carries from one of its sites to another.
    "srsltid",
    "ysclid",
    "_openstat",
    "epik",
    "irclickid",
    "mibextid",
    "rb_clickid",
    "sccid",
    "__hsfp",
    "__hssc",
    "__hstc",
    "hsctatracking",
];

/// How URLs are normalized. Lives in the `urls` section of the
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizeOptions {
    /// Query parameter names to remove, compared without regard to case. A
    /// trailing `*` matches any name with that prefix.
    #[serde(default = "default_tracking_params")]
    pub tracking_params: Vec<String>,
    /// Trailing-slash policy.
    #[serde(default = "default_trailing_slash")]
    pub trailing_slash: TrailingSlash,
}

fn default_tracking_params() -> Vec<String> {
    DEFAULT_TRACKING_PARAMS
        .iter()
        .map(|&name| name.to_owned())
        .collect()
}

const fn default_trailing_slash() -> TrailingSlash {
    TrailingSlash::Keep
}

impl Default for NormalizeOptions {
    fn default() -> Self {
        Self {
            tracking_params: default_tracking_params(),
            trailing_slash: default_trailing_slash(),
        }
    }
}

impl NormalizeOptions {
    fn is_tracking(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.tracking_params.iter().any(|pattern| {
            let pattern = pattern.to_ascii_lowercase();
            match pattern.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == pattern,
            }
        })
    }
}

/// A normalized URL, used only to tell whether two URLs are the same page.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UrlKey(String);

impl UrlKey {
    /// The key as text, for storage and hashing.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for UrlKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for UrlKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UrlKey({})", self.0)
    }
}

/// A host name that has no normal form: one of its labels is empty.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("host {0:?} has an empty label, and names no host")]
pub struct EmptyLabel(pub String);

/// The normal form of a host as the `url` crate spells it (lower case, an
/// internationalized name in its ASCII form): without the one trailing dot
/// of a fully qualified name. An address and a name without that dot are
/// their own normal form.
///
/// # Errors
///
/// [`EmptyLabel`] for a name with an empty label: two or more dots at its
/// end, one at its start, two in a row, or nothing but dots.
pub fn normal_host(host: &str) -> Result<&str, EmptyLabel> {
    // An IPv6 address in brackets has no labels.
    if host.starts_with('[') {
        return Ok(host);
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    if name.is_empty() || name.split('.').any(str::is_empty) {
        return Err(EmptyLabel(host.to_owned()));
    }
    Ok(name)
}

/// The URL with its host in its normal form ([`normal_host`]); a URL
/// without a host as it is.
///
/// # Errors
///
/// [`EmptyLabel`] for a host with an empty label.
pub fn normal_url(url: &Url) -> Result<Url, EmptyLabel> {
    let Some(host) = url.host_str() else {
        return Ok(url.clone());
    };
    let normal = normal_host(host)?;
    if normal.len() == host.len() {
        return Ok(url.clone());
    }
    let mut out = url.clone();
    // The shorter name is a name the URL already held: it parses.
    if out.set_host(Some(normal)).is_err() {
        return Err(EmptyLabel(host.to_owned()));
    }
    Ok(out)
}

/// The URL with the one trailing dot of its host dropped where it has one,
/// and as it is otherwise: a host with an empty label keeps it, for the
/// guard to refuse.
fn without_root_dot(url: &Url) -> Url {
    normal_url(url).unwrap_or_else(|_| url.clone())
}

/// Normalizes a URL into its key.
///
/// The `url` crate already lower-cases the scheme and the host, converts an
/// internationalized host to punycode, drops default ports and resolves `.`
/// and `..` segments when it parses. The host is keyed in its normal form
/// ([`normal_host`]).
pub fn normalize(url: &Url, options: &NormalizeOptions) -> UrlKey {
    let mut key = without_root_dot(url);
    key.set_fragment(None);

    let path = normalize_percent_encoding(key.path());
    let path = apply_trailing_slash(&path, options.trailing_slash).unwrap_or(path);
    if path != key.path() {
        key.set_path(&path);
    }

    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(name, _)| !options.is_tracking(name))
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    if pairs.is_empty() {
        key.set_query(None);
    } else {
        // By name, then by value: repeated parameters end up in one fixed order.
        pairs.sort();
        key.query_pairs_mut().clear().extend_pairs(pairs);
    }

    UrlKey(key.into())
}

/// RFC 3986 §6.2.2: a percent-encoded octet is written with upper-case hex
/// digits, and an encoded unreserved character (a letter, a digit, `-`, `.`,
/// `_` or `~`) is written as itself. Reserved characters stay encoded:
/// `%2F` is not a path separator.
fn normalize_percent_encoding(text: &str) -> String {
    use fmt::Write as _;

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find('%') {
        out.push_str(&rest[..position]);
        let after = &rest[position + 1..];
        let octet = after
            .get(..2)
            .filter(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match octet {
            Some(byte) if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) => {
                out.push(char::from(byte));
                rest = &after[2..];
            }
            Some(byte) => {
                // Writing to a String cannot fail.
                let _ = write!(out, "%{byte:02X}");
                rest = &after[2..];
            }
            // A stray percent sign is kept as it is.
            None => {
                out.push('%');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The path after the trailing-slash policy, or `None` when it is unchanged.
fn apply_trailing_slash(path: &str, policy: TrailingSlash) -> Option<String> {
    if path.is_empty() || path == "/" {
        return None;
    }
    match policy {
        TrailingSlash::Keep => None,
        TrailingSlash::Strip => path
            .strip_suffix('/')
            .filter(|stripped| !stripped.is_empty())
            .map(str::to_owned),
        TrailingSlash::Add => {
            let last_segment = path.rsplit('/').next().unwrap_or_default();
            if path.ends_with('/') || last_segment.contains('.') {
                None
            } else {
                Some(format!("{path}/"))
            }
        }
    }
}

/// The URL to request for a discovered link: exactly as discovered, without
/// the fragment, which browsers never send, and with its host in its normal
/// form ([`normal_host`]).
pub fn request_url(discovered: &Url) -> Url {
    let mut url = without_root_dot(discovered);
    url.set_fragment(None);
    url
}

#[cfg(test)]
mod properties;

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{NormalizeOptions, TrailingSlash, UrlKey, join_in, normalize, request_url};
    use crate::macros::test_support::assert_string_enum;

    fn key_with(text: &str, options: &NormalizeOptions) -> String {
        normalize(&Url::parse(text).unwrap(), options)
            .as_str()
            .to_owned()
    }

    fn key(text: &str) -> String {
        key_with(text, &NormalizeOptions::default())
    }

    fn with_slash(policy: TrailingSlash) -> NormalizeOptions {
        NormalizeOptions {
            trailing_slash: policy,
            ..NormalizeOptions::default()
        }
    }

    #[test]
    fn scheme_and_host_are_lower_cased() {
        assert_eq!(key("HTTPS://Example.ORG/Path"), "https://example.org/Path");
        // The path keeps its case: paths are case-sensitive.
        assert_ne!(
            key("https://example.org/Path"),
            key("https://example.org/path")
        );
    }

    #[test]
    fn default_ports_are_removed_and_other_ports_kept() {
        assert_eq!(key("https://example.org:443/a"), "https://example.org/a");
        assert_eq!(key("http://example.org:80/a"), "http://example.org/a");
        assert_eq!(
            key("https://example.org:8443/a"),
            "https://example.org:8443/a"
        );
        assert_eq!(key("http://example.org:443/a"), "http://example.org:443/a");
    }

    #[test]
    fn the_fragment_is_removed() {
        assert_eq!(
            key("https://example.org/a#section-2"),
            "https://example.org/a"
        );
        assert_eq!(
            key("https://example.org/a?x=1#top"),
            "https://example.org/a?x=1"
        );
        assert_eq!(key("https://example.org/#"), "https://example.org/");
    }

    #[test]
    fn tracking_parameters_are_removed() {
        assert_eq!(
            key("https://example.org/p?utm_source=news&id=7&utm_medium=email&gclid=abc"),
            "https://example.org/p?id=7"
        );
        assert_eq!(
            key("https://example.org/p?fbclid=x&msclkid=y&_ga=z"),
            "https://example.org/p"
        );
        // The click ids added in M10, each by itself and all together.
        for name in [
            "srsltid",
            "ysclid",
            "_openstat",
            "epik",
            "irclickid",
            "mibextid",
            "rb_clickid",
            "ScCid",
            "__hsfp",
            "__hssc",
            "__hstc",
            "hsCtaTracking",
        ] {
            assert_eq!(
                key(&format!("https://example.org/p?id=7&{name}=x")),
                "https://example.org/p?id=7",
                "{name}"
            );
        }
        assert_eq!(super::DEFAULT_TRACKING_PARAMS.len(), 36);
        // Matching ignores case; the prefix rule covers every utm_ tag.
        assert_eq!(
            key("https://example.org/p?UTM_Campaign=a&Gclid=b&utm_anything_new=c"),
            "https://example.org/p"
        );
    }

    #[test]
    fn parameters_that_select_content_are_kept() {
        assert_eq!(
            key("https://example.org/p?ref=sidebar&source=rss&id=7&sid=abc&page=2"),
            "https://example.org/p?id=7&page=2&ref=sidebar&sid=abc&source=rss"
        );
        // Only whole names and declared prefixes match.
        assert_eq!(
            key("https://example.org/p?gclid2=a&my_gclid=b&utm=c"),
            "https://example.org/p?gclid2=a&my_gclid=b&utm=c"
        );
    }

    #[test]
    fn the_tracking_list_is_configurable() {
        let options = NormalizeOptions {
            tracking_params: vec!["session".to_owned(), "trk_*".to_owned()],
            ..NormalizeOptions::default()
        };

        assert_eq!(
            key_with(
                "https://example.org/p?session=1&trk_a=2&utm_source=x&q=y",
                &options
            ),
            "https://example.org/p?q=y&utm_source=x"
        );

        let nothing = NormalizeOptions {
            tracking_params: Vec::new(),
            ..NormalizeOptions::default()
        };
        assert_eq!(
            key_with("https://example.org/p?utm_source=x", &nothing),
            "https://example.org/p?utm_source=x"
        );
    }

    #[test]
    fn the_query_is_sorted_by_name_then_value() {
        assert_eq!(
            key("https://example.org/p?b=2&a=1&c=3"),
            "https://example.org/p?a=1&b=2&c=3"
        );
        assert_eq!(
            key("https://example.org/p?a=1&b=2&c=3"),
            key("https://example.org/p?c=3&b=2&a=1")
        );
        // Repeated names are all kept, ordered by value.
        assert_eq!(
            key("https://example.org/p?tag=z&tag=a&tag=m"),
            "https://example.org/p?tag=a&tag=m&tag=z"
        );
        assert_eq!(
            key("https://example.org/p?x=1&x=1"),
            "https://example.org/p?x=1&x=1"
        );
    }

    #[test]
    fn empty_queries_disappear_and_empty_values_stay() {
        assert_eq!(key("https://example.org/p?"), "https://example.org/p");
        assert_eq!(
            key("https://example.org/p?utm_source=a"),
            "https://example.org/p"
        );
        assert_eq!(
            key("https://example.org/p?flag&b="),
            "https://example.org/p?b=&flag="
        );
    }

    #[test]
    fn equivalent_encodings_share_a_key() {
        assert_eq!(
            key("https://example.org/s?q=a+b"),
            key("https://example.org/s?q=a%20b")
        );
        assert_eq!(
            key("https://example.org/s?q=caf%C3%A9"),
            key("https://example.org/s?q=café")
        );
        assert_ne!(
            key("https://example.org/s?q=a+b"),
            key("https://example.org/s?q=a%2Bb")
        );
    }

    #[test]
    fn percent_encoding_is_normalized() {
        // Hex digits are upper-cased.
        assert_eq!(
            key("https://example.org/caf%c3%a9"),
            "https://example.org/caf%C3%A9"
        );
        assert_eq!(
            key("https://example.org/caf%c3%a9"),
            key("https://example.org/caf%C3%A9")
        );
        assert_eq!(
            key("https://example.org/café"),
            key("https://example.org/caf%C3%A9")
        );
        // Encoded unreserved characters are decoded.
        assert_eq!(
            key("https://example.org/%7Euser/%41%2Db%5F1%2e0"),
            "https://example.org/~user/A-b_1.0"
        );
        assert_eq!(
            key("https://example.org/%7euser"),
            key("https://example.org/~user")
        );
        // Reserved characters stay encoded: %2F is not a path separator.
        assert_eq!(
            key("https://example.org/a%2fb"),
            "https://example.org/a%2Fb"
        );
        assert_ne!(
            key("https://example.org/a%2Fb"),
            key("https://example.org/a/b")
        );
        assert_eq!(
            key("https://example.org/a%20b%3Fc"),
            "https://example.org/a%20b%3Fc"
        );
        // The query is normalized the same way.
        assert_eq!(
            key("https://example.org/s?q=%7e%41"),
            key("https://example.org/s?q=~A")
        );
        assert_eq!(
            key("https://example.org/s?q=caf%c3%a9"),
            key("https://example.org/s?q=caf%C3%A9")
        );
    }

    #[test]
    fn malformed_percent_sequences_are_left_alone() {
        use super::normalize_percent_encoding;

        assert_eq!(normalize_percent_encoding("/100%"), "/100%");
        assert_eq!(normalize_percent_encoding("/a%zzb"), "/a%zzb");
        assert_eq!(normalize_percent_encoding("/a%4"), "/a%4");
        assert_eq!(normalize_percent_encoding("/a%+1b"), "/a%+1b");
        assert_eq!(normalize_percent_encoding("/%é"), "/%é");
        assert_eq!(normalize_percent_encoding("/%41%"), "/A%");
        assert_eq!(normalize_percent_encoding("/plain"), "/plain");
        assert_eq!(normalize_percent_encoding(""), "");
    }

    #[test]
    fn internationalized_hosts_are_keyed_in_ascii_form() {
        assert_eq!(
            key("https://BÜCHER.example/a"),
            "https://xn--bcher-kva.example/a"
        );
        assert_eq!(
            key("https://bücher.example/a"),
            key("https://xn--bcher-kva.example/a")
        );
    }

    #[test]
    fn one_trailing_dot_of_a_host_is_no_part_of_it() {
        use super::{normal_host, normal_url};

        assert_eq!(normal_host("example.org."), Ok("example.org"));
        assert_eq!(normal_host("example.org"), Ok("example.org"));
        assert_eq!(normal_host("localhost."), Ok("localhost"));
        assert_eq!(normal_host("[2001:db8::7]"), Ok("[2001:db8::7]"));
        assert_eq!(
            normal_url(&Url::parse("http://Other.QA.test.:8091/yes?x=1#f").unwrap())
                .unwrap()
                .as_str(),
            "http://other.qa.test:8091/yes?x=1#f"
        );
        // Nothing else of the URL moves, and a host without the dot is
        // left as it is.
        let plain = Url::parse("https://example.org/a/?b=2&a=1").unwrap();
        assert_eq!(normal_url(&plain).unwrap(), plain);
        // An address has no labels: the parser drops the dot of `1.2.3.4.`.
        assert_eq!(
            normal_url(&Url::parse("http://203.0.113.9./").unwrap())
                .unwrap()
                .as_str(),
            "http://203.0.113.9/"
        );
        // A URL without a host has no host to normalize.
        let mail = Url::parse("mailto:someone@example.org").unwrap();
        assert_eq!(normal_url(&mail).unwrap(), mail);

        // The key and the URL to request are those of the name.
        assert_eq!(key("https://example.org./a"), key("https://example.org/a"));
        assert_eq!(key("https://Example.ORG./a"), "https://example.org/a");
        assert_eq!(
            request_url(&Url::parse("https://example.org.:8443/a#top").unwrap()).as_str(),
            "https://example.org:8443/a"
        );
    }

    #[test]
    fn a_host_with_an_empty_label_has_no_normal_form() {
        use super::{EmptyLabel, normal_host, normal_url};

        for host in [
            "localhost..",
            "127.0.0.1..",
            "169.254.169.254..",
            "postgres...",
            "example.org..",
            ".example.org",
            "a..b.example",
            "..",
            ".",
            "",
        ] {
            assert_eq!(
                normal_host(host),
                Err(EmptyLabel(host.to_owned())),
                "{host}"
            );
        }
        for text in [
            "http://localhost../",
            "http://127.0.0.1../",
            "http://169.254.169.254../",
            "http://postgres../",
            "http://.fixture.kage.test/",
            "http://../",
        ] {
            let url = Url::parse(text).unwrap();
            assert!(normal_url(&url).is_err(), "{text}");
            // The key and the URL to request keep such a host as it is, for
            // the guard to refuse.
            assert_eq!(request_url(&url).host_str(), url.host_str(), "{text}");
        }
        assert_eq!(
            EmptyLabel("a..".to_owned()).to_string(),
            "host \"a..\" has an empty label, and names no host"
        );
    }

    #[test]
    fn dot_segments_and_empty_paths_are_resolved() {
        assert_eq!(key("https://example.org"), "https://example.org/");
        assert_eq!(
            key("https://example.org/a/./b/../c"),
            "https://example.org/a/c"
        );
    }

    #[test]
    fn trailing_slash_is_kept_by_default() {
        assert_eq!(key("https://example.org/a/"), "https://example.org/a/");
        assert_eq!(key("https://example.org/a"), "https://example.org/a");
        assert_ne!(key("https://example.org/a"), key("https://example.org/a/"));
    }

    #[test]
    fn trailing_slash_can_be_stripped() {
        let strip = with_slash(TrailingSlash::Strip);

        assert_eq!(
            key_with("https://example.org/a/", &strip),
            "https://example.org/a"
        );
        assert_eq!(
            key_with("https://example.org/a/b/?x=1", &strip),
            "https://example.org/a/b?x=1"
        );
        assert_eq!(
            key_with("https://example.org/a", &strip),
            "https://example.org/a"
        );
        // The root path is never touched.
        assert_eq!(
            key_with("https://example.org/", &strip),
            "https://example.org/"
        );
        assert_eq!(
            key_with("https://example.org", &strip),
            "https://example.org/"
        );
    }

    #[test]
    fn trailing_slash_can_be_added_to_directory_like_paths() {
        let add = with_slash(TrailingSlash::Add);

        assert_eq!(
            key_with("https://example.org/a", &add),
            "https://example.org/a/"
        );
        assert_eq!(
            key_with("https://example.org/a/b?x=1", &add),
            "https://example.org/a/b/?x=1"
        );
        assert_eq!(
            key_with("https://example.org/a/", &add),
            "https://example.org/a/"
        );
        assert_eq!(
            key_with("https://example.org/page.html", &add),
            "https://example.org/page.html"
        );
        assert_eq!(
            key_with("https://example.org/", &add),
            "https://example.org/"
        );
    }

    #[test]
    fn normalization_is_idempotent() {
        let options = with_slash(TrailingSlash::Strip);
        for text in [
            "HTTPS://Example.ORG:443/a/b/?utm_source=x&b=2&a=1#frag",
            "http://example.org/p?tag=z&tag=a",
            "https://example.org",
            "https://example.org/s?q=a+b&q=caf%C3%A9",
        ] {
            let once = key_with(text, &options);
            let twice = key_with(&once, &options);
            assert_eq!(once, twice, "{text}");
        }
    }

    #[test]
    fn the_request_url_is_the_discovered_url_without_its_fragment() {
        let discovered =
            Url::parse("https://Example.org/p?utm_source=news&b=2&a=1#reviews").unwrap();

        let request = request_url(&discovered);

        // Parameter order and the click tag are untouched.
        assert_eq!(
            request.as_str(),
            "https://example.org/p?utm_source=news&b=2&a=1"
        );
        assert_eq!(
            normalize(&request, &NormalizeOptions::default()),
            normalize(&discovered, &NormalizeOptions::default())
        );
        assert_ne!(
            normalize(&discovered, &NormalizeOptions::default()).as_str(),
            request.as_str()
        );
    }

    #[test]
    fn options_have_documented_defaults_and_read_from_config() {
        let defaults = NormalizeOptions::default();
        assert_eq!(defaults.trailing_slash, TrailingSlash::Keep);
        assert!(defaults.tracking_params.contains(&"utm_*".to_owned()));
        assert!(!defaults.tracking_params.contains(&"ref".to_owned()));

        let empty: NormalizeOptions = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, defaults);

        let custom: NormalizeOptions =
            serde_json::from_str(r#"{"tracking_params":["x"],"trailing_slash":"strip"}"#).unwrap();
        assert_eq!(custom.tracking_params, ["x"]);
        assert_eq!(custom.trailing_slash, TrailingSlash::Strip);
        assert!(serde_json::from_str::<NormalizeOptions>(r#"{"trailing":"strip"}"#).is_err());

        assert_string_enum(
            TrailingSlash::ALL,
            TrailingSlash::as_str,
            &["keep", "strip", "add"],
        );
    }

    #[test]
    fn keys_print_and_serialize_as_plain_text() {
        let key: UrlKey = normalize(
            &Url::parse("https://example.org/a?b=1").unwrap(),
            &NormalizeOptions::default(),
        );

        assert_eq!(key.to_string(), "https://example.org/a?b=1");
        assert_eq!(format!("{key:?}"), "UrlKey(https://example.org/a?b=1)");
        assert_eq!(
            serde_json::to_string(&key).unwrap(),
            "\"https://example.org/a?b=1\""
        );
        assert_eq!(
            serde_json::from_str::<UrlKey>("\"https://example.org/a?b=1\"").unwrap(),
            key
        );
    }

    /// A link resolved in a document of a legacy encoding (F161): its query
    /// in that encoding, a character it lacks as `%26%23…%3B`, the path and
    /// a fragment in UTF-8, a UTF-16 document's query in UTF-8, as the URL
    /// standard and both browsers write them.
    #[test]
    fn a_links_query_is_written_in_its_documents_encoding() {
        let base = Url::parse("http://shop.example/dir/page?x=1").unwrap();
        let joined = |href: &str, encoding| join_in(&base, href, encoding).unwrap().to_string();
        assert_eq!(
            joined("f\u{fc}r?q=f\u{fc}r#f\u{fc}r", encoding_rs::WINDOWS_1252),
            "http://shop.example/dir/f%C3%BCr?q=f%FCr#f%C3%BCr"
        );
        assert_eq!(
            joined("/s?q=\u{65e5}\u{672c}&r=\u{20ac}", encoding_rs::SHIFT_JIS),
            "http://shop.example/s?q=%93%FA%96{&r=%26%238364%3B"
        );
        assert_eq!(
            joined("?q=\u{65e5}a\u{672c}", encoding_rs::WINDOWS_1252),
            "http://shop.example/dir/page?q=%26%2326085%3Ba%26%2326412%3B"
        );
        for encoding in [
            encoding_rs::UTF_8,
            encoding_rs::UTF_16LE,
            encoding_rs::UTF_16BE,
        ] {
            assert_eq!(
                joined("/s?q=f\u{fc}r", encoding),
                "http://shop.example/s?q=f%C3%BCr",
                "{}",
                encoding.name()
            );
        }
        // A long query fills the encoder's buffer more than once.
        let long = "\u{e9}".repeat(3000);
        let url = join_in(&base, &format!("/s?q={long}"), encoding_rs::WINDOWS_1252).unwrap();
        assert_eq!(url.query().unwrap(), format!("q={}", "%E9".repeat(3000)));
        assert_eq!(
            join_in(&base, "http://[::1", encoding_rs::WINDOWS_1252),
            None
        );
    }
}
