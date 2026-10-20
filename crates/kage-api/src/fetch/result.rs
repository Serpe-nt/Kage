//! The result of a fetch as the API returns it (spec §16.5).
//!
//! The same document answers `POST /v1/fetch` and `GET /v1/results/{id}`.
//! The first is built from what the pipeline just did, the second from
//! what was stored, and both say the same: what the answer says of the
//! visit besides the page (`pacing`, `behavior`, `console`,
//! `side_effects`) is stored with the result (follow-up F36). A fetch that
//! was answered from the response cache is the second kind from the
//! start: the stored result of the visit that filled the cache, under the
//! request's own id, with `cached: true` and no attempts; what the
//! persona did on the page, the browser's log and what a capture fired
//! are that visit's, the pacing is the request's own.
//!
//! What a result shows of the pool depends on who asks (spec §19). A key
//! without the `admin` scope sees its persona as a handle
//! ([`crate::handle`]) with the engine, the claimed system and the
//! country, and nothing of a proxy. The values of the cookies a site set
//! are left out for everyone unless `security.expose_set_cookie` is on.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use kage_browser::contract::{BodyEncoding, PageResult, ScrollEnd};
use kage_core::domain::{
    ArtifactKind, ArtifactRef, Attempt, BehaviorDone, ClaimedOs, ConsoleEntry, Exit, FetchRecord,
    Header, Level, PacingApplied, PersonaSummary, Render, RobotsVerdict, Timing, Visit,
};
use kage_core::ids::{ApiKeyId, ArtifactId, PersonaId, RequestId};
use kage_core::outcome::{Block, Outcome};
use kage_core::seed::Seed;
use kage_scheduler::FetchReport;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use super::request::{EngineField, Include, Included, RefererStrategyField};
use crate::handle::Handles;
use crate::schema::FreeForm;

/// What an artifact's id starts with in the API.
pub(crate) const ARTIFACT_PREFIX: &str = "art_";

/// An artifact's id as the API writes it: `art_` and 32 hexadecimal
/// characters.
pub(crate) fn artifact_handle(id: ArtifactId) -> String {
    format!("{ARTIFACT_PREFIX}{}", id.as_uuid().simple())
}

/// Reads an artifact's id as the API writes it; the bare UUID is taken
/// too.
pub(crate) fn parse_artifact(text: &str) -> Option<ArtifactId> {
    let text = text.strip_prefix(ARTIFACT_PREFIX).unwrap_or(text);
    uuid::Uuid::try_parse(text).ok().map(ArtifactId::from_uuid)
}

/// How an attempt or a request ended (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutcomeField {
    /// The page was fetched; also a 4xx other than 403 and 429.
    Ok,
    /// A challenge appeared and resolved by itself in the browser.
    ChallengePassed,
    /// A challenge did not resolve.
    ChallengeFailed,
    /// A CAPTCHA stands in front of the content; Kage solves none.
    Captcha,
    /// The site answered with a rate limit.
    RateLimited,
    /// The site refused the request.
    Forbidden,
    /// A success status with nothing behind it.
    SoftBlock,
    /// It failed outside the site's defences; `reason` says where.
    ProviderError,
    /// The attempt ran out of time.
    Timeout,
    /// The guard refused a redirect hop.
    SsrfDenied,
    /// robots.txt disallows the URL and the policy enforces it.
    RobotsDenied,
}

impl From<Outcome> for OutcomeField {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Ok => Self::Ok,
            Outcome::ChallengePassed => Self::ChallengePassed,
            Outcome::ChallengeFailed => Self::ChallengeFailed,
            Outcome::Captcha => Self::Captcha,
            Outcome::RateLimited => Self::RateLimited,
            Outcome::Forbidden => Self::Forbidden,
            Outcome::SoftBlock => Self::SoftBlock,
            Outcome::ProviderError => Self::ProviderError,
            Outcome::Timeout => Self::Timeout,
            Outcome::SsrfDenied => Self::SsrfDenied,
            Outcome::RobotsDenied => Self::RobotsDenied,
        }
    }
}

/// The path an attempt took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RenderTaken {
    /// The http path.
    Http,
    /// A browser.
    Browser,
}

impl From<Render> for RenderTaken {
    fn from(render: Render) -> Self {
        match render {
            Render::Http => Self::Http,
            Render::Browser => Self::Browser,
        }
    }
}

/// Where a plain client's attempt (level 0) left from: never an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExitField {
    /// The host's own connection (`egress.plain: host`).
    Host,
    /// A proxy of the pool: the site's plain exit (`egress.plain: pool`).
    Pool,
}

impl From<Exit> for ExitField {
    fn from(exit: Exit) -> Self {
        match exit {
            Exit::Host => Self::Host,
            Exit::Pool => Self::Pool,
        }
    }
}

/// The operating system a persona claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ClaimedOsField {
    /// Windows.
    Windows,
    /// macOS.
    Macos,
    /// Linux.
    Linux,
}

