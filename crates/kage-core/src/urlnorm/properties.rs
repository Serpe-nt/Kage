//! Properties of URL normalization (spec §13): what must hold for every
//! URL, not only for the examples next door.

use proptest::prelude::*;
use url::Url;

use super::{NormalizeOptions, TrailingSlash, normalize, request_url};

/// A query parameter that selects content: its name is on no tracking list.
fn content_param() -> impl Strategy<Value = (String, String)> {
    (
        prop::sample::select(vec![
            "id", "page", "q", "sort", "ref", "lang", "tag", "sid", "View", "x-y", "a.b", "utm",
        ]),
        // No space: one at the very end of a URL is cut off by whoever
        // reads the URL, before anything is normalized.
        "[a-zA-Z0-9_.~+%-]{0,8}",
    )
        .prop_map(|(name, value)| (name.to_owned(), value))
}

/// A query parameter that only says where a visit came from.
fn tracking_param() -> impl Strategy<Value = (String, String)> {
    (
        prop::sample::select(vec![
            "utm_source",
            "utm_medium",
            "UTM_Campaign",
            "utm_anything",
            "gclid",
            "Fbclid",
            "msclkid",
            "_ga",
            "mc_eid",
        ]),
        "[a-zA-Z0-9]{0,10}",
    )
        .prop_map(|(name, value)| (name.to_owned(), value))
}

fn segment() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-zA-Z0-9_~-]{1,8}",
        // Encoded octets in both cases, encoded unreserved characters, a
        // reserved one that must stay encoded, and letters outside ASCII.
        Just("caf%c3%A9".to_owned()),
        Just("%7Euser".to_owned()),
        Just("a%2Fb".to_owned()),
        Just("größe".to_owned()),
        Just("page.html".to_owned()),
        Just("100%".to_owned()),
    ]
}

/// The parts a URL is put together from.
#[derive(Debug, Clone)]
struct Parts {
    scheme: &'static str,
    host: String,
    port: Option<u16>,
    segments: Vec<String>,
    trailing_slash: bool,
    params: Vec<(String, String)>,
    fragment: Option<String>,
}

impl Parts {
    fn write(&self) -> String {
        let mut text = match self.port {
            Some(port) => format!("{}://{}:{port}", self.scheme, self.host),
            None => format!("{}://{}", self.scheme, self.host),
        };
        for segment in &self.segments {
            text.push('/');
            text.push_str(segment);
        }
        if self.trailing_slash || self.segments.is_empty() {
            text.push('/');
        }
        if !self.params.is_empty() {
            let query: Vec<String> = self
                .params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            text.push('?');
            text.push_str(&query.join("&"));
        }
        if let Some(fragment) = &self.fragment {
            text.push('#');
            text.push_str(fragment);
        }
        text
    }

    fn url(&self) -> Url {
        Url::parse(&self.write()).expect("the parts make a URL")
    }
}

fn parts() -> impl Strategy<Value = Parts> {
    (
        prop::sample::select(vec!["http", "https"]),
        prop::sample::select(vec![
            "example.org",
            "www.example.org",
            "shop.example.co.uk",
            "bücher.example",
            "a-b.c1.example",
        ]),
        prop::option::of(prop::sample::select(vec![8080_u16, 8443, 81])),
        prop::collection::vec(segment(), 0..4),
        any::<bool>(),
        prop::collection::vec(content_param(), 0..4),
        prop::option::of("[a-z0-9/-]{0,6}"),
    )
        .prop_map(
            |(scheme, host, port, segments, trailing_slash, params, fragment)| Parts {
                scheme,
                host: host.to_owned(),
                port,
                segments,
                trailing_slash,
                params,
                fragment,
            },
        )
}

fn options() -> impl Strategy<Value = NormalizeOptions> {
    prop::sample::select(vec![
        TrailingSlash::Keep,
        TrailingSlash::Strip,
        TrailingSlash::Add,
    ])
    .prop_map(|trailing_slash| NormalizeOptions {
        trailing_slash,
        ..NormalizeOptions::default()
    })
}

fn key(parts: &Parts, options: &NormalizeOptions) -> String {
    normalize(&parts.url(), options).as_str().to_owned()
}

