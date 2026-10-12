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
//! as it was discovered, minus the fragment ([`request_url`]): a sorted
//! query string or a missing click id is a URL shape the site's own links
//! never produce, and that is a cheap tell. [`UrlKey`] therefore cannot be
//! turned back into a `Url`.

use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::macros::string_enum;

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

/// Normalizes a URL into its key.
///
/// The `url` crate already lower-cases the scheme and the host, converts an
/// internationalized host to punycode, drops default ports and resolves `.`
/// and `..` segments when it parses.
pub fn normalize(url: &Url, options: &NormalizeOptions) -> UrlKey {
    let mut key = url.clone();
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
/// the fragment, which browsers never send.
pub fn request_url(discovered: &Url) -> Url {
    let mut url = discovered.clone();
    url.set_fragment(None);
    url
}

#[cfg(test)]
mod properties;

#[cfg(test)]
mod tests {
    use url::Url;

    use super::{NormalizeOptions, TrailingSlash, UrlKey, normalize, request_url};
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
}