impl From<ClaimedOs> for ClaimedOsField {
    fn from(os: ClaimedOs) -> Self {
        match os {
            ClaimedOs::Windows => Self::Windows,
            ClaimedOs::Macos => Self::Macos,
            ClaimedOs::Linux => Self::Linux,
        }
    }
}

/// One response header. A list keeps the order and repeated names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct HeaderView {
    /// The name as received.
    pub(crate) name: String,
    /// The value. Of a `Set-Cookie` only the cookie's name and attributes:
    /// its value reads `[redacted]` unless the operator exposes it.
    pub(crate) value: String,
}

/// The persona that made a request, as its caller may know it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct PersonaView {
    /// For a key without the `admin` scope a handle (`p_` and 16
    /// hexadecimal characters) that is the same for this key and this
    /// persona every time and means nothing to another key; for an admin
    /// key the persona's id.
    #[schema(example = "p_3f9a6c0d12e4b87a")]
    pub(crate) id: String,
    /// Its engine.
    pub(crate) engine: EngineField,
    /// The operating system it claims.
    pub(crate) claimed_os: ClaimedOsField,
    /// The country its address is in.
    #[schema(example = "DE")]
    pub(crate) country: Option<String>,
}

/// One try at a request: by one persona, or at level 0 by the plain
/// client from its exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct AttemptView {
    /// 1 for the first attempt.
    pub(crate) n: u32,
    /// The path it took.
    pub(crate) render: RenderTaken,
    /// How it ended.
    pub(crate) outcome: OutcomeField,
    /// The status of the response, when one arrived.
    pub(crate) status: Option<u16>,
    /// How long it took, in milliseconds.
    pub(crate) total_ms: u64,
    /// The persona that made it, as in `persona.id`; null at level 0.
    pub(crate) persona: Option<String>,
    /// The engine of that persona; null at level 0.
    pub(crate) engine: Option<EngineField>,
    /// Where a level-0 attempt left from; null for a persona's.
    pub(crate) exit: Option<ExitField>,
    /// Why it ended as it did when the site did not say, for example
    /// `proxy_failed`.
    pub(crate) reason: Option<String>,
    /// The seed its behavior and pauses were drawn from. Admin keys only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) seed: Option<String>,
}

/// Who blocked, and how (spec §12).
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct BlockView {
    /// The vendor, for example `cloudflare`.
    pub(crate) vendor: String,
    /// The kind of block, for example `managed_challenge`.
    #[serde(rename = "type")]
    pub(crate) kind: String,
    /// How sure the signature is, from 0 to 1.
    pub(crate) confidence: f32,
    /// The markers that matched.
    pub(crate) evidence: Vec<String>,
}

impl From<&Block> for BlockView {
    fn from(block: &Block) -> Self {
        Self {
            vendor: block.vendor.as_str().to_owned(),
            kind: block.block_type.as_str().to_owned(),
            confidence: block.confidence,
            evidence: block.evidence.clone(),
        }
    }
}

/// Time per phase of the last attempt, in milliseconds; null for a phase
/// that did not happen on the path taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, ToSchema)]
#[allow(clippy::struct_field_names)] // The names are the API's.
pub(crate) struct TimingView {
    /// TCP to the proxy and the tunnel (http path).
    pub(crate) connect_ms: Option<u64>,
    /// The TLS handshake (http path).
    pub(crate) tls_ms: Option<u64>,
    /// To the first byte of the response.
    pub(crate) ttfb_ms: Option<u64>,
    /// To `DOMContentLoaded` (browser).
    pub(crate) dom_content_loaded_ms: Option<u64>,
    /// To the load event (browser).
    pub(crate) load_ms: Option<u64>,
    /// The whole attempt: on a browser render this includes the time the
    /// persona spent on the page.
    pub(crate) total_ms: u64,
}

impl From<Timing> for TimingView {
    fn from(timing: Timing) -> Self {
        Self {
            connect_ms: timing.connect_ms,
            tls_ms: timing.tls_ms,
            ttfb_ms: timing.ttfb_ms,
            dom_content_loaded_ms: timing.dom_content_loaded_ms,
            load_ms: timing.load_ms,
            total_ms: timing.total_ms,
        }
    }
}

/// The pacing that applied (spec §11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct PacingView {
    /// The profile's name; `custom` for one sent with the request.
    pub(crate) applied: String,
    /// Whether the request asked for pacing that the policy's lock
    /// overrode.
    pub(crate) request_ignored: bool,
    /// How long the request waited for personas and their slots.
    pub(crate) waited_ms: u64,
}

/// Whose the behavior on the page was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BehaviorSource {
    /// The domain policy's default.
    Policy,
    /// The request's, inside what the policy allows.
    Request,
}

/// Why the scrolling of a page ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScrollEndField {
    /// The persona came as far as it meant to.
    Depth,
    /// Its patience with the page ended first.
    Patience,
    /// The page does not scroll, or not any further.
    Page,
}

