//! Requests, attempts, stored results and artifacts (spec §5, §16.5, §18).

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{ClaimedOs, CountryCode, Engine, Render, RenderMode, SessionId};
use crate::ids::{ApiKeyId, ArtifactId, AttemptId, JobId, PersonaId, RequestId, ResultId};
use crate::macros::string_enum;
use crate::outcome::{Block, Outcome};

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
}

/// One response header. A list keeps order and repeated names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// Header name as received.
    pub name: String,
    /// Header value.
    pub value: String,
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

    use super::{
        ArtifactEncoding, ArtifactKind, ArtifactRef, ContentHash, FetchRecord, Header,
        InvalidContentHash, PersonaSummary, RobotsVerdict, Timing,
    };
    use crate::domain::{ClaimedOs, Engine};
    use crate::ids::{ArtifactId, PersonaId, RequestId, ResultId};
    use crate::macros::test_support::assert_string_enum;
    use crate::outcome::Outcome;

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
            created_at: Utc::now(),
        };

        assert_eq!(record.artifact(ArtifactKind::DomHtml), Some(dom));
        assert_eq!(record.artifact(ArtifactKind::Screenshot), Some(shot));
        assert_eq!(record.artifact(ArtifactKind::Pdf), None);
    }
}
