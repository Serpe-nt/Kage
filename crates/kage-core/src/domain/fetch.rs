//! Requests, attempts, stored results and artifacts (spec §5, §16.5, §18).

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{ClaimedOs, CountryCode, Engine, Render, RenderMode, SessionId};
use crate::ids::{ApiKeyId, ArtifactId, AttemptId, JobId, PersonaId, RequestId, ResultId};
use crate::macros::string_enum;
use crate::outcome::{Block, Outcome};
use crate::policy::RefererStrategy;
use crate::seed::Seed;

/// One fetch request as it was accepted, whether synchronous or a job item.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestRecord {
    /// Identity; also the `X-Request-Id` of a synchronous fetch.
    pub id: RequestId,
    /// The key that made the request; `None` for Kage's own traffic.
    pub api_key_id: Option<ApiKeyId>,
    /// The job this request belongs to, if any.
    pub job_id: Option<JobId>,
    /// The session the request was pinned to, if any.
    pub session_id: Option<SessionId>,
    /// The target URL as the caller gave it.
    pub url: String,
    /// The HTTP method.
    pub method: String,
    /// The target host, the key for policy and pacing.
    pub domain: String,
    /// The render mode the caller asked for.
    pub render: RenderMode,
    /// The full request body as accepted (spec §16.4).
    pub spec: serde_json::Value,
    /// When the request was accepted.
    pub created_at: DateTime<Utc>,
    /// When the last attempt finished.
    pub finished_at: Option<DateTime<Utc>>,
    /// Final outcome, once finished.
    pub outcome: Option<Outcome>,
}

/// Time spent in each phase of an attempt, in milliseconds. Phases that did
/// not happen on the path taken are `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Timing {
    /// DNS resolution.
    #[serde(default)]
    pub dns_ms: Option<u64>,
    /// TCP connect, through the proxy.
    #[serde(default)]
    pub connect_ms: Option<u64>,
    /// TLS handshake.
    #[serde(default)]
    pub tls_ms: Option<u64>,
    /// Time to the first response byte.
    #[serde(default)]
    pub ttfb_ms: Option<u64>,
    /// `DOMContentLoaded` (browser path).
    #[serde(default)]
    pub dom_content_loaded_ms: Option<u64>,
    /// `load` event (browser path).
    #[serde(default)]
    pub load_ms: Option<u64>,
    /// The whole attempt.
    pub total_ms: u64,
}

/// One try at fetching a request with one persona.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    /// Identity.
    pub id: AttemptId,
    /// The request this attempt belongs to.
    pub request_id: RequestId,
    /// 1 for the first attempt, 2 for the first retry, and so on.
    pub n: u32,
    /// The persona used.
    pub persona_id: PersonaId,
    /// That persona's engine.
    pub engine: Engine,
    /// The path taken.
    pub render: Render,
    /// When the attempt started.
    pub started_at: DateTime<Utc>,
    /// When the attempt finished.
    pub finished_at: DateTime<Utc>,
    /// HTTP status of the final response, if one arrived.
    pub status: Option<u16>,
    /// URL after redirects, if a response arrived.
    pub final_url: Option<String>,
    /// Classifier verdict.
    pub outcome: Outcome,
    /// Block report when the outcome is a block.
    pub block: Option<Block>,
    /// Per-phase timing.
    pub timing: Timing,
    /// The seed of the attempt: with the persona's own seed it replays the
    /// behavior of a browser attempt and the pauses of an http attempt.
    pub seed: Option<Seed>,
    /// Why the attempt ended as it did when the site did not say, as a
    /// stable `snake_case` name: for example `proxy_failed`.
    pub reason: Option<String>,
    /// How long the persona would have gone on scrolling the page at most,
    /// in milliseconds: drawn for a browser attempt from its seed (spec
    /// §10.4), and kept with it so that the visit replays whatever the
    /// configuration says by then. `None` on the http path and for a flow.
    pub patience_ms: Option<u64>,
}