impl ScrollEndField {
    /// What a browser says ended the scrolling, as a result shows it.
    /// Kage gives every page a patience, so the time to reach the page
    /// (`budget`) never ends it.
    pub(crate) const fn of(end: ScrollEnd) -> Option<Self> {
        match end {
            ScrollEnd::Depth => Some(Self::Depth),
            ScrollEnd::Patience => Some(Self::Patience),
            ScrollEnd::Page => Some(Self::Page),
            ScrollEnd::Budget => None,
        }
    }

    /// What a result keeps as the end of the scrolling, by its name.
    /// Kage gives every page a patience, so the time to reach the page
    /// (a browser's `budget`) never ends it and is not kept.
    pub(crate) fn named(name: &str) -> Option<Self> {
        match name {
            "depth" => Some(Self::Depth),
            "patience" => Some(Self::Patience),
            "page" => Some(Self::Page),
            _ => None,
        }
    }

    /// Its name.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Depth => "depth",
            Self::Patience => "patience",
            Self::Page => "page",
        }
    }
}

/// What the persona did on the page of the last browser attempt.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct BehaviorView {
    /// Whose it was.
    pub(crate) applied: BehaviorSource,
    /// Whether the request asked for behavior that the policy's lock
    /// overrode.
    pub(crate) request_ignored: bool,
    /// How far the persona meant to scroll the page, from 0 to 1; 0 for
    /// no scrolling.
    pub(crate) scroll_depth: Option<f64>,
    /// How far down the document it came, from 0 to 1; null when the page
    /// was not scrolled or the browser did not say.
    pub(crate) scroll_reached: Option<f64>,
    /// Why the scrolling ended: `depth` (as far as it meant to go),
    /// `patience` (its patience with the page ended first) or `page` (the
    /// page does not go further); null when the page was not scrolled.
    #[schema(example = "depth")]
    pub(crate) scroll_ended_by: Option<ScrollEndField>,
    /// The patience the persona had with this page, in milliseconds: how
    /// long it would have gone on scrolling at most. Kage draws it anew
    /// for every page; `timeout_ms` bounds reaching the page and nothing
    /// after it. Null for a flow, which scrolls where its steps say.
    pub(crate) patience_ms: Option<u64>,
    /// The time spent on the page after it loaded, in milliseconds.
    pub(crate) dwell_ms: Option<u64>,
    /// How the page was arrived at.
    pub(crate) referer_strategy: RefererStrategyField,
    /// Movements of the pointer.
    pub(crate) moves: Option<u64>,
    /// Notches of the wheel.
    pub(crate) notches: Option<u64>,
    /// The seed all of it was drawn from. Admin keys only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) seed: Option<String>,
}

/// One response a script of the page fetched.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct XhrView {
    /// The responses that matched the request's filter: each with `url`,
    /// `method`, `kind` (`xhr` or `fetch`), `status`, `content_type`,
    /// `bytes`, `truncated`, and its body as `body` (text, when it is at
    /// most 256 kB) or as `body_artifact` (an artifact's id).
    #[schema(value_type = Vec<FreeForm>)]
    pub(crate) responses: Vec<Value>,
    /// Matching responses that were not kept because the page had more
    /// than `xhr.max_responses`.
    pub(crate) dropped: u64,
}

/// One entry of the browser's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct ConsoleView {
    /// `log`, `info`, `warning`, `error` or `debug`.
    pub(crate) level: String,
    /// Where the entry comes from.
    pub(crate) source: String,
    /// The text.
    pub(crate) text: String,
    /// The URL it names, if any.
    pub(crate) url: Option<String>,
}

/// Whether a fetch has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResultState {
    /// The fetch is still under way.
    Running,
    /// The fetch has ended; `outcome` says how.
    Finished,
}

