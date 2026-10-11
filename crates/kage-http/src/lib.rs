//! The http path (spec §9): fetching pages with wreq so that the request is
//! a browser's at every layer a server can see without running JavaScript.
//!
//! - [`profile`]: the emulation profiles wreq-util ships, and the mapping
//!   from a persona's engine, browser major and operating system to one of
//!   them (spec §6.1 step 4).
//! - [`client`]: the [`ClientFactory`], which keeps one [`HttpClient`] per
//!   profile and proxy, with that proxy's cookies.
//! - [`fetch`]: [`HttpClient::fetch`], a navigation with its redirects, the
//!   SSRF guard on every hop, the body cap and per-phase timing.
//! - [`headers`]: which headers a request carries and in which order, for
//!   each browser family, kind of request, scheme and HTTP version; and the
//!   denylist for callers.
//! - [`cookies`]: the cookie jar, with a browser's storage and sending
//!   rules, persisted through `kage-store`.
//! - [`site`]: origins, sites and the referrer policy.
//! - [`subresources`]: subresource emulation, off by default.
//! - [`echo`]: the fingerprint echo, a TLS server that reports the JA4, the
//!   HTTP/2 fingerprint and the header order of the requests it receives.
//! - [`selftest`]: the fingerprint self-test (spec §7.4), which compares what
//!   an echo saw with the values recorded per profile.
//! - [`timing`]: how connection setup is split into tunnel and handshake.
//!
//! Every request goes through the client's proxy. Nothing in this crate
//! connects anywhere directly, and nothing in it runs JavaScript or
//! pretends to.

#![forbid(unsafe_code)]

pub mod client;
pub mod cookies;
pub mod echo;
pub mod error;
pub mod fetch;
pub mod headers;
pub mod profile;
pub mod selftest;
pub mod site;
pub mod subresources;
pub mod timing;

pub use client::{ClientFactory, ClientSpec, HopResolver, HttpClient, HttpSettings};
pub use cookies::{
    CookieBackend, CookieBackendError, CookieJar, JarOwner, MemoryCookies, StoreCookies,
};
pub use error::{HttpError, Stage};
pub use fetch::{FetchSpec, Fetched, Guard, Hop};
pub use headers::{CallerHeaders, FORWARDING_HEADERS, HeaderError, IdentityOverride, RequestKind};
pub use profile::{Family, ProfileChoice, ProfileError};
pub use subresources::SubresourceReport;

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Checks the parts of the configuration only this crate can judge: that
/// a browser major configured for an engine has an emulation profile (an
/// engine upgrade must not switch the http path off unnoticed), and that
/// the certificate authority of the operator's echo can be read.
/// `kage config check` calls it.
///
/// # Errors
///
/// The first problem, as a message that names the configuration key.
pub fn check_config(config: &kage_core::config::Config) -> Result<(), String> {
    profile::check_engine_majors(
        config.engines.chromium.browser_major,
        config.engines.firefox.browser_major,
    )?;
    if let Some(path) = &config.selftest.http.echo_ca_file {
        let pem = std::fs::read(path).map_err(|error| {
            format!(
                "selftest.http.echo_ca_file: cannot read {}: {error}",
                path.display()
            )
        })?;
        wreq::tls::trust::CertStore::builder()
            .add_stack_pem_certs(&pem)
            .build()
            .map_err(|_| {
                format!(
                    "selftest.http.echo_ca_file: {} holds no valid PEM certificate",
                    path.display()
                )
            })?;
    }
    Ok(())
}

