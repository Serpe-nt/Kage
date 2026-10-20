//! The key of the response cache (spec §15): what makes two requests one.
//!
//! A result may be served to a request only when the request that fetched
//! it asked for the same thing. The key therefore holds everything that
//! changes what a result holds, and nothing that only changes when or how
//! politely a page is fetched:
//!
//! - the address, normalized as a crawl normalizes it ([`crate::urlnorm`]);
//! - the method, the body, the kind of request and the caller's headers;
//! - the level (spec §25): the same page read by a plain client and by a
//!   person are two results, whatever they hold;
//! - the include set, in any order (a result fetched with more parts is
//!   not cut down to serve a request for fewer);
//! - the options that shape what is captured (what the browser waits for,
//!   what the persona does on the page, the screenshot, the PDF, the
//!   captured responses, the extraction, the size limit) and the persona
//!   hints, all as asked;
//! - whose it is: the API key, or nobody's when the site's policy says its
//!   pages are the same for everybody.
//!
//! Not in it: how long a result may be served for, the timeout, the
//! retries, the pacing, what to do about `robots.txt`.

use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::domain::Level;
use crate::ids::ApiKeyId;
use crate::urlnorm::{NormalizeOptions, normalize};

/// The version of the key: raised when what goes into it changes, so that
/// an entry written under an older meaning is never served. 2 since the
/// level replaced the render mode (2.0).
const VERSION: u32 = 2;

/// Everything that makes two requests one to the cache.
#[derive(Debug, Clone)]
pub struct CacheKey<'a> {
    /// The page.
    pub url: &'a Url,
    /// `GET`, `HEAD` or `POST`.
    pub method: &'a str,
    /// The request body.
    pub body: Option<&'a [u8]>,
    /// What the request is on the http path.
    pub request_kind: &'a str,
    /// The caller's headers.
    pub headers: &'a [(String, String)],
    /// The level the request named.
    pub level: Level,
    /// The parts of the result that were asked for.
    pub include: &'a [&'a str],
    /// The options that shape what is captured, as the request wrote them.
    pub options: &'a Value,
    /// The API key the result belongs to; `None` when the site's policy
    /// shares its pages between keys.
    pub scope: Option<ApiKeyId>,
}

/// A key and the normalized address it was made for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest256 {
    /// The key: 64 hexadecimal characters.
    pub key: String,
    /// The normalized address, for inspection.
    pub url_key: String,
}

/// Writes a JSON value with the members of every object in the order of
/// their names, so that two values that say the same are the same bytes.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(members) => {
            let mut sorted: Vec<(&String, &Value)> = members.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (index, (name, member)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(name.clone()).to_string());
                out.push(':');
                canonical(member, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        // A member that says nothing is the member left out.
        other => out.push_str(&other.to_string()),
    }
}

/// A JSON value without the members that are null: a request that leaves
/// an option out and one that writes `null` for it ask for the same.
fn without_nulls(value: &Value) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .filter(|(_, member)| !member.is_null())
                .map(|(name, member)| (name.clone(), without_nulls(member)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_nulls).collect()),
        other => other.clone(),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