/// The result of a fetch (spec §16.5).
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct FetchResult {
    /// The request's id, as in `X-Request-Id`; `GET /v1/results/{id}`
    /// returns this document again.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) id: RequestId,
    /// Always `finished` here; a fetch still under way is a
    /// [`FetchAccepted`].
    pub(crate) state: ResultState,
    /// How the request ended. A block is an outcome, not an HTTP error:
    /// the status of this answer is 200 whenever a fetch was made.
    pub(crate) outcome: OutcomeField,
    /// Why it ended without the site's verdict, as a stable name:
    /// `no_persona`, `domain_paused`, `egress_unsafe`,
    /// `bandwidth_exhausted`, `proxy_failed`, `timeout`, `interrupted`
    /// (the server stopped while the page was being read) and others.
    pub(crate) reason: Option<String>,
    /// When asking again is worth it, when Kage knows: the end of a
    /// domain's pause or of a persona's cooldown.
    pub(crate) retry_at: Option<DateTime<Utc>>,
    /// The level the request was made at (spec §25); null for a result
    /// stored before 2.0.
    #[schema(minimum = 0, maximum = 3, example = 3)]
    pub(crate) level: Option<u64>,
    /// The status of the last response.
    pub(crate) status: Option<u16>,
    /// The URL after redirects.
    pub(crate) final_url: Option<String>,
    /// Every URL visited before `final_url`, in order.
    pub(crate) redirects: Vec<String>,
    /// The headers of the main response; with `include: ["headers"]`.
    pub(crate) headers: Option<Vec<HeaderView>>,
    /// The document as the browser had it after the page settled; with
    /// `include: ["dom"]`. Null on the http path, which has no DOM: the
    /// document is `raw_body` there.
    pub(crate) dom_html: Option<String>,
    /// The body of the main response as text; with `include: ["raw"]`,
    /// and on the http path with `include: ["dom"]` too. Null when the
    /// body is not text: `artifacts.raw_body` has it then.
    pub(crate) raw_body: Option<String>,
    /// What the page says; with `include: ["extract"]`. Null for a fetch
    /// that did not get its page and for a response that is no HTML
    /// document.
    pub(crate) extract: Option<ExtractView>,
    /// The responses the page's scripts fetched; with `include: ["xhr"]`.
    pub(crate) xhr: Option<XhrView>,
    /// The browser's log; with `include: ["console"]`. At most the first
    /// 1000 entries, each text cut at 4096 bytes.
    pub(crate) console: Option<Vec<ConsoleView>>,
    /// What was stored of the page, by kind (`dom_html`, `raw_body`,
    /// `screenshot`, `pdf`): ids for `GET /v1/artifacts/{id}`.
    pub(crate) artifacts: BTreeMap<String, String>,
    /// Events a capture fired in the page (`resize` for a full-page
    /// screenshot on chromium, `beforeprint` and `afterprint` for a PDF).
    pub(crate) side_effects: Vec<String>,
    /// Time per phase of the last attempt.
    pub(crate) timing: TimingView,
    /// The persona of the last attempt.
    pub(crate) persona: Option<PersonaView>,
    /// Every attempt, in order.
    pub(crate) attempts: Vec<AttemptView>,
    /// Who blocked and how, when the outcome is a block.
    pub(crate) block: Option<BlockView>,
    /// What the site's `robots.txt` says of the address. Null for a
    /// request that ended before it was looked at (a refused URL).
    pub(crate) robots: Option<RobotsView>,
    /// The pacing that applied to the request. For a result served from
    /// the cache it is the request's own, which waited for nobody.
    pub(crate) pacing: Option<PacingView>,
    /// What the persona did on the page, for a browser attempt. For a
    /// result served from the cache: on the visit that got the page.
    pub(crate) behavior: Option<BehaviorView>,
    /// The session the request was made in.
    pub(crate) session_id: Option<String>,
    /// Whether the result was served from the response cache: nothing was
    /// asked of the site, `attempts` is empty, and the page is the one a
    /// visit at `cached_at` got.
    pub(crate) cached: bool,
    /// When the page of a result served from the cache was fetched.
    pub(crate) cached_at: Option<DateTime<Utc>>,
    /// When the result was stored.
    pub(crate) created_at: DateTime<Utc>,
}

/// What the site's `robots.txt` says of the address (spec §14).
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct RobotsView {
    /// Whether the verdict is the file's. False when nothing is known of
    /// it: the site's policy never asks for it (the default), the request
    /// said `ignore`, or the site's scout has not brought it yet.
    pub(crate) fetched: bool,
    /// Whether the file allows the address; true when nothing is known.
    /// Under `report` a page that is not allowed is fetched all the same.
    pub(crate) allowed: bool,
    /// The line that decided, as `Allow: <pattern>` or `Disallow:
    /// <pattern>`; null when no rule matched.
    #[schema(example = "Disallow: /private")]
    pub(crate) matched_rule: Option<String>,
    /// The `Crawl-delay` of the group that applies, in seconds. Kept to
    /// only where the file is enforced.
    pub(crate) crawl_delay: Option<f64>,
}

impl From<&RobotsVerdict> for RobotsView {
    fn from(verdict: &RobotsVerdict) -> Self {
        Self {
            fetched: verdict.fetched,
            allowed: verdict.allowed,
            matched_rule: verdict.matched_rule.clone(),
            crawl_delay: verdict.crawl_delay,
        }
    }
}

/// A heading of a page's main content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeadingView {
    /// 1 for `h1` to 6 for `h6`.
    #[schema(minimum = 1, maximum = 6)]
    pub(crate) level: u8,
    /// Its text.
    pub(crate) text: String,
    /// Its `id`, when it has one: the fragment that links to it.
    pub(crate) id: Option<String>,
}

/// A link of a page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct LinkView {
    /// Where it leads, as an absolute address.
    pub(crate) url: String,
    /// Its text.
    pub(crate) text: String,
    /// The tokens of its `rel`, in lower case.
    pub(crate) rel: Vec<String>,
    /// Whether it leads to a page of the site the document was loaded
    /// from (the same registrable domain).
    pub(crate) same_site: bool,
}

