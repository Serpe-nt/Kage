//! The fingerprint self-test of the http path (spec §7.4): ask an echo what
//! a client looks like on the wire and compare the answer with the values
//! recorded for the client's profile.
//!
//! What is compared:
//!
//! - the **JA4** of the TLS `ClientHello`;
//! - the **HTTP/2 fingerprint** (the "Akamai" string: settings, window
//!   update, priority frames, pseudo-header order);
//! - the **header order** of a typed navigation;
//! - that the request went out over HTTP/2 and carried the `User-Agent` the
//!   client was built with.
//!
//! The expected values live in `tests/fixtures/fingerprints/`, one file per
//! mapped profile and operating system, and are compiled into the crate.
//!
//! # Which echo
//!
//! A fingerprint is a function of the wreq and wreq-util pins, the profile
//! and Kage's own header code. It cannot differ from one proxy or persona
//! to the next: a tunnel carries the handshake end to end. So it is checked
//! where it can change:
//!
//! - on every change, by an integration test, against the in-repo echo
//!   ([`crate::echo`]) on this machine, with no network;
//! - on demand and in the gated live suite, through a real proxy against
//!   an echo run by someone else, which proves that Kage's own JA4 and
//!   HTTP/2 arithmetic agree with an independent implementation and that a
//!   real proxy path changes nothing;
//! - in a deployment, against the operator's own echo, when one is
//!   configured.
//!
//! The service never asks a third-party echo on a timer: it would show that
//! party the exit address of every proxy, on one cadence, with one JA4.
//!
//! Two answer formats are understood: the in-repo echo's
//! ([`crate::echo::Observation`]) and that of `tls.peet.ws/api/all`.

use std::time::Duration;

use kage_core::domain::Timing;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::client::HttpClient;
use crate::echo::{ECHO_NAME, Observation};
use crate::error::HttpError;
use crate::fetch::FetchSpec;
use crate::profile::ProfileChoice;

include!(concat!(env!("OUT_DIR"), "/fingerprints.rs"));

/// What a profile on an operating system must look like on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    /// The wreq-util profile, for example `chrome_149`.
    pub profile: String,
    /// The operating system it claims.
    pub os: String,
    /// The JA4 of its `ClientHello`.
    pub ja4: String,
    /// Its HTTP/2 fingerprint.
    pub akamai: String,
    /// The header names of a typed navigation over HTTP/2, in wire order.
    pub headers: Vec<String>,
}

impl Expected {
    /// What an echo saw, as the expectation for a profile: how a fixture is
    /// recorded.
    pub fn record(choice: &ProfileChoice, observed: &Observed) -> Self {
        Self {
            profile: choice.name().to_owned(),
            os: choice.os().as_str().to_owned(),
            ja4: observed.ja4.clone(),
            akamai: observed.akamai.clone().unwrap_or_default(),
            headers: observed.header_names(),
        }
    }

    /// The fixture file content: pretty JSON with a final newline.
    pub fn to_json(&self) -> String {
        // A struct of strings always serializes.
        let mut json = serde_json::to_string_pretty(self).unwrap_or_default();
        json.push('\n');
        json
    }
}

/// One request of a browser capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedRequest {
    /// The request method.
    pub method: String,
    /// `h2` or `http/1.1`.
    pub http_version: String,
    /// The header lines in wire order, as the browser spelled them.
    pub headers: Vec<(String, String)>,
}

