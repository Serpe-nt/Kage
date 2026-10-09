//! Configuration of the http path (spec §9), of the fingerprint self-test
//! (spec §7.4) and of the browser majors the engines bundle (spec §6.1).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::duration::HumanDuration;
use crate::macros::string_enum;

/// Most redirect hops that can be configured. Browsers stop at 20.
pub const MAX_REDIRECTS_LIMIT: u32 = 20;

/// Most requests a browser runs at once against one host.
pub const MAX_SUBRESOURCE_CONCURRENCY: u32 = 6;

string_enum! {
    /// Which of a page's assets count as its own.
    pub enum SubresourceScope {
        /// Assets on the page's site: its registrable domain, any host.
        SameSite => "same_site",
        /// Assets on the page's exact origin only.
        SameOrigin => "same_origin",
    }
}

/// Subresource emulation (spec §9): after a document, fetch some of the
/// assets a browser would fetch for its first paint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubresourcesConfig {
    /// Which assets are fetched. Assets of other sites never are: their
    /// host would see a download that no script ever follows up on.
    #[serde(default = "default_scope")]
    pub scope: SubresourceScope,
    /// Most assets fetched for one page, the icon included.
    #[serde(default = "default_subresource_max")]
    pub max: u32,
    /// Most assets in flight at once.
    #[serde(default = "default_subresource_concurrency")]
    pub max_concurrent: u32,
    /// An asset is read up to this many bytes, then abandoned.
    #[serde(default = "default_subresource_bytes_each")]
    pub max_bytes_each: u64,
    /// All assets of one page together are read up to this many bytes.
    #[serde(default = "default_subresource_bytes_total")]
    pub max_bytes_total: u64,
}

const fn default_scope() -> SubresourceScope {
    SubresourceScope::SameSite
}

const fn default_subresource_max() -> u32 {
    12
}

const fn default_subresource_concurrency() -> u32 {
    MAX_SUBRESOURCE_CONCURRENCY
}

const fn default_subresource_bytes_each() -> u64 {
    2_000_000
}

const fn default_subresource_bytes_total() -> u64 {
    8_000_000
}

impl Default for SubresourcesConfig {
    fn default() -> Self {
        Self {
            scope: default_scope(),
            max: default_subresource_max(),
            max_concurrent: default_subresource_concurrency(),
            max_bytes_each: default_subresource_bytes_each(),
            max_bytes_total: default_subresource_bytes_total(),
        }
    }
}

/// The http path (spec §9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    /// Longest one attempt may take: every redirect hop and the body
    /// together (spec §8.4).
    #[serde(default = "default_http_timeout")]
    pub timeout: HumanDuration,
    /// Longest the setup of one connection may take: the tunnel through the
    /// proxy and the TLS handshake.
    #[serde(default = "default_http_connect_timeout")]
    pub connect_timeout: HumanDuration,
    /// How long an unused connection is kept open for the next request.
    #[serde(default = "default_idle_connection_timeout")]
    pub idle_connection_timeout: HumanDuration,
    /// Redirect hops followed before the attempt fails.
    #[serde(default = "default_max_redirects")]
    pub max_redirects: u32,
    /// A response body is read up to this many bytes; the rest is dropped
    /// and the result says so. A request may ask for less.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
    /// Session cookies are forgotten when a cookie jar was not used for
    /// this long: the user closed the browser.
    #[serde(default = "default_session_cookie_idle")]
    pub session_cookie_idle: HumanDuration,
    /// Fetch a page's own stylesheets, scripts and icon after the page.
    #[serde(default)]
    pub fetch_subresources: bool,
    /// What subresource emulation fetches.
    #[serde(default)]
    pub subresources: SubresourcesConfig,
}

const fn default_http_timeout() -> HumanDuration {
    HumanDuration::from_secs(30)
}

const fn default_http_connect_timeout() -> HumanDuration {
    HumanDuration::from_secs(15)
}

const fn default_idle_connection_timeout() -> HumanDuration {
    HumanDuration::from_mins(5)
}

const fn default_max_redirects() -> u32 {
    10
}

const fn default_max_body_bytes() -> u64 {
    10_000_000
}

const fn default_session_cookie_idle() -> HumanDuration {
    HumanDuration::from_hours(12)
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            timeout: default_http_timeout(),
            connect_timeout: default_http_connect_timeout(),
            idle_connection_timeout: default_idle_connection_timeout(),
            max_redirects: default_max_redirects(),
            max_body_bytes: default_max_body_bytes(),
            session_cookie_idle: default_session_cookie_idle(),
            fetch_subresources: false,
            subresources: SubresourcesConfig::default(),
        }
    }
}