/// A string with the case of its letters changed by a mask.
fn recased(text: &str, mask: u64) -> String {
    text.chars()
        .enumerate()
        .map(|(index, c)| {
            if mask >> (index % 64) & 1 == 1 {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A key is a URL, and normalizing it changes nothing: whatever has
    /// one key keeps it.
    #[test]
    fn a_key_is_its_own_key(parts in parts(), options in options()) {
        let once = key(&parts, &options);
        let again = normalize(&Url::parse(&once).expect("a key is a URL"), &options);
        prop_assert_eq!(again.as_str(), once);
    }

    /// The fragment never reaches a server, so it never reaches a key.
    #[test]
    fn the_fragment_is_no_part_of_a_key(
        parts in parts(),
        fragment in prop::option::of("[a-z0-9/?=&-]{0,8}"),
        options in options(),
    ) {
        let other = Parts { fragment, ..parts.clone() };
        prop_assert_eq!(key(&other, &options), key(&parts, &options));
        prop_assert!(!key(&parts, &options).contains('#'));
    }

    /// The order of the parameters is how a page wrote its link, not which
    /// page it is.
    #[test]
    fn the_order_of_the_query_is_no_part_of_a_key(
        parts in parts(),
        rotate in 0_usize..4,
        reverse in any::<bool>(),
        options in options(),
    ) {
        let mut params = parts.params.clone();
        if !params.is_empty() {
            let by = rotate % params.len();
            params.rotate_left(by);
        }
        if reverse {
            params.reverse();
        }
        let other = Parts { params, ..parts.clone() };
        prop_assert_eq!(key(&other, &options), key(&parts, &options));
    }

    /// Campaign tags and click ids say where a visit came from. Wherever
    /// they stand in a query, the page is the same one.
    #[test]
    fn tracking_parameters_are_no_part_of_a_key(
        parts in parts(),
        tags in prop::collection::vec((tracking_param(), 0_usize..5), 1..4),
        options in options(),
    ) {
        let mut params = parts.params.clone();
        for (tag, at) in tags {
            params.insert(at.min(params.len()), tag);
        }
        let other = Parts { params, ..parts.clone() };
        prop_assert_eq!(key(&other, &options), key(&parts, &options));
        let written = Url::parse(&key(&other, &options)).expect("a key is a URL");
        for (name, _) in written.query_pairs() {
            let name = name.to_ascii_lowercase();
            prop_assert!(!name.starts_with("utm_") && !name.ends_with("clid") && !name.starts_with('_'), "{}", name);
        }
    }

    /// A parameter that selects content makes another page, whatever else
    /// the query holds.
    #[test]
    fn a_content_parameter_is_part_of_a_key(
        parts in parts(),
        value in "[a-z0-9]{1,6}",
        options in options(),
    ) {
        let mut params = parts.params.clone();
        params.push(("variant".to_owned(), value));
        let other = Parts { params, ..parts.clone() };
        prop_assert_ne!(key(&other, &options), key(&parts, &options));
    }

    /// Scheme and host are written in lower case, and a host is the same
    /// host however its letters are cased.
    #[test]
    fn the_case_of_scheme_and_host_is_no_part_of_a_key(
        parts in parts(),
        mask in any::<u64>(),
        options in options(),
    ) {
        // Only ASCII letters are recased: the case rules of other letters
        // are the URL library's.
        let host = if parts.host.is_ascii() {
            recased(&parts.host, mask)
        } else {
            parts.host.clone()
        };
        let other = Parts { host, ..parts.clone() }
            .write()
            .replacen(parts.scheme, &recased(parts.scheme, mask >> 32), 1);
        let expected = key(&parts, &options);
        let recased_key = normalize(&Url::parse(&other).expect("still a URL"), &options);
        prop_assert_eq!(recased_key.as_str(), expected.as_str());
        let start = expected.split('/').take(3).collect::<Vec<_>>().join("/");
        prop_assert_eq!(start.to_lowercase(), start.clone());
        prop_assert!(start.is_ascii(), "{}", start);
    }

    /// The port a scheme implies says nothing; any other port is another
    /// server.
    #[test]
    fn the_default_port_is_no_part_of_a_key(parts in parts(), options in options()) {
        let bare = Parts { port: None, ..parts.clone() };
        let implied = if parts.scheme == "https" { 443 } else { 80 };
        let explicit = Parts { port: Some(implied), ..parts.clone() };
        prop_assert_eq!(key(&explicit, &options), key(&bare, &options));
        let elsewhere = Parts { port: Some(8081), ..parts.clone() };
        prop_assert_ne!(key(&elsewhere, &options), key(&bare, &options));
        // And the scheme is part of the key: another scheme, another page.
        let other_scheme = Parts {
            scheme: if parts.scheme == "https" { "http" } else { "https" },
            port: None,
            ..parts.clone()
        };
        prop_assert_ne!(key(&other_scheme, &options), key(&bare, &options));
    }

    /// How an octet is written in hexadecimal is not which octet it is.
    #[test]
    fn the_case_of_percent_encoding_is_no_part_of_a_key(
        parts in parts(),
        options in options(),
    ) {
        let lower = Parts {
            segments: parts.segments.iter().map(|segment| segment.replace("%c3%A9", "%c3%a9").replace("%7E", "%7e").replace("%2F", "%2f")).collect(),
            ..parts.clone()
        };
        prop_assert_eq!(key(&lower, &options), key(&parts, &options));
        // An encoded reserved character is never decoded into a separator.
        let written = Url::parse(&key(&parts, &options)).expect("a key is a URL");
        let segments = written.path().matches("a%2Fb").count();
        let wanted = parts.segments.iter().filter(|segment| *segment == "a%2Fb").count();
        prop_assert_eq!(segments, wanted);
    }

    /// The trailing-slash policy: kept apart, folded to none, or folded to
    /// one, and never touching the root.
    #[test]
    fn the_trailing_slash_follows_its_policy(parts in parts()) {
        prop_assume!(!parts.segments.is_empty());
        let with = Parts { trailing_slash: true, ..parts.clone() };
        let without = Parts { trailing_slash: false, ..parts.clone() };
        let policy = |trailing_slash| NormalizeOptions {
            trailing_slash,
            ..NormalizeOptions::default()
        };
        let path_of = |key: &str| Url::parse(key).expect("a key is a URL").path().to_owned();

        let keep = policy(TrailingSlash::Keep);
        prop_assert_ne!(key(&with, &keep), key(&without, &keep));

        let strip = policy(TrailingSlash::Strip);
        prop_assert_eq!(key(&with, &strip), key(&without, &strip));
        prop_assert!(!path_of(&key(&with, &strip)).ends_with('/'));

        let add = policy(TrailingSlash::Add);
        let last = parts.segments.last().map_or("", String::as_str);
        if last.contains('.') {
            // A file name is a file: nothing is added to it.
            prop_assert!(!path_of(&key(&without, &add)).ends_with('/'));
        } else {
            prop_assert_eq!(key(&with, &add), key(&without, &add));
            prop_assert!(path_of(&key(&without, &add)).ends_with('/'));
        }
    }

    /// The root is the root under every policy.
    #[test]
    fn the_root_path_is_never_touched(parts in parts(), options in options()) {
        let root = Parts { segments: Vec::new(), ..parts };
        let path = Url::parse(&key(&root, &options)).expect("a key is a URL").path().to_owned();
        prop_assert_eq!(path, "/");
    }

    /// A query without content parameters leaves no question mark behind.
    #[test]
    fn an_empty_query_disappears(
        parts in parts(),
        tags in prop::collection::vec(tracking_param(), 0..3),
        options in options(),
    ) {
        let only_tags = Parts { params: tags, ..parts };
        prop_assert!(!key(&only_tags, &options).contains('?'));
    }

    /// What is asked of a server is the link as it was found, less its
    /// fragment; its key is the key of the link.
    #[test]
    fn the_request_url_is_the_link_without_its_fragment(
        parts in parts(),
        tags in prop::collection::vec(tracking_param(), 0..2),
        options in options(),
    ) {
        let mut params = parts.params.clone();
        params.extend(tags);
        let found = Parts { params, ..parts }.url();
        let asked = request_url(&found);
        prop_assert_eq!(asked.fragment(), None);
        let mut without = found.clone();
        without.set_fragment(None);
        prop_assert_eq!(asked.as_str(), without.as_str());
        prop_assert_eq!(normalize(&asked, &options), normalize(&found, &options));
    }
}
