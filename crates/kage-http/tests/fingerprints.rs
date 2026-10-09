//! The fingerprint regression gate (spec §7.4, §21): every mapped emulation
//! profile, sent through a local proxy to the in-repo echo, must look on the
//! wire exactly as recorded in `tests/fixtures/fingerprints/`.
//!
//! A change of the wreq or wreq-util pin, or of Kage's header code, that
//! moves a JA4, an HTTP/2 fingerprint or a header order fails here, on the
//! change itself, with no network involved.
//!
//! To record the fixtures after a deliberate change:
//!
//! ```text
//! KAGE_RECORD_FINGERPRINTS=1 cargo test -p kage-http --test fingerprints
//! ```
//!
//! and review the diff of the fixture files like any other change.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TestEcho, TestProxy, factory, observation, spec};
use kage_core::domain::{ClaimedOs, Engine};
use kage_http::headers::IdentityOverride;
use kage_http::profile::{self, Family, VERIFIED_MAJORS, mapping_table};
use kage_http::selftest::{self, Expected, Observed};
use kage_http::{ClientFactory, ClientSpec, FetchSpec, ProfileChoice};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fingerprints")
}

fn recording() -> bool {
    std::env::var("KAGE_RECORD_FINGERPRINTS").is_ok_and(|value| value == "1")
}

fn read_fixture(stem: &str) -> Option<Expected> {
    let text = std::fs::read_to_string(fixture_dir().join(format!("{stem}.json"))).ok()?;
    Some(serde_json::from_str(&text).unwrap_or_else(|error| panic!("{stem}.json: {error}")))
}

struct Bench {
    proxy: TestProxy,
    _echo: TestEcho,
    factory: ClientFactory,
}

impl Bench {
    async fn start() -> Self {
        let proxy = TestProxy::start().await;
        let echo = TestEcho::start().await;
        echo.route_through(&proxy);
        let factory = factory(&echo);
        Self {
            proxy,
            _echo: echo,
            factory,
        }
    }