/// What a page says (spec §14): read from the document the visit captured,
/// never by a second request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtractView {
    /// The document's `<title>`.
    pub(crate) title: Option<String>,
    /// `<meta name="description">`.
    pub(crate) description: Option<String>,
    /// `<link rel="canonical">`, as an absolute address.
    pub(crate) canonical: Option<String>,
    /// The language the document says it is in (`<html lang>`).
    pub(crate) lang: Option<String>,
    /// When the page says it was published: from its JSON-LD, else its
    /// `article:` metadata, else other metadata, else a `<time>` of its
    /// content. In UTC when the page named a zone, as written when not.
    pub(crate) published_at: Option<String>,
    /// When the page says it was last changed, read the same way.
    pub(crate) modified_at: Option<String>,
    /// Every `<meta name>` with content, by name in lower case: its values
    /// in document order.
    pub(crate) meta: BTreeMap<String, Vec<String>>,
    /// The OpenGraph properties (`og:`, `article:` and their kin), by
    /// name.
    pub(crate) opengraph: BTreeMap<String, Vec<String>>,
    /// The Twitter card properties, by name.
    pub(crate) twitter: BTreeMap<String, Vec<String>>,
    /// Every JSON-LD block that parses, a top-level array as its members;
    /// at most 64.
    #[schema(value_type = Vec<FreeForm>)]
    pub(crate) jsonld: Vec<Value>,
    /// JSON-LD blocks left out: not JSON, too large, or beyond the 64.
    pub(crate) jsonld_skipped: u32,
    /// The links a reader can see, in document order; null when the
    /// request said `extract.links: false`.
    pub(crate) links: Option<Vec<LinkView>>,
    /// Whether the page held more links than are listed.
    pub(crate) links_truncated: bool,
    /// The headings of the main content, in order.
    pub(crate) headings: Vec<HeadingView>,
    /// The main content as text: what a reader would read, without the
    /// navigation, the side bars and the footer.
    pub(crate) text: String,
    /// The main content as markdown: headings, lists, fenced code with its
    /// language, tables, absolute links. Null when the request said
    /// `extract.markdown: false`.
    pub(crate) markdown: Option<String>,
    /// How many words the main content has.
    pub(crate) word_count: u64,
    /// Whether the main content has fewer words than `extract.thin_words`
    /// (80 unless configured): the document gave a reader next to nothing.
    /// On the http path that usually means scripts or a wall.
    pub(crate) thin_content: bool,
}

/// What was extracted, as the API returns it; null for what was stored in
/// a shape this release does not know.
fn extract_of(stored: Option<&Value>) -> Option<ExtractView> {
    serde_json::from_value(stored?.clone()).ok()
}

/// The answer to a fetch that is not finished: `202`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct FetchAccepted {
    /// The request's id.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) id: RequestId,
    /// Always `running`.
    pub(crate) state: ResultState,
    /// The job that carries the fetch on: `GET /v1/jobs/{id}` says where
    /// it stands (it may be `paused`, for a site or for its persona), and
    /// `POST /v1/jobs/{id}/cancel` stops it. Null for a result that is
    /// asked for while a job's item is still being fetched.
    #[schema(value_type = Option<String>, format = Uuid)]
    pub(crate) job_id: Option<uuid::Uuid>,
    /// Where the result will be: `GET` it until it is `finished`.
    #[schema(example = "/v1/results/0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e")]
    pub(crate) result: String,
}

impl FetchAccepted {
    pub(crate) fn running(id: RequestId) -> Self {
        Self {
            id,
            state: ResultState::Running,
            job_id: None,
            result: format!("/v1/results/{id}"),
        }
    }

    /// A fetch that a job carries on.
    pub(crate) fn carried(id: RequestId, job: kage_core::ids::JobId) -> Self {
        Self {
            job_id: Some(job.as_uuid()),
            ..Self::running(id)
        }
    }
}

/// Who is looking at a result.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Viewer<'a> {
    /// The key.
    pub(crate) key: ApiKeyId,
    /// Whether the key has the `admin` scope.
    pub(crate) admin: bool,
    /// Makes the handles.
    pub(crate) handles: &'a Handles,
}

impl Viewer<'_> {
    /// A persona's name for this viewer.
    pub(crate) fn name(&self, persona: PersonaId) -> String {
        if self.admin {
            persona.to_string()
        } else {
            self.handles.of(self.key, persona)
        }
    }

    pub(crate) fn persona(&self, summary: &PersonaSummary) -> PersonaView {
        PersonaView {
            id: self.name(summary.id),
            engine: summary.engine.into(),
            claimed_os: summary.claimed_os.into(),
            country: summary.country.map(|country| country.to_string()),
        }
    }

    fn attempt(&self, attempt: &Attempt) -> AttemptView {
        AttemptView {
            n: attempt.n,
            render: attempt.render.into(),
            outcome: attempt.outcome.into(),
            status: attempt.status,
            total_ms: attempt.timing.total_ms,
            persona: attempt.persona_id.map(|persona| self.name(persona)),
            engine: attempt.engine.map(Into::into),
            exit: attempt.exit.map(Into::into),
            reason: attempt.reason.clone(),
            seed: attempt
                .seed
                .filter(|_| self.admin)
                .map(|seed| seed.to_string()),
        }
    }
}

/// The bodies of a result, when they are at hand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Bodies {
    /// The document.
    pub(crate) dom_html: Option<String>,
    /// The raw body, when it is text.
    pub(crate) raw_body: Option<String>,
}