/// The fingerprint self-test of the http path (spec §7.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSelftestConfig {
    /// The operator's own echo (`kage echo serve`), reached through a
    /// proxy. Unset: the service runs no scheduled self-test, and the
    /// per-change and pre-release checks are the gate.
    #[serde(default)]
    pub echo_url: Option<String>,
    /// A PEM file with the certificate authority of that echo, when its
    /// certificate is not from a public one.
    #[serde(default)]
    pub echo_ca_file: Option<PathBuf>,
    /// An echo run by someone else. Only `kage selftest http` and the gated
    /// live suite ask it, never the service on a timer: it sees the exit
    /// address of every proxy that asks.
    #[serde(default = "default_third_party_url")]
    pub third_party_url: String,
    /// Time between two scheduled self-tests of one persona, before
    /// jitter.
    #[serde(default = "default_selftest_interval")]
    pub interval: HumanDuration,
    /// Timeout of one self-test request.
    #[serde(default = "default_selftest_timeout")]
    pub timeout: HumanDuration,
}

fn default_third_party_url() -> String {
    "https://tls.peet.ws/api/all".to_owned()
}

const fn default_selftest_interval() -> HumanDuration {
    HumanDuration::from_hours(24)
}

const fn default_selftest_timeout() -> HumanDuration {
    HumanDuration::from_secs(30)
}

impl Default for HttpSelftestConfig {
    fn default() -> Self {
        Self {
            echo_url: None,
            echo_ca_file: None,
            third_party_url: default_third_party_url(),
            interval: default_selftest_interval(),
            timeout: default_selftest_timeout(),
        }
    }
}

/// Self-tests (spec §7.4).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelftestConfig {
    /// The http path.
    #[serde(default)]
    pub http: HttpSelftestConfig,
}

/// One browser engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    /// The major version of the browser the engine image bundles. Set by
    /// the milestone that pins the browser (M4 for Chrome, M5 for Camoufox)
    /// and moved with every engine upgrade. A set major must resolve to an
    /// emulation profile, or `kage config check` fails: an engine upgrade
    /// must not switch the http path off unnoticed.
    #[serde(default)]
    pub browser_major: Option<u32>,
}

/// The browser engines (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnginesConfig {
    /// patchright with real Chrome.
    #[serde(default)]
    pub chromium: EngineConfig,
    /// Camoufox.
    #[serde(default)]
    pub firefox: EngineConfig,
}

fn https_url(key: &str, value: &str) -> Result<(), String> {
    match url::Url::parse(value) {
        Ok(url) if url.scheme() == "https" && url.host_str().is_some() => Ok(()),
        Ok(_) => Err(format!(
            "{key} must be an https URL: a fingerprint echo reads the TLS handshake"
        )),
        Err(error) => Err(format!("{key} is not a URL: {error}")),
    }
}