/// What a real browser showed the fingerprint echo: the capture a
/// milestone records for its engine (follow-up F5), kept in
/// `tests/fixtures/fingerprints/browser/<engine>-<major>.json`.
///
/// `ja4`, `akamai` and `headers` are what the http path must equal before
/// a profile is derived for the major ([`crate::profile::VERIFIED_MAJORS`]),
/// and what the sidecar's self-test expects the browser to show an echo.
/// `requests` keeps every kind of request that was captured, header by
/// header: the typed navigation, `fetch()`, a JSON POST, `XMLHttpRequest`,
/// and a navigation over HTTP/1.1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserCapture {
    /// The engine: `chromium` or `firefox`.
    pub engine: String,
    /// The browser's full version.
    pub browser_version: String,
    /// The operating system the capturing persona claimed: `linux` for
    /// Chrome, which claims the system it runs on, and `windows` for
    /// Camoufox, which runs on Linux and does not claim it by default. A
    /// comparison with a derived profile uses the profile on this system.
    pub os: String,
    /// The day of the capture, `YYYY-MM-DD`.
    pub recorded: String,
    /// The JA4 of the browser's `ClientHello`.
    pub ja4: String,
    /// The lists the JA4 is computed from, written out.
    pub ja4_r: String,
    /// The HTTP/2 fingerprint.
    pub akamai: String,
    /// The header names of a typed navigation over HTTP/2, in wire order.
    pub headers: Vec<String>,
    /// Every captured kind of request by name.
    pub requests: std::collections::BTreeMap<String, CapturedRequest>,
}

/// The capture recorded for an engine's browser major; `None` when this
/// build has none.
pub fn browser_capture(engine: &str, major: u32) -> Option<BrowserCapture> {
    let stem = format!("{engine}-{major}");
    BROWSER_CAPTURES
        .iter()
        .find(|(name, _)| *name == stem)
        .and_then(|(_, json)| serde_json::from_str(json).ok())
}

/// Every embedded browser capture by file stem, parsed, or the reason it
/// does not parse.
pub fn browser_captures() -> Vec<(&'static str, Result<BrowserCapture, String>)> {
    BROWSER_CAPTURES
        .iter()
        .map(|(name, json)| {
            (
                *name,
                serde_json::from_str(json).map_err(|error| error.to_string()),
            )
        })
        .collect()
}

/// The recorded expectation for a profile on an operating system; `None`
/// when there is no fixture for it.
pub fn expected(choice: &ProfileChoice) -> Option<Expected> {
    let stem = choice.fixture_stem();
    FIXTURES
        .iter()
        .find(|(name, _)| *name == stem)
        .and_then(|(_, json)| serde_json::from_str(json).ok())
}

/// Every embedded fixture by file stem, parsed, or the reason it does not
/// parse.
pub fn fixtures() -> Vec<(&'static str, Result<Expected, String>)> {
    FIXTURES
        .iter()
        .map(|(name, json)| {
            (
                *name,
                serde_json::from_str(json).map_err(|error| error.to_string()),
            )
        })
        .collect()
}

/// Which service answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoKind {
    /// The in-repo echo.
    Kage,
    /// A service that answers in the format of `tls.peet.ws/api/all`.
    Peet,
}

/// What an echo saw of one request, in the terms the self-test compares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observed {
    /// Which service answered.
    pub source: EchoKind,
    /// `h2` or `http/1.1`.
    pub http_version: String,
    /// The JA4 it computed.
    pub ja4: String,
    /// The HTTP/2 fingerprint it computed; `None` over HTTP/1.1.
    pub akamai: Option<String>,
    /// The headers it received, in order, without pseudo-headers.
    pub headers: Vec<(String, String)>,
    /// The address it saw the request come from: the proxy's exit.
    pub peer: Option<String>,
}