fn headers_of(headers: &[Header]) -> Vec<HeaderView> {
    headers
        .iter()
        .map(|header| HeaderView {
            name: header.name.clone(),
            value: header.value.clone(),
        })
        .collect()
}

/// The artifacts of a result by kind. The bodies of XHR responses are
/// named by the responses themselves.
fn artifacts_of(artifacts: &[ArtifactRef]) -> BTreeMap<String, String> {
    let mut by_kind = BTreeMap::new();
    for artifact in artifacts {
        if artifact.kind != ArtifactKind::XhrBody {
            by_kind
                .entry(artifact.kind.as_str().to_owned())
                .or_insert_with(|| artifact_handle(artifact.id));
        }
    }
    by_kind
}

/// The stored XHR document as the API returns it: the ids of body
/// artifacts in the API's spelling.
fn xhr_of(stored: Option<&Value>) -> Option<XhrView> {
    let stored = stored?;
    let mut responses: Vec<Value> = stored
        .get("responses")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for response in &mut responses {
        let handle = response
            .get("body_artifact")
            .and_then(Value::as_str)
            .and_then(parse_artifact)
            .map(artifact_handle);
        if let (Some(handle), Some(object)) = (handle, response.as_object_mut()) {
            object.insert("body_artifact".to_owned(), Value::String(handle));
        }
    }
    Some(XhrView {
        responses,
        dropped: stored.get("dropped").and_then(Value::as_u64).unwrap_or(0),
    })
}

/// The bodies a page holds in memory.
fn bodies_of_page(page: &PageResult) -> Bodies {
    Bodies {
        dom_html: page.dom_html.clone(),
        raw_body: page
            .raw_body
            .as_ref()
            .filter(|body| body.encoding == BodyEncoding::Utf8)
            .map(|body| body.data.clone()),
    }
}

/// The pacing a result keeps, as the API shows it.
fn pacing_of(kept: &PacingApplied) -> PacingView {
    PacingView {
        applied: kept.applied.clone(),
        request_ignored: kept.request_ignored,
        waited_ms: kept.waited_ms,
    }
}

/// What a result keeps of the persona's behavior, as the API shows it.
/// `seed` is the seed of the attempt it was drawn from.
fn behavior_of(kept: &BehaviorDone, seed: Option<Seed>, viewer: &Viewer<'_>) -> BehaviorView {
    BehaviorView {
        applied: if kept.from_request {
            BehaviorSource::Request
        } else {
            BehaviorSource::Policy
        },
        request_ignored: kept.request_ignored,
        scroll_depth: kept.scroll_depth,
        scroll_reached: kept.scroll_reached,
        scroll_ended_by: kept
            .scroll_ended_by
            .as_deref()
            .and_then(ScrollEndField::named),
        patience_ms: kept.patience_ms,
        dwell_ms: kept.dwell_ms,
        referer_strategy: kept.referer_strategy.into(),
        moves: kept.moves,
        notches: kept.notches,
        seed: seed.filter(|_| viewer.admin).map(|seed| seed.to_string()),
    }
}

/// The browser's log a result keeps, as the API shows it.
fn console_of(kept: &[ConsoleEntry]) -> Vec<ConsoleView> {
    kept.iter()
        .map(|entry| ConsoleView {
            level: entry.level.clone(),
            source: entry.source.clone(),
            text: entry.text.clone(),
            url: entry.url.clone(),
        })
        .collect()
}

/// What a result says of the visit besides the page: the same for the
/// answer of a fetch and for the stored result.
struct VisitViews {
    pacing: Option<PacingView>,
    behavior: Option<BehaviorView>,
    console: Option<Vec<ConsoleView>>,
    side_effects: Vec<String>,
}

fn visit_of(
    kept: &Visit,
    attempts: &[Attempt],
    include: &Included,
    viewer: &Viewer<'_>,
) -> VisitViews {
    // The behavior on a page was drawn from the seed of its attempt, the
    // last one. An answer from the cache has no attempt of its own.
    let seed = attempts.last().and_then(|attempt| attempt.seed);
    VisitViews {
        pacing: kept.pacing.as_ref().map(pacing_of),
        behavior: kept
            .behavior
            .as_ref()
            .map(|kept| behavior_of(kept, seed, viewer)),
        console: kept
            .console
            .as_deref()
            .filter(|_| include.has(Include::Console))
            .map(console_of),
        side_effects: kept.side_effects.clone(),
    }
}