    /// One typed navigation of a fresh client to the echo.
    async fn observe(&self, spec: &ClientSpec) -> (Observed, String) {
        let client = self
            .factory
            .ephemeral(spec, &self.proxy.endpoint())
            .unwrap();
        let fetched = client
            .fetch(&FetchSpec::operator_get(TestEcho::url("/")))
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", spec.profile));
        assert_eq!(fetched.status, 200);
        let user_agent = client.identity().user_agent().to_str().unwrap().to_owned();
        (Observed::parse(&fetched.body).unwrap(), user_agent)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn every_mapped_profile_looks_on_the_wire_as_recorded() {
    let bench = Bench::start().await;
    let table = mapping_table();
    assert_eq!(table.len(), 48);
    let mut problems = Vec::new();

    for row in &table {
        let spec = spec(&row.profile, row.os);
        let (observed, user_agent) = bench.observe(&spec).await;
        let stem = spec.profile.fixture_stem();
        if recording() {
            std::fs::create_dir_all(fixture_dir()).unwrap();
            std::fs::write(
                fixture_dir().join(format!("{stem}.json")),
                Expected::record(&spec.profile, &observed).to_json(),
            )
            .unwrap();
            continue;
        }
        let Some(expected) = read_fixture(&stem) else {
            problems.push(format!(
                "{stem}: no fixture; record it (see this file's header)"
            ));
            continue;
        };
        for finding in selftest::compare(&expected, &user_agent, &observed) {
            if !finding.passed {
                problems.push(format!(
                    "{stem}: {} differs\n    recorded: {}\n    on the wire: {}",
                    finding.check, finding.expected, finding.observed
                ));
            }
        }
    }

    assert!(
        problems.is_empty(),
        "the wire fingerprint of {} profile(s) is not the recorded one:\n{}",
        problems.len(),
        problems.join("\n")
    );
    // Every request went through the proxy, as a tunnel to the echo's name.
    let tunnels = bench.proxy.tunnels();
    assert_eq!(tunnels.len(), table.len());
    assert!(
        tunnels
            .iter()
            .all(|seen| seen.target == "echo.kage.test:443")
    );
}

/// The fixtures themselves: one transport fingerprint per browser family
/// (wreq-util's current generation shares it), the well-known shapes, and no
/// fixture without a mapped profile.
#[test]
fn the_fixtures_say_what_is_known_about_each_browser() {
    if recording() {
        return;
    }
    let table = mapping_table();
    let mut stems = BTreeSet::new();
    let mut by_family: [BTreeSet<(String, String, Vec<String>)>; 2] = Default::default();
    for row in &table {
        let choice = ProfileChoice::named(&row.profile, row.os).unwrap();
        let fixture = read_fixture(&choice.fixture_stem())
            .unwrap_or_else(|| panic!("no fixture for {choice}"));
        assert_eq!(fixture.profile, row.profile);
        assert_eq!(fixture.os, row.os.as_str());
        stems.insert(choice.fixture_stem());
        let index = usize::from(choice.family() == Family::Firefox);
        by_family[index].insert((fixture.ja4, fixture.akamai, fixture.headers));
    }
    let on_disk: BTreeSet<String> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| path.file_stem()?.to_str().map(str::to_owned))
        .collect();
    assert_eq!(on_disk, stems, "fixture files and mapped profiles differ");

    let [chrome, firefox] = by_family;
    assert_eq!(
        chrome.len(),
        1,
        "Chrome profiles differ among themselves: {chrome:#?}"
    );
    assert_eq!(
        firefox.len(),
        1,
        "Firefox profiles differ among themselves: {firefox:#?}"
    );
    let (chrome_ja4, chrome_akamai, chrome_headers) = chrome.into_iter().next().unwrap();
    let (firefox_ja4, firefox_akamai, firefox_headers) = firefox.into_iter().next().unwrap();

    // Chrome: TLS 1.3, a server name, 15 ciphers and 16 extensions without
    // GREASE, HTTP/2 first; its HTTP/2 settings and pseudo-header order.
    assert!(chrome_ja4.starts_with("t13d1516h2_"), "{chrome_ja4}");
    assert_eq!(
        chrome_akamai,
        "1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p"
    );
    assert_eq!(
        chrome_headers,
        [
            "sec-ch-ua",
            "sec-ch-ua-mobile",
            "sec-ch-ua-platform",
            "upgrade-insecure-requests",
            "user-agent",
            "accept",
            "sec-fetch-site",
            "sec-fetch-mode",
            "sec-fetch-user",
            "sec-fetch-dest",
            "accept-encoding",
            "accept-language",
            "priority",
        ]
    );
    // Firefox: its own TLS shape, settings and pseudo-header order.
    assert!(firefox_ja4.starts_with("t13d17"), "{firefox_ja4}");
    assert!(firefox_ja4.contains("h2_"), "{firefox_ja4}");
    assert!(firefox_akamai.ends_with("|m,p,a,s"), "{firefox_akamai}");
    assert!(
        firefox_akamai.starts_with("1:65536;2:0;4:131072;5:16384|"),
        "{firefox_akamai}"
    );
    assert_eq!(
        firefox_headers,
        [
            "user-agent",
            "accept",
            "accept-language",
            "accept-encoding",
            "upgrade-insecure-requests",
            "sec-fetch-dest",
            "sec-fetch-mode",
            "sec-fetch-site",
            "sec-fetch-user",
            "priority",
            "te",
        ]
    );
    assert_ne!(chrome_ja4, firefox_ja4);
}

#[tokio::test(flavor = "multi_thread")]
async fn chrome_shuffles_its_extensions_and_keeps_its_fingerprint() {
    let bench = Bench::start().await;
    let chrome = spec("chrome_149", ClaimedOs::Windows);
    let firefox = spec("firefox_151", ClaimedOs::Windows);
    let hello = |profile: ClientSpec| {
        let bench = &bench;
        async move {
            let client = bench
                .factory
                .ephemeral(&profile, &bench.proxy.endpoint())
                .unwrap();
            let fetched = client
                .fetch(&FetchSpec::operator_get(TestEcho::url("/")))
                .await
                .unwrap();
            observation(&fetched.body).tls
        }
    };

    let mut chrome_orders = BTreeSet::new();
    let mut chrome_ja4 = BTreeSet::new();
    let mut firefox_orders = BTreeSet::new();
    for _ in 0..5 {
        let tls = hello(chrome.clone()).await;
        assert_eq!(tls.sni.as_deref(), Some("echo.kage.test"));
        assert_eq!(tls.alpn, ["h2", "http/1.1"]);
        chrome_orders.insert(tls.extensions);
        chrome_ja4.insert(tls.ja4);
        firefox_orders.insert(hello(firefox.clone()).await.extensions);
    }

    // Chrome sends its extensions in a new order on every connection; JA4
    // sorts them, so the fingerprint holds. Firefox keeps one order.
    assert!(
        chrome_orders.len() > 1,
        "Chrome's extension order never changed"
    );
    assert_eq!(chrome_ja4.len(), 1, "{chrome_ja4:?}");
    assert_eq!(firefox_orders.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_self_test_passes_through_a_proxy_and_says_what_it_compared() {
    if recording() {
        return;
    }
    let bench = Bench::start().await;
    bench.proxy.set_delay(Duration::from_millis(40));

    for (profile, os) in [
        ("chrome_149", ClaimedOs::Windows),
        ("firefox_151", ClaimedOs::Macos),
    ] {
        let client = bench
            .factory
            .ephemeral(&spec(profile, os), &bench.proxy.endpoint())
            .unwrap();

        let report = selftest::run(
            &client,
            &TestEcho::url("/?probe=1"),
            Duration::from_secs(20),
        )
        .await
        .unwrap();

        assert!(report.passed, "{profile}: {:#?}", report.findings);
        assert_eq!(report.profile, profile);
        assert_eq!(report.os, os.as_str());
        // The query is not part of what is reported.
        assert_eq!(report.echo, "https://echo.kage.test/");
        assert_eq!(
            report
                .findings
                .iter()
                .map(|finding| finding.check)
                .collect::<Vec<_>>(),
            [
                "http_version",
                "ja4",
                "akamai",
                "header_order",
                "user_agent"
            ]
        );
        assert_eq!(report.observed.peer.as_deref(), Some("127.0.0.1"));
        // A fresh connection: tunnel and handshake were both timed.
        assert!(
            report.timing.connect_ms.is_some_and(|ms| ms >= 40),
            "{:?}",
            report.timing
        );
        assert!(report.timing.tls_ms.is_some());
        assert!(report.timing.ttfb_ms.is_some());
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["passed"], true);
        assert_eq!(json["observed"]["source"], "kage");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_self_test_that_cannot_be_judged_says_why() {
    let bench = Bench::start().await;
    let client = bench
        .factory
        .ephemeral(
            &spec("chrome_149", ClaimedOs::Windows),
            &bench.proxy.endpoint(),
        )
        .unwrap();

    // A URL the proxy cannot reach.
    let error = selftest::run(
        &client,
        &"https://nowhere.kage.test/".parse().unwrap(),
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(error, selftest::SelftestError::Fetch(_)),
        "{error}"
    );
    assert!(error.to_string().contains("could not be reached"));

    // A profile nobody recorded: an older generation is not mapped.
    let old = bench
        .factory
        .ephemeral(
            &spec("chrome_120", ClaimedOs::Windows),
            &bench.proxy.endpoint(),
        )
        .unwrap();
    let error = selftest::run(&old, &TestEcho::url("/"), Duration::from_secs(10))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, selftest::SelftestError::NoExpectation { profile } if profile == "chrome_120 on windows"),
        "{error}"
    );
}

/// The derivation of consult 4: the newest profile's transport, presenting
/// the engine's real major. It is not offered for any major yet (the
/// verified table is empty), but what it would send is pinned here.
#[tokio::test(flavor = "multi_thread")]
async fn a_derived_profile_keeps_the_transport_and_presents_the_engines_major() {
    if recording() {
        return;
    }
    let bench = Bench::start().await;

    for (family, major, base) in [
        (Family::Chrome, 156, "chrome_149-windows"),
        (Family::Firefox, 152, "firefox_151-windows"),
    ] {
        let resolved = profile::derive(family, major, ClaimedOs::Windows).unwrap();
        let spec = ClientSpec::new(resolved.choice.clone()).with_identity(IdentityOverride {
            user_agent: resolved.user_agent.clone(),
            client_hint_brands: resolved.client_hint_brands.clone(),
        });

        let (observed, user_agent) = bench.observe(&spec).await;

        let expected = read_fixture(base).unwrap();
        let findings = selftest::compare(&expected, &user_agent, &observed);
        assert!(
            findings.iter().all(|finding| finding.passed),
            "{findings:#?}"
        );
        assert!(user_agent.contains(&format!("/{major}.0")), "{user_agent}");
        assert_eq!(observed.header("user-agent"), Some(user_agent.as_str()));
        if family == Family::Chrome {
            assert_eq!(
                observed.header("sec-ch-ua"),
                Some(profile::chrome_brands(major).as_str())
            );
        }
    }
}

/// Consult 4: a major is offered a derived profile only when a capture from
/// the real browser proves that its TLS fingerprint, its HTTP/2 fingerprint
/// and its navigation headers equal what the derived profile sends.
#[tokio::test(flavor = "multi_thread")]
async fn every_verified_major_has_a_browser_capture_that_equals_the_derived_profile() {
    let captures = fixture_dir().join("browser");
    let bench = Bench::start().await;
    let mut listed = BTreeSet::new();

    for &(engine, major) in VERIFIED_MAJORS {
        let name = format!("{engine}-{major}");
        let text =
            std::fs::read_to_string(captures.join(format!("{name}.json"))).unwrap_or_else(|_| {
                panic!(
                    "{name} is in VERIFIED_MAJORS without tests/fixtures/fingerprints/browser/\
                     {name}.json: record the capture from the engine against the echo first"
                )
            });
        let capture: Expected = serde_json::from_str(&text).unwrap();
        listed.insert(name.clone());
        let family = Family::of_engine(engine);
        // Whatever the capture's operating system, the transport and the
        // header names are the family's.
        let resolved = profile::derive(family, major, ClaimedOs::Windows)
            .unwrap_or_else(|| panic!("{name} is not newer than the newest profile"));
        let spec = ClientSpec::new(resolved.choice.clone()).with_identity(IdentityOverride {
            user_agent: resolved.user_agent.clone(),
            client_hint_brands: resolved.client_hint_brands.clone(),
        });
        let (observed, _) = bench.observe(&spec).await;
        assert_eq!(observed.ja4, capture.ja4, "{name}: JA4");
        assert_eq!(
            observed.akamai.as_deref(),
            Some(capture.akamai.as_str()),
            "{name}: HTTP/2"
        );
        assert_eq!(
            observed.header_names(),
            capture.headers,
            "{name}: header order"
        );
    }

    // A capture nobody lists verifies nothing and would mislead.
    if let Ok(entries) = std::fs::read_dir(&captures) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let stem = path.file_stem().unwrap().to_str().unwrap().to_owned();
                assert!(
                    listed.contains(&stem),
                    "{stem}.json is not in VERIFIED_MAJORS"
                );
            }
        }
    }
    assert!(
        VERIFIED_MAJORS
            .iter()
            .all(|(engine, _)| matches!(engine, Engine::Chromium | Engine::Firefox))
    );
}