impl Observed {
    /// Reads an echo's answer in either format.
    ///
    /// # Errors
    ///
    /// Fails when the body is neither.
    pub fn parse(body: &[u8]) -> Result<Self, SelftestError> {
        let value: serde_json::Value = serde_json::from_slice(body)
            .map_err(|error| SelftestError::Unrecognised(format!("it is not JSON: {error}")))?;
        if value.get("echo").and_then(serde_json::Value::as_str) == Some(ECHO_NAME) {
            let observation: Observation = serde_json::from_value(value).map_err(|error| {
                SelftestError::Unrecognised(format!("the echo's answer does not parse: {error}"))
            })?;
            return Ok(Self {
                source: EchoKind::Kage,
                http_version: observation.http_version.clone(),
                ja4: observation.tls.ja4.clone(),
                akamai: observation.http2.as_ref().map(|http2| http2.akamai.clone()),
                headers: observation.headers().to_vec(),
                peer: Some(observation.peer),
            });
        }
        let text = |path: &[&str]| -> Option<String> {
            path.iter()
                .try_fold(&value, |value, key| value.get(key))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        let Some(ja4) = text(&["tls", "ja4"]) else {
            return Err(SelftestError::Unrecognised(
                "it holds neither this echo's marker nor tls.ja4".to_owned(),
            ));
        };
        // The HEADERS frame as a list of `name: value` lines.
        let headers = value
            .get("http2")
            .and_then(|http2| http2.get("sent_frames"))
            .and_then(serde_json::Value::as_array)
            .and_then(|frames| {
                frames.iter().find(|frame| {
                    frame.get("frame_type").and_then(serde_json::Value::as_str) == Some("HEADERS")
                })
            })
            .and_then(|frame| frame.get("headers"))
            .and_then(serde_json::Value::as_array)
            .map(|lines| {
                lines
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter(|line| !line.starts_with(':'))
                    .filter_map(|line| line.split_once(':'))
                    .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            source: EchoKind::Peet,
            http_version: text(&["http_version"]).unwrap_or_default(),
            ja4,
            akamai: text(&["http2", "akamai_fingerprint"]),
            headers,
            // `address:port`, of either family.
            peer: text(&["ip"]).map(|peer| {
                if peer.parse::<std::net::IpAddr>().is_ok() {
                    return peer;
                }
                match peer.rsplit_once(':') {
                    Some((address, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => {
                        address.trim_matches(['[', ']']).to_owned()
                    }
                    _ => peer,
                }
            }),
        })
    }

    /// The header names in the order received, in lowercase.
    pub fn header_names(&self) -> Vec<String> {
        self.headers
            .iter()
            .map(|(name, _)| name.to_ascii_lowercase())
            .collect()
    }

    /// The first value of a header.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One comparison of the self-test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// `http_version`, `ja4`, `akamai`, `header_order` or `user_agent`.
    pub check: &'static str,
    /// What the fixture, or the client's identity, says.
    pub expected: String,
    /// What the echo saw.
    pub observed: String,
    /// Whether the two are equal.
    pub passed: bool,
}

fn finding(check: &'static str, expected: String, observed: String) -> Finding {
    Finding {
        check,
        passed: expected == observed,
        expected,
        observed,
    }
}

/// Compares what an echo saw with what is expected of the profile.
/// `user_agent` is the one the client was built with.
pub fn compare(expected: &Expected, user_agent: &str, observed: &Observed) -> Vec<Finding> {
    vec![
        finding(
            "http_version",
            "h2".to_owned(),
            observed.http_version.clone(),
        ),
        finding("ja4", expected.ja4.clone(), observed.ja4.clone()),
        finding(
            "akamai",
            expected.akamai.clone(),
            observed.akamai.clone().unwrap_or_default(),
        ),
        finding(
            "header_order",
            expected.headers.join(", "),
            observed.header_names().join(", "),
        ),
        finding(
            "user_agent",
            user_agent.to_owned(),
            observed.header("user-agent").unwrap_or_default().to_owned(),
        ),
    ]
}

/// Why a self-test could not be judged.
#[derive(Debug, thiserror::Error)]
pub enum SelftestError {
    /// There is no fixture for the profile.
    #[error(
        "there are no recorded fingerprint values for {profile}; record them with \
         KAGE_RECORD_FINGERPRINTS=1 (docs/runbooks/fingerprint-echo.md)"
    )]
    NoExpectation {
        /// The profile and its operating system.
        profile: String,
    },
    /// The echo could not be fetched.
    #[error("the echo could not be reached: {0}")]
    Fetch(#[from] HttpError),
    /// The echo answered with an error status.
    #[error("the echo answered with status {0}")]
    Status(u16),
    /// The echo's answer is in no known format.
    #[error("the echo's answer is not understood: {0}")]
    Unrecognised(String),
}

/// The outcome of one self-test.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SelftestReport {
    /// The profile under test.
    pub profile: String,
    /// The operating system it claims.
    pub os: String,
    /// The echo that was asked, without its query.
    pub echo: String,
    /// Whether every comparison held.
    pub passed: bool,
    /// The comparisons.
    pub findings: Vec<Finding>,
    /// What the echo saw.
    pub observed: Observed,
    /// Where the time went.
    pub timing: Timing,
}

/// Runs the self-test of a client against an echo: one typed navigation,
/// compared with the fixture of the client's profile.
///
/// Use a client from [`crate::ClientFactory::ephemeral`], so that the
/// request opens a fresh connection and carries no cookies: a resumed TLS
/// session or a `Cookie` header changes what the echo sees.
///
/// # Errors
///
/// Fails when the profile has no fixture, or the echo cannot be reached or
/// understood. A fingerprint that differs is not an error: the report says
/// so.
pub async fn run(
    client: &HttpClient,
    echo: &Url,
    timeout: Duration,
) -> Result<SelftestReport, SelftestError> {
    let profile = client.profile();
    let expected = expected(profile).ok_or_else(|| SelftestError::NoExpectation {
        profile: profile.to_string(),
    })?;
    let mut spec = FetchSpec::operator_get(echo.clone());
    spec.timeout = Some(timeout);
    let fetched = client.fetch(&spec).await?;
    if fetched.status != 200 {
        return Err(SelftestError::Status(fetched.status));
    }
    let observed = Observed::parse(&fetched.body)?;
    let user_agent =
        String::from_utf8_lossy(client.identity().user_agent().as_bytes()).into_owned();
    let findings = compare(&expected, &user_agent, &observed);
    let mut asked = echo.clone();
    asked.set_query(None);
    asked.set_fragment(None);
    Ok(SelftestReport {
        profile: profile.name().to_owned(),
        os: profile.os().as_str().to_owned(),
        echo: asked.to_string(),
        passed: findings.iter().all(|finding| finding.passed),
        findings,
        observed,
        timing: fetched.timing,
    })
}

#[cfg(test)]
mod tests {
    use kage_core::domain::ClaimedOs;