/// One response header. A list keeps order and repeated names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// Header name as received.
    pub name: String,
    /// Header value.
    pub value: String,
}

/// What stands in a result where a cookie's value was.
pub const REDACTED_COOKIE: &str = "[redacted]";

/// The same headers with the value of every `Set-Cookie` replaced by
/// [`REDACTED_COOKIE`]; names and attributes stay.
///
/// A cookie a site sets is the persona's standing there, bound to its
/// address and its fingerprint. A result that carried the values would
/// hand them to whoever reads it, and a log of results would be a list of
/// the pool's sessions (spec §19; `security.expose_set_cookie` turns the
/// redaction off).
pub fn redact_set_cookie(headers: &[Header]) -> Vec<Header> {
    headers
        .iter()
        .map(|header| {
            if !header.name.eq_ignore_ascii_case("set-cookie")
                && !header.name.eq_ignore_ascii_case("set-cookie2")
            {
                return header.clone();
            }
            let (pair, attributes) = match header.value.split_once(';') {
                Some((pair, attributes)) => (pair, Some(attributes)),
                None => (header.value.as_str(), None),
            };
            // A cookie without `=` is all value.
            let mut value = match pair.split_once('=') {
                Some((name, _)) => format!("{name}={REDACTED_COOKIE}"),
                None => REDACTED_COOKIE.to_owned(),
            };
            if let Some(attributes) = attributes {
                value.push(';');
                value.push_str(attributes);
            }
            Header {
                name: header.name.clone(),
                value,
            }
        })
        .collect()
}

/// The persona that produced a result, as reported to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaSummary {
    /// The persona.
    pub id: PersonaId,
    /// Its engine.
    pub engine: Engine,
    /// Its claimed operating system.
    pub claimed_os: ClaimedOs,
    /// Its proxy's country.
    #[serde(default)]
    pub country: Option<CountryCode>,
}

/// robots.txt verdict reported on every result (spec §14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RobotsVerdict {
    /// Whether robots.txt was fetched (policy `ignore` skips it).
    pub fetched: bool,
    /// Whether the URL is allowed.
    pub allowed: bool,
    /// The rule that decided, if any.
    #[serde(default)]
    pub matched_rule: Option<String>,
    /// `Crawl-delay` in seconds, if declared.
    #[serde(default)]
    pub crawl_delay: Option<f64>,
}

string_enum! {
    /// What an artifact holds.
    pub enum ArtifactKind {
        /// The raw network body of the main response.
        RawBody => "raw_body",
        /// The rendered DOM after settle.
        DomHtml => "dom_html",
        /// A screenshot.
        Screenshot => "screenshot",
        /// A PDF rendering.
        Pdf => "pdf",
        /// A captured XHR or fetch response body.
        XhrBody => "xhr_body",
    }
}

string_enum! {
    /// How an artifact's bytes are stored on disk.
    pub enum ArtifactEncoding {
        /// As they are.
        Identity => "identity",
        /// gzip-compressed (large text bodies, spec §18).
        Gzip => "gzip",
    }
}

/// SHA-256 of an artifact's content before compression, as 64 lowercase
/// hexadecimal characters. Artifacts are stored under this key.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentHash(String);

/// A string that is not 64 lowercase hexadecimal characters.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a content hash is 64 lowercase hexadecimal characters")]
pub struct InvalidContentHash;

impl ContentHash {
    /// Formats a SHA-256 digest.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        use fmt::Write as _;

        let mut text = String::with_capacity(64);
        for byte in digest {
            // Writing to a String cannot fail.
            let _ = write!(text, "{byte:02x}");
        }
        Self(text)
    }

    /// The hash as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.0)
    }
}

impl FromStr for ContentHash {
    type Err = InvalidContentHash;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let valid = text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidContentHash)
        }
    }
}