impl CacheKey<'_> {
    /// The key of the request under the normalization in force.
    pub fn digest(&self, normalization: &NormalizeOptions) -> Digest256 {
        let url_key = normalize(self.url, normalization).as_str().to_owned();
        let mut include: Vec<&str> = self.include.to_vec();
        include.sort_unstable();
        include.dedup();
        // Header names are no matter of case; their order is the caller's
        // and a browser's tables decide where they go.
        let mut headers: Vec<(String, &str)> = self
            .headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), value.as_str()))
            .collect();
        headers.sort();
        let said = serde_json::json!({
            "v": VERSION,
            "url": url_key,
            "method": self.method.to_ascii_uppercase(),
            "body": self.body.map(|body| hex(&Sha256::digest(body))),
            "kind": self.request_kind,
            "headers": headers,
            "level": self.level.number(),
            "include": include,
            "options": without_nulls(self.options),
            "scope": self.scope.map(|key| key.to_string()),
        });
        let mut written = String::new();
        canonical(&said, &mut written);
        Digest256 {
            key: hex(&Sha256::digest(written.as_bytes())),
            url_key,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use url::Url;

    use super::{CacheKey, canonical, hex, without_nulls};
    use crate::domain::Level;
    use crate::ids::ApiKeyId;
    use crate::urlnorm::NormalizeOptions;

    struct Request {
        url: Url,
        method: &'static str,
        body: Option<Vec<u8>>,
        kind: &'static str,
        headers: Vec<(String, String)>,
        level: Level,
        include: Vec<&'static str>,
        options: Value,
        scope: Option<ApiKeyId>,
    }

    impl Request {
        fn plain() -> Self {
            Self {
                url: Url::parse("https://shop.example/list?page=2").unwrap(),
                method: "GET",
                body: None,
                kind: "navigation",
                headers: Vec::new(),
                level: Level::Human,
                include: vec!["dom", "headers"],
                options: json!({}),
                scope: Some(ApiKeyId::new()),
            }
        }

        fn key(&self) -> String {
            self.digest().0
        }

        fn digest(&self) -> (String, String) {
            let digest = CacheKey {
                url: &self.url,
                method: self.method,
                body: self.body.as_deref(),
                request_kind: self.kind,
                headers: &self.headers,
                level: self.level,
                include: &self.include,
                options: &self.options,
                scope: self.scope,
            }
            .digest(&NormalizeOptions::default());
            (digest.key, digest.url_key)
        }
    }

    fn with(base: &Request, change: impl FnOnce(&mut Request)) -> String {
        let mut request = Request {
            url: base.url.clone(),
            method: base.method,
            body: base.body.clone(),
            kind: base.kind,
            headers: base.headers.clone(),
            level: base.level,
            include: base.include.clone(),
            options: base.options.clone(),
            scope: base.scope,
        };
        change(&mut request);
        request.key()
    }

    #[test]
    fn a_key_is_a_hash_of_the_request_and_names_its_address() {
        let request = Request::plain();
        let (key, url_key) = request.digest();
        assert_eq!(key.len(), 64);
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(url_key, "https://shop.example/list?page=2");
        // The same request, the same key.
        assert_eq!(request.key(), key);
    }

    #[test]
    fn what_a_crawl_calls_one_address_is_one_key() {
        let base = Request::plain();
        let same = |url: &str| with(&base, |request| request.url = Url::parse(url).unwrap());
        // A tracking tag, a fragment, the case of the host, the order of
        // the query: one page.
        for url in [
            "https://shop.example/list?page=2&utm_source=mail",
            "https://shop.example/list?page=2#top",
            "https://SHOP.example:443/list?page=2",
        ] {
            assert_eq!(same(url), base.key(), "{url}");
        }
        let two = |url: &str| with(&base, |request| request.url = Url::parse(url).unwrap());
        assert_eq!(
            two("https://shop.example/list?b=1&a=2"),
            two("https://shop.example/list?a=2&b=1")
        );
        // Another page, another scheme, another host: other keys.
        for url in [
            "https://shop.example/list?page=3",
            "http://shop.example/list?page=2",
            "https://www.shop.example/list?page=2",
            "https://shop.example/list/?page=2",
        ] {
            assert_ne!(same(url), base.key(), "{url}");
        }
    }

    #[test]
    fn everything_that_changes_what_a_result_holds_changes_the_key() {
        type Change = fn(&mut Request);
        let base = Request::plain();
        let changes: [(&str, Change); 12] = [
            ("the method", |request| request.method = "POST"),
            ("a body", |request| request.body = Some(b"a=1".to_vec())),
            ("the kind of request", |request| request.kind = "fetch"),
            ("a header", |request| {
                request.headers = vec![("Accept".to_owned(), "application/json".to_owned())];
            }),
            ("another level", |request| {
                request.level = Level::Fast;
            }),
            ("the plain level", |request| {
                request.level = Level::Plain;
            }),
            ("a part more", |request| request.include.push("extract")),
            ("a part fewer", |request| request.include = vec!["dom"]),
            ("an option", |request| {
                request.options = json!({ "wait": { "until": "network_idle" } });
            }),
            ("a persona hint", |request| {
                request.options = json!({ "persona": { "countries": ["DE"] } });
            }),
            ("another key", |request| {
                request.scope = Some(ApiKeyId::new());
            }),
            ("nobody's", |request| request.scope = None),
        ];
        let mut keys = vec![base.key()];
        for (what, change) in changes {
            let key = with(&base, change);
            assert!(!keys.contains(&key), "{what} gave a key seen before");
            keys.push(key);
        }
        // Two bodies, two keys; two values of a header, two keys.
        assert_ne!(
            with(&base, |request| request.body = Some(b"a=1".to_vec())),
            with(&base, |request| request.body = Some(b"a=2".to_vec()))
        );
        assert_ne!(
            with(&base, |request| request.body = Some(Vec::new())),
            base.key()
        );
    }

    #[test]
    fn how_a_request_is_written_does_not_change_the_key() {
        let base = Request::plain();
        // The include set in any order, a part named twice.
        assert_eq!(
            with(&base, |request| request.include =
                vec!["headers", "dom", "dom"]),
            base.key()
        );
        // The method and the names of headers in any case, headers in any
        // order.
        assert_eq!(with(&base, |request| request.method = "get"), base.key());
        let headers = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect();
            with(&base, |request| request.headers = pairs)
        };
        assert_eq!(
            headers(&[("Accept", "text/html"), ("Referer", "https://a.example/")]),
            headers(&[("referer", "https://a.example/"), ("ACCEPT", "text/html")])
        );
        // The value of a header is its own.
        assert_ne!(
            headers(&[("Accept", "text/html")]),
            headers(&[("Accept", "TEXT/HTML")])
        );
        // The members of an option in any order, and one that is null is
        // one left out.
        let options = |options: Value| with(&base, |request| request.options = options);
        assert_eq!(
            options(json!({ "wait": { "until": "load", "extra_ms": 5 }, "xhr": null })),
            options(json!({ "wait": { "extra_ms": 5, "until": "load", "selector": null } }))
        );
        assert_eq!(options(json!({ "pdf": null })), base.key());
        // The order of a list is what the request said.
        assert_ne!(
            options(json!({ "persona": { "countries": ["DE", "AT"] } })),
            options(json!({ "persona": { "countries": ["AT", "DE"] } }))
        );
    }

    #[test]
    fn small_things() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        let mut written = String::new();
        canonical(
            &json!({ "b": [1, { "z": null, "a": "x\"y" }], "a": true }),
            &mut written,
        );
        assert_eq!(written, r#"{"a":true,"b":[1,{"a":"x\"y","z":null}]}"#);
        assert_eq!(
            without_nulls(&json!({ "a": null, "b": [null, { "c": null, "d": 1 }] })),
            json!({ "b": [null, { "d": 1 }] })
        );
    }
}