    use super::{EchoKind, Expected, Observed, SelftestError, compare, expected, fixtures};
    use crate::profile::{ProfileChoice, mapping_table};

    const KAGE_ANSWER: &str = r#"{
      "echo": "kage-echo", "version": 1, "peer": "203.0.113.20",
      "method": "GET", "path": "/", "http_version": "h2",
      "tls": {
        "ja4": "t13d1516h2_8daaf6152771_d8a2da3f94cd", "ja4_r": "t13d1516h2_x_y",
        "version": "13", "sni": "echo.kage.test", "alpn": ["h2", "http/1.1"],
        "ciphers": ["1301"], "extensions": ["0000"], "groups": ["001d"],
        "signature_algorithms": ["0403"]
      },
      "http2": {
        "akamai": "1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p",
        "settings": [[1, 65536], [2, 0], [4, 6291456], [6, 262144]],
        "window_update": 15663105, "priority_frames": [], "headers_priority": "1:1:0:256",
        "pseudo_headers": ["m", "a", "s", "p"],
        "headers": [["sec-ch-ua", "x"], ["user-agent", "Test/1"], ["accept", "*/*"]]
      },
      "http1": null
    }"#;

    const PEET_ANSWER: &str = r#"{
      "donate": "...", "ip": "203.0.113.20:51234", "http_version": "h2", "method": "GET",
      "user_agent": "Test/1",
      "tls": {"ciphers": [], "extensions": [], "ja3": "x", "ja3_hash": "y",
              "ja4": "t13d1516h2_8daaf6152771_d8a2da3f94cd", "ja4_r": "z"},
      "http2": {
        "akamai_fingerprint": "1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p",
        "akamai_fingerprint_hash": "52d84b11737d980aef856699f885ca86",
        "sent_frames": [
          {"frame_type": "SETTINGS", "length": 24, "settings": ["HEADER_TABLE_SIZE = 65536"]},
          {"frame_type": "WINDOW_UPDATE", "length": 4, "increment": 15663105},
          {"frame_type": "HEADERS", "stream_id": 1, "length": 300,
           "headers": [":method: GET", ":authority: tls.peet.ws", ":scheme: https",
                       ":path: /api/all", "sec-ch-ua: x", "User-Agent: Test/1",
                       "accept: text/html;q=0.9, */*: all"],
           "flags": ["EndStream (0x1)", "EndHeaders (0x4)", "Priority (0x20)"]}
        ]
      },
      "tcpip": {}
    }"#;

    #[test]
    fn both_echo_formats_are_read_into_the_same_terms() {
        let kage = Observed::parse(KAGE_ANSWER.as_bytes()).unwrap();
        let peet = Observed::parse(PEET_ANSWER.as_bytes()).unwrap();

        assert_eq!(kage.source, EchoKind::Kage);
        assert_eq!(peet.source, EchoKind::Peet);
        for observed in [&kage, &peet] {
            assert_eq!(observed.http_version, "h2");
            assert_eq!(observed.ja4, "t13d1516h2_8daaf6152771_d8a2da3f94cd");
            assert_eq!(
                observed.akamai.as_deref(),
                Some("1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p")
            );
            assert_eq!(
                observed.header_names(),
                ["sec-ch-ua", "user-agent", "accept"]
            );
            assert_eq!(observed.header("User-Agent"), Some("Test/1"));
            assert_eq!(observed.peer.as_deref(), Some("203.0.113.20"));
        }
        // A value may hold a colon.
        assert_eq!(peet.header("accept"), Some("text/html;q=0.9, */*: all"));
    }

    #[test]
    fn an_http1_answer_and_odd_peers_are_read_too() {
        let http1 = r#"{"ip": "[2001:db8::7]:443", "http_version": "HTTP/1.1",
                        "tls": {"ja4": "t13d1516h1_a_b"}, "http1": {"headers": []}}"#;
        let observed = Observed::parse(http1.as_bytes()).unwrap();
        assert_eq!(observed.http_version, "HTTP/1.1");
        assert_eq!(observed.akamai, None);
        assert_eq!(observed.headers, Vec::<(String, String)>::new());
        assert_eq!(observed.peer.as_deref(), Some("2001:db8::7"));

        let bare = Observed::parse(br#"{"ip": "203.0.113.20", "tls": {"ja4": "x"}}"#).unwrap();
        assert_eq!(bare.peer.as_deref(), Some("203.0.113.20"));
        let bare6 = Observed::parse(br#"{"ip": "2001:db8::7", "tls": {"ja4": "x"}}"#).unwrap();
        assert_eq!(bare6.peer.as_deref(), Some("2001:db8::7"));
        assert_eq!(bare.http_version, "");
    }

    #[test]
    fn what_is_not_an_echo_answer_is_refused() {
        for body in [
            &b""[..],
            b"<html>blocked</html>",
            b"[]",
            br#"{"ip": "203.0.113.20"}"#,
            br#"{"tls": {"ja3": "only"}}"#,
            br#"{"echo": "kage-echo", "version": 1}"#,
        ] {
            let error = Observed::parse(body).unwrap_err();
            assert!(
                matches!(error, SelftestError::Unrecognised(_)),
                "{:?}: {error}",
                String::from_utf8_lossy(body)
            );
        }
    }

    fn expectation() -> Expected {
        Expected {
            profile: "chrome_149".to_owned(),
            os: "windows".to_owned(),
            ja4: "t13d1516h2_8daaf6152771_d8a2da3f94cd".to_owned(),
            akamai: "1:65536;2:0;4:6291456;6:262144|15663105|0|m,a,s,p".to_owned(),
            headers: vec![
                "sec-ch-ua".to_owned(),
                "user-agent".to_owned(),
                "accept".to_owned(),
            ],
        }
    }

    #[test]
    fn equal_values_pass_and_every_difference_is_named() {
        let observed = Observed::parse(KAGE_ANSWER.as_bytes()).unwrap();

        let findings = compare(&expectation(), "Test/1", &observed);
        assert_eq!(
            findings
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
        assert!(
            findings.iter().all(|finding| finding.passed),
            "{findings:?}"
        );

        let failing = |change: &dyn Fn(&mut Expected, &mut Observed)| -> Vec<&'static str> {
            let mut expected = expectation();
            let mut observed = observed.clone();
            change(&mut expected, &mut observed);
            compare(&expected, "Test/1", &observed)
                .into_iter()
                .filter(|finding| !finding.passed)
                .map(|finding| finding.check)
                .collect()
        };
        assert_eq!(failing(&|expected, _| expected.ja4.push('x')), ["ja4"]);
        assert_eq!(
            failing(&|_, observed| observed.akamai = Some("1:65536|0|0|m,p,a,s".to_owned())),
            ["akamai"]
        );
        // Order matters, not just the set.
        assert_eq!(
            failing(&|_, observed| observed.headers.swap(0, 1)),
            ["header_order"]
        );
        assert_eq!(
            failing(&|_, observed| observed
                .headers
                .push(("cookie".to_owned(), "a=1".to_owned()))),
            ["header_order"]
        );
        assert_eq!(
            failing(&|_, observed| observed.headers[1].1 = "Other/2".to_owned()),
            ["user_agent"]
        );
        assert_eq!(
            failing(&|_, observed| {
                observed.http_version = "http/1.1".to_owned();
                observed.akamai = None;
            }),
            ["http_version", "akamai"]
        );
    }

    #[test]
    fn a_fixture_is_recorded_from_what_an_echo_saw() {
        let choice = ProfileChoice::named("chrome_149", ClaimedOs::Windows).unwrap();
        let observed = Observed::parse(KAGE_ANSWER.as_bytes()).unwrap();

        let recorded = Expected::record(&choice, &observed);

        assert_eq!(recorded, expectation());
        let json = recorded.to_json();
        assert!(json.ends_with("}\n"));
        assert_eq!(serde_json::from_str::<Expected>(&json).unwrap(), recorded);
        // A fixture with a field nobody knows is a mistake, not ignored.
        assert!(serde_json::from_str::<Expected>(&json.replace("\"os\"", "\"oss\"")).is_err());
    }

    /// Every mapped profile has a fixture, every fixture parses and belongs
    /// to a mapped profile, and each says what its file name says.
    #[test]
    fn every_mapped_profile_has_a_fixture_and_every_fixture_a_profile() {
        let table = mapping_table();
        for row in &table {
            let choice = ProfileChoice::named(&row.profile, row.os).unwrap();
            let fixture = expected(&choice).unwrap_or_else(|| {
                panic!(
                    "no fixture tests/fixtures/fingerprints/{}.json: record it with \
                     KAGE_RECORD_FINGERPRINTS=1 cargo test -p kage-http --test fingerprints",
                    choice.fixture_stem()
                )
            });
            assert_eq!(fixture.profile, row.profile);
            assert_eq!(fixture.os, row.os.as_str());
            assert!(fixture.ja4.starts_with("t13d"), "{fixture:?}");
            assert!(fixture.akamai.contains('|'), "{fixture:?}");
            assert!(
                fixture.headers.contains(&"user-agent".to_owned()),
                "{fixture:?}"
            );
        }
        let all = fixtures();
        assert_eq!(all.len(), table.len(), "a fixture without a mapped profile");
        for (stem, fixture) in all {
            let fixture = fixture.unwrap_or_else(|error| panic!("{stem}.json: {error}"));
            assert_eq!(format!("{}-{}", fixture.profile, fixture.os), stem);
        }
    }
}