/// Checks the three sections of this module.
///
/// # Errors
///
/// The first problem, as a message that names the key.
pub(super) fn validate(
    http: &HttpConfig,
    selftest: &SelftestConfig,
    engines: &EnginesConfig,
) -> Result<(), String> {
    if http.timeout.get().is_zero() {
        return Err("http.timeout must be longer than zero".to_owned());
    }
    if http.connect_timeout.get().is_zero() {
        return Err("http.connect_timeout must be longer than zero".to_owned());
    }
    if http.max_redirects > MAX_REDIRECTS_LIMIT {
        return Err(format!(
            "http.max_redirects must be at most {MAX_REDIRECTS_LIMIT}"
        ));
    }
    if http.max_body_bytes == 0 {
        return Err("http.max_body_bytes must be at least 1".to_owned());
    }
    let subresources = &http.subresources;
    if subresources.max > 64 {
        return Err("http.subresources.max must be at most 64".to_owned());
    }
    if !(1..=MAX_SUBRESOURCE_CONCURRENCY).contains(&subresources.max_concurrent) {
        return Err(format!(
            "http.subresources.max_concurrent must be between 1 and {MAX_SUBRESOURCE_CONCURRENCY}"
        ));
    }
    if subresources.max_bytes_each == 0 || subresources.max_bytes_total == 0 {
        return Err("http.subresources byte limits must be at least 1".to_owned());
    }
    if let Some(echo_url) = &selftest.http.echo_url {
        https_url("selftest.http.echo_url", echo_url)?;
    }
    https_url(
        "selftest.http.third_party_url",
        &selftest.http.third_party_url,
    )?;
    if selftest.http.timeout.get().is_zero() {
        return Err("selftest.http.timeout must be longer than zero".to_owned());
    }
    if selftest.http.interval.get() < std::time::Duration::from_secs(60) {
        return Err("selftest.http.interval must be at least 1m".to_owned());
    }
    for (key, engine) in [
        ("engines.chromium.browser_major", engines.chromium),
        ("engines.firefox.browser_major", engines.firefox),
    ] {
        if engine.browser_major == Some(0) {
            return Err(format!("{key} must be at least 1"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        EngineConfig, EnginesConfig, HttpConfig, HttpSelftestConfig, SelftestConfig,
        SubresourceScope, validate,
    };
    use crate::duration::HumanDuration;

    type Change = Box<dyn FnOnce(&mut HttpConfig, &mut SelftestConfig, &mut EnginesConfig)>;

    fn check(
        change: impl FnOnce(&mut HttpConfig, &mut SelftestConfig, &mut EnginesConfig),
    ) -> Result<(), String> {
        let mut http = HttpConfig::default();
        let mut selftest = SelftestConfig::default();
        let mut engines = EnginesConfig::default();
        change(&mut http, &mut selftest, &mut engines);
        validate(&http, &selftest, &engines)
    }

    #[test]
    fn the_defaults_are_the_specs() {
        let http = HttpConfig::default();

        assert_eq!(http.timeout.get(), Duration::from_secs(30));
        assert_eq!(http.max_redirects, 10);
        assert_eq!(http.max_body_bytes, 10_000_000);
        assert_eq!(http.session_cookie_idle.get(), Duration::from_hours(12));
        assert!(!http.fetch_subresources);
        assert_eq!(http.subresources.scope, SubresourceScope::SameSite);
        assert_eq!(http.subresources.max, 12);
        assert_eq!(http.subresources.max_concurrent, 6);

        let selftest = HttpSelftestConfig::default();
        assert_eq!(selftest.echo_url, None);
        assert_eq!(selftest.third_party_url, "https://tls.peet.ws/api/all");
        assert_eq!(selftest.interval.get(), Duration::from_hours(24));
        assert_eq!(EnginesConfig::default().chromium.browser_major, None);
        assert_eq!(check(|_, _, _| {}), Ok(()));
    }

    #[test]
    fn values_out_of_range_are_named() {
        let cases: [(&str, Change); 13] = [
            (
                "http.timeout",
                Box::new(|http, _, _| http.timeout = HumanDuration::from_secs(0)),
            ),
            (
                "http.connect_timeout",
                Box::new(|http, _, _| http.connect_timeout = HumanDuration::from_secs(0)),
            ),
            (
                "http.max_redirects",
                Box::new(|http, _, _| http.max_redirects = 21),
            ),
            (
                "http.max_body_bytes",
                Box::new(|http, _, _| http.max_body_bytes = 0),
            ),
            (
                "http.subresources.max ",
                Box::new(|http, _, _| http.subresources.max = 65),
            ),
            (
                "http.subresources.max_concurrent",
                Box::new(|http, _, _| http.subresources.max_concurrent = 0),
            ),
            (
                "http.subresources.max_concurrent",
                Box::new(|http, _, _| http.subresources.max_concurrent = 7),
            ),
            (
                "http.subresources byte limits",
                Box::new(|http, _, _| http.subresources.max_bytes_total = 0),
            ),
            (
                "selftest.http.echo_url",
                Box::new(|_, selftest, _| {
                    selftest.http.echo_url = Some("http://echo.example.org/".to_owned());
                }),
            ),
            (
                "selftest.http.third_party_url",
                Box::new(|_, selftest, _| selftest.http.third_party_url = "not a url".to_owned()),
            ),
            (
                "selftest.http.timeout",
                Box::new(|_, selftest, _| selftest.http.timeout = HumanDuration::from_secs(0)),
            ),
            (
                "selftest.http.interval",
                Box::new(|_, selftest, _| selftest.http.interval = HumanDuration::from_secs(5)),
            ),
            (
                "engines.firefox.browser_major",
                Box::new(|_, _, engines| {
                    engines.firefox = EngineConfig {
                        browser_major: Some(0),
                    };
                }),
            ),
        ];
        for (key, change) in cases {
            let error = check(change).unwrap_err();
            assert!(error.starts_with(key.trim_end()), "{key}: {error}");
        }

        assert_eq!(
            check(|http, selftest, engines| {
                http.max_redirects = 0;
                selftest.http.echo_url = Some("https://echo.example.org/".to_owned());
                engines.chromium.browser_major = Some(156);
            }),
            Ok(())
        );
    }
}