impl Serialize for ContentHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A stored blob: a body, screenshot or PDF on disk under `artifacts/`,
/// keyed by content hash, with a retention deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Identity; what the API hands out.
    pub id: ArtifactId,
    /// Hash of the content before compression.
    pub content_hash: ContentHash,
    /// What it is.
    pub kind: ArtifactKind,
    /// MIME type.
    pub content_type: String,
    /// Size of the content before compression.
    pub size_bytes: u64,
    /// Size on disk.
    pub stored_bytes: u64,
    /// How it is stored.
    pub encoding: ArtifactEncoding,
    /// The request that produced it, if still known.
    pub request_id: Option<RequestId>,
    /// When it was stored.
    pub created_at: DateTime<Utc>,
    /// When the retention job may delete it.
    pub expires_at: DateTime<Utc>,
}

/// A pointer from a result to one of its artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// What the artifact holds.
    pub kind: ArtifactKind,
    /// The artifact.
    pub id: ArtifactId,
}

/// The most entries of a browser's log a result keeps. A page can log
/// without end; what a result holds is bounded.
pub const MAX_CONSOLE_ENTRIES: usize = 1000;

/// The most bytes of one log entry's text a result keeps.
pub const MAX_CONSOLE_TEXT_BYTES: usize = 4096;

/// The pacing that applied to a request (spec §11, §16.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacingApplied {
    /// The profile's name; `custom` for one sent with the request.
    pub applied: String,
    /// Whether the request asked for pacing that the policy's lock
    /// overrode.
    #[serde(default)]
    pub request_ignored: bool,
    /// How long the request waited for personas and their slots, in
    /// milliseconds.
    #[serde(default)]
    pub waited_ms: u64,
}

/// What the persona did on the page of the last browser attempt (spec
/// §10.4, §16.5): what it was asked to do, and what the browser says it
/// did. The seed all of it was drawn from is the attempt's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorDone {
    /// Whether any of it was the request's, and not the policy's.
    #[serde(default)]
    pub from_request: bool,
    /// Whether the request asked for behavior that the policy's lock
    /// overrode.
    #[serde(default)]
    pub request_ignored: bool,
    /// How far the persona meant to scroll the page, from 0 to 1; 0 for no
    /// scrolling.
    #[serde(default)]
    pub scroll_depth: Option<f64>,
    /// How far down the document it came; `None` when the page was not
    /// scrolled or the browser did not say.
    #[serde(default)]
    pub scroll_reached: Option<f64>,
    /// Why the scrolling ended: `depth`, `patience` or `page`.
    #[serde(default)]
    pub scroll_ended_by: Option<String>,
    /// The patience the persona had with the page, in milliseconds.
    #[serde(default)]
    pub patience_ms: Option<u64>,
    /// The time spent on the page after it loaded, in milliseconds.
    #[serde(default)]
    pub dwell_ms: Option<u64>,
    /// How the page was arrived at.
    pub referer_strategy: RefererStrategy,
    /// Movements of the pointer.
    #[serde(default)]
    pub moves: Option<u64>,
    /// Notches of the wheel.
    #[serde(default)]
    pub notches: Option<u64>,
}

/// One entry of the browser's log (spec §10.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleEntry {
    /// `log`, `info`, `warning`, `error` or `debug`.
    pub level: String,
    /// Where the browser says the entry comes from.
    pub source: String,
    /// The text, cut at [`MAX_CONSOLE_TEXT_BYTES`].
    pub text: String,
    /// The URL it names, if any.
    #[serde(default)]
    pub url: Option<String>,
}

impl ConsoleEntry {
    /// An entry with its text cut to what a result keeps, at a character
    /// boundary.
    pub fn bounded(level: &str, source: &str, text: &str, url: Option<&str>) -> Self {
        // The end of the last character that still fits.
        let end = text
            .char_indices()
            .map(|(start, character)| start + character.len_utf8())
            .take_while(|end| *end <= MAX_CONSOLE_TEXT_BYTES)
            .last()
            .unwrap_or(0);
        Self {
            level: level.to_owned(),
            source: source.to_owned(),
            text: text[..end].to_owned(),
            url: url.map(str::to_owned),
        }
    }
}