/// The result of a fetch the pipeline just made.
pub(crate) fn from_report(
    report: &FetchReport,
    level: Level,
    include: &Included,
    session: Option<&str>,
    viewer: &Viewer<'_>,
) -> FetchResult {
    let last = report.last.as_ref();
    let page = last.and_then(|last| last.page.as_deref());
    let fetched = last.and_then(|last| last.fetched.as_deref());
    // Nothing read from a hop the guard refused is returned (spec §19).
    let readable = report.outcome != Outcome::SsrfDenied;
    let mut bodies = page
        .filter(|_| readable)
        .map(bodies_of_page)
        .unwrap_or_default();
    if let Some(fetched) = fetched.filter(|_| readable) {
        bodies.raw_body = String::from_utf8(fetched.body.to_vec()).ok();
    }
    let wants_raw = include.has(Include::Raw) || (include.has(Include::Dom) && fetched.is_some());
    let visit = visit_of(&report.visit, &report.attempts, include, viewer);
    FetchResult {
        id: report.request,
        state: ResultState::Finished,
        outcome: report.outcome.into(),
        reason: report.reason.clone(),
        retry_at: report.retry_at,
        level: Some(u64::from(level.number())),
        status: last.and_then(|last| last.status),
        final_url: last.and_then(|last| last.final_url.clone()),
        redirects: last.map_or_else(Vec::new, |last| last.redirects.clone()),
        headers: include
            .has(Include::Headers)
            .then(|| headers_of(&report.headers)),
        dom_html: bodies.dom_html.filter(|_| include.has(Include::Dom)),
        raw_body: bodies.raw_body.filter(|_| wants_raw),
        extract: extract_of(report.extract.as_ref()).filter(|_| include.has(Include::Extract)),
        xhr: xhr_of(report.xhr.as_ref()),
        console: visit.console,
        artifacts: artifacts_of(&report.artifacts),
        side_effects: visit.side_effects,
        timing: last.map(|last| last.timing.into()).unwrap_or_default(),
        persona: report
            .persona
            .as_ref()
            .map(|summary| viewer.persona(summary)),
        attempts: report
            .attempts
            .iter()
            .map(|attempt| viewer.attempt(attempt))
            .collect(),
        block: report.block.as_ref().map(BlockView::from),
        robots: report.robots.as_ref().map(RobotsView::from),
        pacing: visit.pacing,
        behavior: visit.behavior,
        session_id: session.map(str::to_owned),
        cached: false,
        cached_at: None,
        created_at: crate::stored(report.finished_at),
    }
}

/// The result of a fetch as it was stored. `bodies` are the stored
/// document and body, read by the caller when the request asked for them.
pub(crate) fn from_stored(
    (record, level): (&FetchRecord, Option<Level>),
    attempts: &[Attempt],
    bodies: Bodies,
    include: &Included,
    session: Option<&str>,
    viewer: &Viewer<'_>,
) -> FetchResult {
    let last = attempts.last();
    let on_http = last.is_some_and(|attempt| attempt.render == Render::Http);
    let wants_raw = include.has(Include::Raw) || (include.has(Include::Dom) && on_http);
    let visit = visit_of(&record.visit, attempts, include, viewer);
    FetchResult {
        id: record.request_id,
        state: ResultState::Finished,
        outcome: record.outcome.into(),
        reason: record.reason.clone(),
        retry_at: None,
        level: level.map(|level| u64::from(level.number())),
        status: record.status,
        final_url: record.final_url.clone(),
        redirects: record.redirects.clone(),
        headers: include
            .has(Include::Headers)
            .then(|| headers_of(&record.headers)),
        dom_html: bodies.dom_html.filter(|_| include.has(Include::Dom)),
        raw_body: bodies.raw_body.filter(|_| wants_raw),
        extract: extract_of(record.extract.as_ref()).filter(|_| include.has(Include::Extract)),
        xhr: xhr_of(record.xhr.as_ref()),
        console: visit.console,
        artifacts: artifacts_of(&record.artifacts),
        side_effects: visit.side_effects,
        timing: last
            .map(|attempt| attempt.timing.into())
            .unwrap_or_default(),
        persona: record
            .persona
            .as_ref()
            .map(|summary| viewer.persona(summary)),
        attempts: attempts
            .iter()
            .map(|attempt| viewer.attempt(attempt))
            .collect(),
        block: record.block.as_ref().map(BlockView::from),
        robots: record.robots.as_ref().map(RobotsView::from),
        pacing: visit.pacing,
        behavior: visit.behavior,
        session_id: session.map(str::to_owned),
        cached: record.cached,
        cached_at: record.cached_at,
        created_at: record.created_at,
    }
}

#[cfg(test)]
mod tests {
    use kage_core::domain::{ArtifactKind, ArtifactRef};
    use kage_core::ids::{ApiKeyId, ArtifactId, PersonaId, RequestId};
    use serde_json::json;

    use super::{
        ARTIFACT_PREFIX, ExtractView, FetchAccepted, RobotsView, Viewer, artifact_handle,
        artifacts_of, extract_of, parse_artifact, xhr_of,
    };
    use crate::handle::Handles;

    #[test]
    fn an_artifact_id_is_written_with_a_prefix_and_read_with_or_without_it() {
        let id = ArtifactId::new();

        let handle = artifact_handle(id);

        assert!(handle.starts_with(ARTIFACT_PREFIX));
        assert_eq!(handle.len(), 4 + 32);
        assert_eq!(parse_artifact(&handle), Some(id));
        assert_eq!(parse_artifact(&id.to_string()), Some(id));
        assert_eq!(parse_artifact("art_nope"), None);
        assert_eq!(parse_artifact(""), None);
    }