/// What the http path can present as in this build and with this
/// configuration, for `kage config check`.
pub fn describe(config: &kage_core::config::Config) -> serde_json::Value {
    use kage_core::domain::{ClaimedOs, Engine};

    let engine = |engine: Engine, major: Option<u32>| {
        serde_json::json!({
            "browser_major": major,
            // The profile its personas would get; null means browser-only.
            "profile": major
                .and_then(|major| profile::resolve(engine, major, ClaimedOs::Windows))
                .map(|resolved| resolved.choice.name().to_owned()),
        })
    };
    serde_json::json!({
        "exact_profiles": {
            "chromium": profile::mapped_majors(Family::Chrome),
            "firefox": profile::mapped_majors(Family::Firefox),
        },
        "verified_majors": profile::VERIFIED_MAJORS
            .iter()
            .map(|(engine, major)| format!("{engine}-{major}"))
            .collect::<Vec<_>>(),
        // Majors a capture proved the newest profile does not fit: their
        // personas are browser-only on purpose.
        "browser_only_majors": profile::BROWSER_ONLY_MAJORS
            .iter()
            .map(|(engine, major)| format!("{engine}-{major}"))
            .collect::<Vec<_>>(),
        "browser_captures": selftest::browser_captures().len(),
        "engines": {
            "chromium": engine(Engine::Chromium, config.engines.chromium.browser_major),
            "firefox": engine(Engine::Firefox, config.engines.firefox.browser_major),
        },
        "fingerprint_fixtures": selftest::fixtures().len(),
        // Whose echo a scheduled self-test would ask; none without the
        // operator's own.
        "scheduled_selftest": config.selftest.http.echo_url.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use kage_core::config::Config;

    use super::{CRATE_NAME, check_config, describe};

    fn config(extra: &[(&str, &str)]) -> Config {
        let mut pairs = vec![
            ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
            ("KAGE__SIDECAR__TOKEN", "token"),
            (
                "KAGE__SECRETS__MASTER_KEY",
                "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
            ),
        ];
        pairs.extend_from_slice(extra);
        Config::load(
            None,
            pairs
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned())),
        )
        .unwrap()
    }

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-http");
    }

    #[test]
    fn the_default_configuration_passes_and_describes_what_is_served() {
        let config = config(&[]);
        assert_eq!(check_config(&config), Ok(()));

        let described = describe(&config);
        assert_eq!(
            described["exact_profiles"]["chromium"],
            serde_json::json!([142, 143, 144, 145, 146, 147, 148, 149])
        );
        assert_eq!(described["exact_profiles"]["firefox"][7], 151);
        assert_eq!(described["verified_majors"], serde_json::json!([]));
        assert!(described["engines"]["chromium"]["browser_major"].is_null());
        assert!(described["engines"]["chromium"]["profile"].is_null());
        assert_eq!(described["fingerprint_fixtures"], 48);
        assert_eq!(described["scheduled_selftest"], false);
    }

    #[test]
    fn a_configured_engine_major_must_have_a_profile() {
        let served = config(&[
            ("KAGE__ENGINES__CHROMIUM__BROWSER_MAJOR", "149"),
            ("KAGE__ENGINES__FIREFOX__BROWSER_MAJOR", "151"),
            (
                "KAGE__SELFTEST__HTTP__ECHO_URL",
                "https://echo.example.org/",
            ),
        ]);
        assert_eq!(check_config(&served), Ok(()));
        let described = describe(&served);
        assert_eq!(described["engines"]["chromium"]["profile"], "chrome_149");
        assert_eq!(described["engines"]["firefox"]["profile"], "firefox_151");
        assert_eq!(described["scheduled_selftest"], true);

        // The engines of this release: Chrome 155 (M4) and Camoufox 152
        // (M5). A capture of each proved that no profile fits, so both are
        // accepted, and their personas have no http path.
        let released = config(&[
            ("KAGE__ENGINES__CHROMIUM__BROWSER_MAJOR", "155"),
            ("KAGE__ENGINES__FIREFOX__BROWSER_MAJOR", "152"),
        ]);
        assert_eq!(check_config(&released), Ok(()));
        let described = describe(&released);
        assert_eq!(
            described["browser_only_majors"],
            serde_json::json!(["chromium-155", "firefox-152"])
        );
        assert_eq!(described["browser_captures"], 2);
        for engine in ["chromium", "firefox"] {
            assert!(described["engines"][engine]["profile"].is_null());
        }
        assert_eq!(described["engines"]["firefox"]["browser_major"], 152);

        // The next majors: the check that makes a milestone record a
        // browser capture before it sets the key.
        for (key, major, name) in [
            (
                "KAGE__ENGINES__CHROMIUM__BROWSER_MAJOR",
                "156",
                "engines.chromium.browser_major",
            ),
            (
                "KAGE__ENGINES__FIREFOX__BROWSER_MAJOR",
                "153",
                "engines.firefox.browser_major",
            ),
        ] {
            let error = check_config(&config(&[(key, major)])).unwrap_err();
            assert!(error.starts_with(name), "{error}");
            assert!(error.contains(major), "{error}");
        }
    }

    #[test]
    fn the_echos_certificate_authority_must_be_readable_pem() {
        let missing = config(&[(
            "KAGE__SELFTEST__HTTP__ECHO_CA_FILE",
            "/nonexistent/kage-echo-ca.pem",
        )]);
        let error = check_config(&missing).unwrap_err();
        assert!(
            error.starts_with("selftest.http.echo_ca_file: cannot read"),
            "{error}"
        );

        let file = std::env::temp_dir().join(format!("kage-echo-ca-{}.pem", std::process::id()));
        std::fs::write(
            &file,
            "-----BEGIN CERTIFICATE-----\nnot base64\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let broken = config(&[("KAGE__SELFTEST__HTTP__ECHO_CA_FILE", file.to_str().unwrap())]);
        let error = check_config(&broken).unwrap_err();
        std::fs::remove_file(&file).unwrap();
        assert!(error.contains("holds no valid PEM certificate"), "{error}");
    }
}