/// What a result keeps of the visit that got its page, besides the page
/// itself (spec §16.5, follow-up F36): the pacing that applied to the
/// request, what the persona did on the page, the browser's log and the
/// events a capture fired in the page. `GET /v1/results/{id}` and an
/// answer from the response cache show them as the fetch's own answer did.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Visit {
    /// The pacing that applied; `None` for a result that is no fetch's
    /// (a capture point of a flow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pacing: Option<PacingApplied>,
    /// What the persona did on the page; `None` when no browser attempt
    /// was made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior: Option<BehaviorDone>,
    /// The browser's log, when the request asked for it: the first
    /// [`MAX_CONSOLE_ENTRIES`] entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<Vec<ConsoleEntry>>,
    /// Events a capture fired in the page (`resize`, `beforeprint`,
    /// `afterprint`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub side_effects: Vec<String>,
}

impl Visit {
    /// Whether nothing is kept: such a visit is stored as no document.
    pub fn is_empty(&self) -> bool {
        self.pacing.is_none()
            && self.behavior.is_none()
            && self.console.is_none()
            && self.side_effects.is_empty()
    }

    /// The log entries a result keeps of what a browser logged: the first
    /// [`MAX_CONSOLE_ENTRIES`].
    pub fn console_of(entries: impl IntoIterator<Item = ConsoleEntry>) -> Vec<ConsoleEntry> {
        entries.into_iter().take(MAX_CONSOLE_ENTRIES).collect()
    }
}

/// The stored result of a request (spec §5 "Result", §16.5).
///
/// Bodies are not inlined: they live in the artifact store and are listed in
/// `artifacts`. Attempts are stored separately and joined by `request_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchRecord {
    /// Identity.
    pub id: ResultId,
    /// The request this is the result of.
    pub request_id: RequestId,
    /// Final outcome.
    pub outcome: Outcome,
    /// Status of the last response, if any arrived.
    pub status: Option<u16>,
    /// Headers of the main response.
    pub headers: Vec<Header>,
    /// URL after redirects.
    pub final_url: Option<String>,
    /// Every URL visited before `final_url`, in order.
    pub redirects: Vec<String>,
    /// Extraction output (spec §14), when requested.
    pub extract: Option<serde_json::Value>,
    /// What a crawl took from the page (where it ended, its links, its
    /// text), kept until the crawl has recorded the page: a crawl that
    /// changes hands between the result and the page still knows the
    /// page's links (follow-up F49). `None` for every other result and
    /// once the crawl has recorded it.
    pub reading: Option<serde_json::Value>,
    /// Captured XHR and fetch responses, when requested.
    pub xhr: Option<serde_json::Value>,
    /// Bodies, screenshots and PDFs.
    pub artifacts: Vec<ArtifactRef>,
    /// The persona of the last attempt.
    pub persona: Option<PersonaSummary>,
    /// Block report when the outcome is a block.
    pub block: Option<Block>,
    /// robots.txt verdict.
    pub robots: Option<RobotsVerdict>,
    /// Whether this result was served from the response cache.
    pub cached: bool,
    /// When the page of a result that was served from the cache was
    /// fetched; `None` for a result that was fetched for its own request.
    pub cached_at: Option<DateTime<Utc>>,
    /// Why the request ended without the site's verdict, as a stable
    /// `snake_case` name: for example `domain_paused` (spec §8.3).
    pub reason: Option<String>,
    /// What is kept of the visit besides the page: the pacing, what the
    /// persona did, the browser's log, what a capture fired.
    pub visit: Visit,
    /// When the result was stored.
    pub created_at: DateTime<Utc>,
}