    #[test]
    fn a_persona_is_a_handle_to_a_consumer_and_its_id_to_an_admin() {
        let handles = Handles::new(&[3; 32]);
        let key = ApiKeyId::new();
        let persona = PersonaId::new();
        let consumer = Viewer {
            key,
            admin: false,
            handles: &handles,
        };
        let admin = Viewer {
            key,
            admin: true,
            handles: &handles,
        };

        assert_eq!(consumer.name(persona), handles.of(key, persona));
        assert!(consumer.name(persona).starts_with("p_"));
        assert_eq!(admin.name(persona), persona.to_string());
    }

    #[test]
    fn artifacts_are_listed_by_kind_and_xhr_bodies_by_their_responses() {
        let (dom, shot, body) = (ArtifactId::new(), ArtifactId::new(), ArtifactId::new());
        let artifacts = [
            ArtifactRef {
                kind: ArtifactKind::DomHtml,
                id: dom,
            },
            ArtifactRef {
                kind: ArtifactKind::XhrBody,
                id: body,
            },
            ArtifactRef {
                kind: ArtifactKind::Screenshot,
                id: shot,
            },
        ];

        let by_kind = artifacts_of(&artifacts);

        assert_eq!(by_kind.len(), 2);
        assert_eq!(by_kind["dom_html"], artifact_handle(dom));
        assert_eq!(by_kind["screenshot"], artifact_handle(shot));

        let stored = json!({
            "responses": [
                { "url": "https://example.org/api/a", "status": 200, "body": "{}" },
                { "url": "https://example.org/api/b", "status": 200, "body_artifact": body },
            ],
            "dropped": 3,
        });
        let view = xhr_of(Some(&stored)).unwrap();
        assert_eq!(view.dropped, 3);
        assert_eq!(view.responses[0]["body"], "{}");
        assert_eq!(view.responses[1]["body_artifact"], artifact_handle(body));
        assert!(xhr_of(None).is_none());
    }

    #[test]
    fn what_was_extracted_is_returned_as_it_was_stored() {
        // Everything the extraction writes has a place in the view, and
        // nothing is lost on the way: a field added to one and not to the
        // other fails here.
        let document = kage_extract::Document::parse(
            "<html lang=\"en\"><head><title>A page</title>\
             <meta name=\"description\" content=\"About it.\">\
             <meta property=\"og:title\" content=\"A page, shared\">\
             <meta name=\"twitter:card\" content=\"summary\">\
             <link rel=\"canonical\" href=\"/a\">\
             <script type=\"application/ld+json\">{\"@type\":\"Article\",\
             \"datePublished\":\"2026-10-01T08:00:00+02:00\"}</script></head>\
             <body><article><h1 id=\"top\">A page</h1><p>Some words about it, and \
             <a rel=\"nofollow\" href=\"https://other.example/b\">a link</a>.</p>\
             </article></body></html>",
            &url::Url::parse("https://example.org/a?x=1").unwrap(),
        );
        let extract = kage_extract::extract(&document, &kage_extract::Options::default());
        let stored = serde_json::to_value(&extract).unwrap();

        let view: ExtractView = extract_of(Some(&stored)).unwrap();

        assert_eq!(serde_json::to_value(&view).unwrap(), stored);
        assert_eq!(view.title.as_deref(), Some("A page"));
        assert_eq!(view.canonical.as_deref(), Some("https://example.org/a"));
        assert_eq!(view.published_at.as_deref(), Some("2026-10-01T06:00:00Z"));
        assert_eq!(view.opengraph["og:title"], ["A page, shared"]);
        assert_eq!(view.headings[0].id.as_deref(), Some("top"));
        let links = view.links.unwrap();
        assert_eq!(links[0].url, "https://other.example/b");
        assert!(!links[0].same_site);
        assert_eq!(links[0].rel, ["nofollow"]);
        assert!(view.thin_content);
        // Nothing stored, or something this release cannot read: null.
        assert!(extract_of(None).is_none());
        assert!(extract_of(Some(&json!({ "title": "x" }))).is_none());
    }

    #[test]
    fn a_robots_verdict_is_returned_as_it_was_reported() {
        let verdict = kage_core::domain::RobotsVerdict {
            fetched: true,
            allowed: false,
            matched_rule: Some("Disallow: /private".to_owned()),
            crawl_delay: Some(2.5),
        };
        assert_eq!(
            serde_json::to_value(RobotsView::from(&verdict)).unwrap(),
            json!({
                "fetched": true,
                "allowed": false,
                "matched_rule": "Disallow: /private",
                "crawl_delay": 2.5,
            })
        );
    }

    #[test]
    fn a_fetch_under_way_says_where_its_result_will_be() {
        let id = RequestId::new();

        let accepted = serde_json::to_value(FetchAccepted::running(id)).unwrap();

        assert_eq!(
            accepted,
            json!({
                "id": id,
                "state": "running",
                "job_id": null,
                "result": format!("/v1/results/{id}"),
            })
        );
    }
}