impl FetchRecord {
    /// The first artifact of the given kind, if any.
    pub fn artifact(&self, kind: ArtifactKind) -> Option<ArtifactId> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.kind == kind)
            .map(|artifact| artifact.id)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::{
        ArtifactEncoding, ArtifactKind, ArtifactRef, BehaviorDone, ConsoleEntry, ContentHash,
        FetchRecord, Header, InvalidContentHash, MAX_CONSOLE_ENTRIES, MAX_CONSOLE_TEXT_BYTES,
        PacingApplied, PersonaSummary, REDACTED_COOKIE, RobotsVerdict, Timing, Visit,
        redact_set_cookie,
    };
    use crate::domain::{ClaimedOs, Engine};
    use crate::ids::{ArtifactId, PersonaId, RequestId, ResultId};
    use crate::macros::test_support::assert_string_enum;
    use crate::outcome::Outcome;
    use crate::policy::RefererStrategy;

    #[test]
    fn enum_names_are_stable() {
        assert_string_enum(
            ArtifactKind::ALL,
            ArtifactKind::as_str,
            &["raw_body", "dom_html", "screenshot", "pdf", "xhr_body"],
        );
        assert_string_enum(
            ArtifactEncoding::ALL,
            ArtifactEncoding::as_str,
            &["identity", "gzip"],
        );
    }

    #[test]
    fn content_hash_is_lowercase_hex() {
        let hash = ContentHash::from_digest([0xab; 32]);

        assert_eq!(hash.as_str(), "ab".repeat(32));
        assert_eq!(hash.to_string().parse::<ContentHash>().unwrap(), hash);
        assert_eq!(
            format!("{hash:?}"),
            format!("ContentHash({})", "ab".repeat(32))
        );
        assert_eq!(
            serde_json::from_str::<ContentHash>(&format!("\"{hash}\"")).unwrap(),
            hash
        );
        assert_eq!(serde_json::to_string(&hash).unwrap(), format!("\"{hash}\""));

        for bad in [
            "",
            "abc",
            &"AB".repeat(32),
            &"zz".repeat(32),
            &"ab".repeat(33),
        ] {
            assert_eq!(
                bad.parse::<ContentHash>(),
                Err(InvalidContentHash),
                "{bad:?}"
            );
        }
        assert!(serde_json::from_str::<ContentHash>("\"short\"").is_err());
    }

    #[test]
    fn timing_tolerates_missing_phases() {
        let timing: Timing = serde_json::from_str(r#"{"ttfb_ms":410,"total_ms":9800}"#).unwrap();

        assert_eq!(timing.ttfb_ms, Some(410));
        assert_eq!(timing.dns_ms, None);
        assert_eq!(timing.total_ms, 9800);
        assert_eq!(Timing::default().total_ms, 0);
    }

    #[test]
    fn stored_json_shapes_round_trip() {
        let robots = RobotsVerdict {
            fetched: true,
            allowed: false,
            matched_rule: Some("Disallow: /private".to_owned()),
            crawl_delay: Some(2.0),
        };
        let json = serde_json::to_string(&robots).unwrap();
        assert_eq!(
            serde_json::from_str::<RobotsVerdict>(&json).unwrap(),
            robots
        );
        let minimal: RobotsVerdict =
            serde_json::from_str(r#"{"fetched":false,"allowed":true}"#).unwrap();
        assert_eq!(minimal.matched_rule, None);

        let persona = PersonaSummary {
            id: PersonaId::new(),
            engine: Engine::Firefox,
            claimed_os: ClaimedOs::Macos,
            country: Some("AT".parse().unwrap()),
        };
        let json = serde_json::to_value(&persona).unwrap();
        assert_eq!(json["engine"], "firefox");
        assert_eq!(json["country"], "AT");
        assert_eq!(
            serde_json::from_value::<PersonaSummary>(json).unwrap(),
            persona
        );

        let header = Header {
            name: "Content-Type".to_owned(),
            value: "text/html".to_owned(),
        };
        let json = serde_json::to_string(&header).unwrap();
        assert_eq!(serde_json::from_str::<Header>(&json).unwrap(), header);
    }

    #[test]
    fn a_cookie_a_site_sets_keeps_its_name_and_loses_its_value() {
        let header = |name: &str, value: &str| Header {
            name: name.to_owned(),
            value: value.to_owned(),
        };
        let headers = [
            header("content-type", "text/html"),
            header(
                "set-cookie",
                "cf_clearance=AbC.dEf-123; Path=/; Expires=Thu, 08 Oct 2027 10:00:00 GMT; Secure; HttpOnly; SameSite=None",
            ),
            header("Set-Cookie", "sid=a=b=c"),
            header("SET-COOKIE", "bare-value"),
            header("set-cookie", "=nameless; Path=/"),
            header("x-set-cookie", "left=alone"),
        ];

        let redacted = redact_set_cookie(&headers);

        assert_eq!(redacted.len(), headers.len());
        assert_eq!(redacted[0], headers[0]);
        assert_eq!(
            redacted[1].value,
            "cf_clearance=[redacted]; Path=/; Expires=Thu, 08 Oct 2027 10:00:00 GMT; Secure; HttpOnly; SameSite=None"
        );
        // The name is what stands before the first `=`.
        assert_eq!(redacted[2].value, "sid=[redacted]");
        assert_eq!(redacted[2].name, "Set-Cookie");
        assert_eq!(redacted[3].value, REDACTED_COOKIE);
        assert_eq!(redacted[4].value, "=[redacted]; Path=/");
        assert_eq!(redacted[5], headers[5]);
        // No value survives anywhere.
        let text = serde_json::to_string(&redacted).unwrap();
        for secret in ["AbC.dEf-123", "a=b=c", "bare-value", "nameless"] {
            assert!(!text.contains(secret), "{secret}");
        }
        assert_eq!(redact_set_cookie(&[]), Vec::<Header>::new());
    }

    #[test]
    fn record_finds_artifacts_by_kind() {
        let dom = ArtifactId::new();
        let shot = ArtifactId::new();
        let record = FetchRecord {
            id: ResultId::new(),
            request_id: RequestId::new(),
            outcome: Outcome::Ok,
            status: Some(200),
            headers: Vec::new(),
            final_url: Some("https://example.org/".to_owned()),
            redirects: Vec::new(),
            extract: None,
            xhr: None,
            artifacts: vec![
                ArtifactRef {
                    kind: ArtifactKind::DomHtml,
                    id: dom,
                },
                ArtifactRef {
                    kind: ArtifactKind::Screenshot,
                    id: shot,
                },
            ],
            persona: None,
            block: None,
            robots: None,
            cached: false,
            cached_at: None,
            reason: None,
            visit: Visit::default(),
            reading: None,
            created_at: Utc::now(),
        };

        assert_eq!(record.artifact(ArtifactKind::DomHtml), Some(dom));
        assert_eq!(record.artifact(ArtifactKind::Screenshot), Some(shot));
        assert_eq!(record.artifact(ArtifactKind::Pdf), None);
    }

    #[test]
    fn a_visit_is_stored_as_what_it_holds_and_read_back_from_less() {
        // Nothing kept is no member at all: an older row has none either.
        let nothing = Visit::default();
        assert!(nothing.is_empty());
        assert_eq!(serde_json::to_value(&nothing).unwrap(), json!({}));
        assert_eq!(serde_json::from_value::<Visit>(json!({})).unwrap(), nothing);

        let visit = Visit {
            pacing: Some(PacingApplied {
                applied: "human".to_owned(),
                request_ignored: false,
                waited_ms: 3900,
            }),
            behavior: Some(BehaviorDone {
                from_request: true,
                request_ignored: false,
                scroll_depth: Some(0.8),
                scroll_reached: Some(0.76),
                scroll_ended_by: Some("depth".to_owned()),
                patience_ms: Some(38_000),
                dwell_ms: Some(6100),
                referer_strategy: RefererStrategy::Direct,
                moves: Some(87),
                notches: Some(12),
            }),
            console: Some(vec![ConsoleEntry::bounded(
                "error",
                "network",
                "Failed to load resource",
                Some("https://example.org/missing.js"),
            )]),
            side_effects: vec!["resize".to_owned()],
        };
        assert!(!visit.is_empty());
        let stored = serde_json::to_value(&visit).unwrap();
        assert_eq!(stored["pacing"]["waited_ms"], 3900);
        assert_eq!(stored["behavior"]["scroll_ended_by"], "depth");
        assert_eq!(
            stored["console"][0]["url"],
            "https://example.org/missing.js"
        );
        assert_eq!(stored["side_effects"], json!(["resize"]));
        assert_eq!(serde_json::from_value::<Visit>(stored).unwrap(), visit);
        // Each part alone makes a visit one that is kept.
        for part in ["pacing", "behavior", "console", "side_effects"] {
            let mut one = Visit::default();
            match part {
                "pacing" => one.pacing.clone_from(&visit.pacing),
                "behavior" => one.behavior.clone_from(&visit.behavior),
                "console" => one.console = Some(Vec::new()),
                _ => one.side_effects.clone_from(&visit.side_effects),
            }
            assert!(!one.is_empty(), "{part}");
        }
        // What a later version leaves out of a part reads as nothing said.
        let sparse: Visit = serde_json::from_value(json!({
            "pacing": { "applied": "fast" },
            "behavior": { "referer_strategy": "none" },
        }))
        .unwrap();
        assert_eq!(sparse.pacing.unwrap().waited_ms, 0);
        let behavior = sparse.behavior.unwrap();
        assert_eq!((behavior.dwell_ms, behavior.moves), (None, None));
        assert!(!behavior.from_request);
    }

    #[test]
    fn a_result_keeps_a_bounded_part_of_a_browsers_log() {
        // The numbers themselves: a constant that is wrong moves with the
        // tests that name it.
        assert_eq!((MAX_CONSOLE_ENTRIES, MAX_CONSOLE_TEXT_BYTES), (1000, 4096));
        let entry = |n: usize| ConsoleEntry::bounded("log", "console", &format!("line {n}"), None);
        let kept = Visit::console_of((0..1500).map(entry));
        assert_eq!(kept.len(), 1000);
        assert_eq!(
            (kept[0].text.as_str(), kept[999].text.as_str()),
            ("line 0", "line 999")
        );
        assert_eq!(Visit::console_of((0..1000).map(entry)).len(), 1000);
        assert_eq!(Visit::console_of((0..3).map(entry)).len(), 3);

        // A text is cut at the bound, never inside a character.
        let short = ConsoleEntry::bounded("log", "console", &"a".repeat(4096), None);
        assert_eq!(short.text.len(), 4096);
        let long = ConsoleEntry::bounded("log", "console", &"a".repeat(4097), None);
        assert_eq!(long.text.len(), 4096);
        // "é" is two bytes: 4095 bytes of text and one that would be cut.
        let split = format!("{}é", "a".repeat(4095));
        let cut = ConsoleEntry::bounded("warning", "security", &split, Some("https://a.example/"));
        assert_eq!(cut.text, "a".repeat(4095));
        assert_eq!(
            (cut.level.as_str(), cut.source.as_str(), cut.url.as_deref()),
            ("warning", "security", Some("https://a.example/"))
        );
        let whole = format!("{}é", "a".repeat(4094));
        assert_eq!(
            ConsoleEntry::bounded("log", "console", &whole, None).text,
            whole
        );
    }
}
