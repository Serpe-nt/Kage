//! The request of `POST /v1/fetch` (spec §16.4) and what makes one
//! acceptable.
//!
//! A request is refused, never quietly changed. Every request names its
//! `level` (spec §25), and the level decides what else it may name (the
//! table of [`plan`]): a method, a body and caller headers are the http
//! path's (levels 0 and 1) and refused at 2 and 3, where a visit is a
//! plain navigation that would be made without them; a `fetch()` or an
//! `XMLHttpRequest` is level 1's; what only a browser produces (a DOM, a
//! screenshot, a wait) is refused at 0 and 1; `behavior` is level 3's.
//! What depends on the site (its floor, what its policy says about
//! `robots.txt` and the response cache) is settled in
//! [`terms`](super::terms).
//!
//! # What a caller may shorten
//!
//! At level 3 a visit behaves like a person by default: it scrolls to 0.6
//! to 0.9 of the page with pauses to read and dwells as that persona
//! dwells, about 35 s a page at the median. `behavior` shortens that inside
//! what the domain's policy allows: `scroll_depth: 0.3, dwell_ms: 3000` is
//! about 11 s a page, `scroll_depth: 0, dwell_ms: 3000` about 5 s.
//! `dwell_ms` is a median, not a constant: the dwell of every attempt is
//! drawn around it and never falls below the policy's `min_dwell` (2 s
//! unless the policy says otherwise). A page nobody touches is level 2,
//! which says so. `timeout_ms` of a browser visit (2 and 3) is at least
//! 15 s.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bytes::Bytes;
use kage_browser::contract::{
    CaptureOptions, PaperFormat, PdfOptions, ScreenshotFormat, ScreenshotOptions, Wait, WaitUntil,
    XhrCapture,
};
use kage_core::config::MAX_ATTEMPT_TIMEOUT;
use kage_core::domain::{CountryCode, Engine, Level, SessionId};
use kage_core::pacing::{PacingProfile, PacingProfiles};
use kage_core::policy::{MAX_RETRIES, PacingRequest, RefererStrategy};
use kage_core::schedule::queue::Priority;
use kage_core::ssrf::SsrfDenial;
use kage_http::headers::{
    CallerHeaders, HeaderError, denied_reason, placed_in_scripted, plain_denied_reason,
};
use kage_http::{HttpError, RequestKind};
use kage_scheduler::{BehaviorWish, FetchOrder, Keep, RobotsWish};
use serde::{Deserialize, Serialize};
use url::Url;
use utoipa::ToSchema;

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::schema::FreeForm;

/// The headers a caller may give at level 0 that carry its own
/// credential: sent to the request's origin only, never stored, never
/// logged.
pub(crate) const CREDENTIAL_HEADERS: [&str; 2] = ["cookie", "authorization"];

/// What a stored request holds in place of a credential's value.
pub(crate) const REDACTED: &str = "[redacted]";

/// The request as it is kept: everything as given, but the value of a
/// caller's credential header ([`CREDENTIAL_HEADERS`]), which Kage never
/// holds.
///
/// # Errors
///
/// Fails when the request cannot be written as JSON.
pub(crate) fn kept_spec(request: &FetchRequest) -> Result<serde_json::Value, serde_json::Error> {
    let mut kept = request.clone();
    for (name, value) in &mut kept.headers {
        if is_credential(name) {
            REDACTED.clone_into(value);
        }
    }
    serde_json::to_value(&kept)
}

/// Whether a header is a caller's credential.
pub(crate) fn is_credential(name: &str) -> bool {
    CREDENTIAL_HEADERS
        .iter()
        .any(|credential| name.eq_ignore_ascii_case(credential))
}

/// Whether a kept request lost a credential it was made with: it cannot
/// be made again as it was asked.
pub(crate) fn lost_credential(request: &FetchRequest) -> bool {
    request
        .headers
        .iter()
        .any(|(name, value)| is_credential(name) && value == REDACTED)
}

/// The longest URL a request may name, in bytes (spec §16.4).
pub(crate) const MAX_URL_BYTES: usize = 8 * 1024;

/// The most headers a request may carry (spec §16.4).
pub(crate) const MAX_HEADERS: usize = 32;

/// The shortest `timeout_ms` of an attempt that may run in a browser: the
/// page has to load, an interstitial has to be given its time, and the
/// persona has to stay at least as long as the policy's shortest dwell.
/// A shorter budget would cut the behavior down to a bare load.
pub(crate) const MIN_BROWSER_TIMEOUT: Duration = Duration::from_secs(15);

/// The shortest `timeout_ms` on the http path.
pub(crate) const MIN_HTTP_TIMEOUT: Duration = Duration::from_secs(1);

/// The longest median a request may ask the dwell to have.
pub(crate) const MAX_DWELL: Duration = Duration::from_secs(60);

/// The largest body the contract lets a browser keep.
pub(crate) const MAX_BODY_BYTES: u64 = 100_000_000;

/// The level of a request as it names it: required, 0 to 3.
///
/// # Errors
///
/// `level_required` without one; `validation_failed` for a number that
/// is no level.
pub(crate) fn level_of(level: Option<u64>, field: &str) -> ApiResult<Level> {
    let Some(number) = level else {
        return Err(ApiError::new(
            ErrorCode::LevelRequired,
            format!(
                "{field} is required: 0 (a plain HTTP client that says it is Kage), 1 (a \
                 persona's http client), 2 (a persona's browser that loads the page and \
                 leaves) or 3 (a persona's browser as a person). Nothing is decided for the \
                 caller"
            ),
        )
        .with("field", field));
    };
    Level::from_number(number).ok_or_else(|| ApiError::field(field, "the level is 0, 1, 2 or 3"))
}

/// What a request is on the http path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestKindField {
    /// A navigation: a typed address, or a followed link when a `Referer`
    /// is given. The default, and never guessed otherwise: a JSON API URL
    /// fetched as a navigation is what a developer does by hand. To read
    /// the API behind a site, render the page that calls it with
    /// `include: ["xhr"]`.
    #[default]
    Navigation,
    /// A `fetch()` call of the page named by the `Referer` header.
    Fetch,
    /// An `XMLHttpRequest` of the page named by the `Referer` header.
    Xhr,
}

impl RequestKindField {
    /// The name as a request writes it.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Navigation => "navigation",
            Self::Fetch => "fetch",
            Self::Xhr => "xhr",
        }
    }
}

impl From<RequestKindField> for RequestKind {
    fn from(kind: RequestKindField) -> Self {
        match kind {
            RequestKindField::Navigation => Self::Document,
            RequestKindField::Fetch => Self::Fetch,
            RequestKindField::Xhr => Self::Xhr,
        }
    }
}

/// A part of the result a request asks for.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Include {
    /// The document as the browser had it after the page settled.
    Dom,
    /// The body of the main response as the server sent it, decoded.
    Raw,
    /// The headers of the main response.
    Headers,
    /// What the page says: its metadata, its links, its headings and its
    /// main content as text and as markdown, read from the document the
    /// visit captured (`extract` in the result).
    Extract,
    /// The responses the page's scripts fetched.
    Xhr,
    /// A screenshot.
    Screenshot,
    /// A PDF.
    Pdf,
    /// The browser's log.
    Console,
}

impl Include {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Dom => "dom",
            Self::Raw => "raw",
            Self::Headers => "headers",
            Self::Extract => "extract",
            Self::Xhr => "xhr",
            Self::Screenshot => "screenshot",
            Self::Pdf => "pdf",
            Self::Console => "console",
        }
    }

    /// Whether only a browser produces it.
    const fn needs_browser(self) -> bool {
        matches!(
            self,
            Self::Dom | Self::Xhr | Self::Screenshot | Self::Pdf | Self::Console
        )
    }
}

/// A browser engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EngineField {
    /// Chrome.
    Chromium,
    /// Firefox.
    Firefox,
}

impl From<EngineField> for Engine {
    fn from(engine: EngineField) -> Self {
        match engine {
            EngineField::Chromium => Self::Chromium,
            EngineField::Firefox => Self::Firefox,
        }
    }
}

impl From<Engine> for EngineField {
    fn from(engine: Engine) -> Self {
        match engine {
            Engine::Chromium => Self::Chromium,
            Engine::Firefox => Self::Firefox,
        }
    }
}

/// Which personas a request would have.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersonaHints {
    /// Countries the request would rather be fetched from, as two-letter
    /// codes. A preference, not a filter: the result says where the
    /// persona was. Against a session's persona it is a condition.
    #[serde(default)]
    #[schema(example = json!(["DE", "AT"]))]
    pub(crate) countries: Vec<String>,
    /// The engines the request accepts; empty for any the policy allows.
    #[serde(default)]
    pub(crate) engines: Vec<EngineField>,
}

/// The pacing a request asks for (decision D9). Give `profile` or `custom`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PacingField {
    /// The name of a profile of `policies/pacing-profiles.yaml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "human")]
    pub(crate) profile: Option<String>,
    /// A profile of the request's own, in the format of that file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<FreeForm>)]
    pub(crate) custom: Option<PacingProfile>,
}

/// The lifecycle point a browser waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WaitUntilField {
    /// The response of the navigation was committed.
    Commit,
    /// The document was parsed.
    DomContentLoaded,
    /// The load event.
    #[default]
    Load,
    /// No request for half a second.
    NetworkIdle,
}

/// What a browser waits for before the page is captured.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitField {
    /// The lifecycle point.
    #[serde(default)]
    pub(crate) until: WaitUntilField,
    /// A CSS selector that must match an element afterwards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 1000)]
    pub(crate) selector: Option<String>,
    /// Milliseconds to wait after everything else.
    #[serde(default)]
    #[schema(maximum = 60000)]
    pub(crate) extra_ms: u64,
}

/// How a page is arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RefererStrategyField {
    /// A typed address or a bookmark.
    Direct,
    /// The same on the wire as `direct`.
    None,
    /// A click on a search result; a typed address while no search
    /// template is configured.
    SearchEngine,
}

impl From<RefererStrategyField> for RefererStrategy {
    fn from(strategy: RefererStrategyField) -> Self {
        match strategy {
            RefererStrategyField::Direct => Self::Direct,
            RefererStrategyField::None => Self::None,
            RefererStrategyField::SearchEngine => Self::SearchEngine,
        }
    }
}

impl From<RefererStrategy> for RefererStrategyField {
    fn from(strategy: RefererStrategy) -> Self {
        match strategy {
            RefererStrategy::Direct => Self::Direct,
            RefererStrategy::None => Self::None,
            RefererStrategy::SearchEngine => Self::SearchEngine,
        }
    }
}

/// What the persona does on a browser page, where the request wants less
/// than the domain policy's default. What is left out is the policy's.
/// Ignored, and reported as ignored, when the policy sets
/// `behavior_locked`.
///
/// What it costs: with the policy's default a page takes about 35 s
/// (median), which is about 100 pages an hour per persona. `scroll_depth:
/// 0.3, dwell_ms: 3000` is about 11 s a page and still looks like someone
/// skimming a listing; `scroll_depth: 0, dwell_ms: 3000` is about 5 s.
/// There is no bare load: the pointer always moves, and the dwell never
/// falls below the policy's `min_dwell` (2 s unless a domain says
/// otherwise).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BehaviorField {
    /// How far down the page to scroll, from 0 to 1; 0 for no scrolling.
    /// Default: drawn between 0.6 and 0.9. Scrolling with its reading
    /// pauses is most of a page's time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 0, maximum = 1, example = 0.3)]
    pub(crate) scroll_depth: Option<f64>,
    /// The median of the time spent on the page after it loaded, in
    /// milliseconds. Each attempt's dwell is drawn around it and is never
    /// shorter than the policy's `min_dwell`. Default: the persona's own
    /// human dwell (a median of about 6 s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(maximum = 60000, example = 3000)]
    pub(crate) dwell_ms: Option<u64>,
    /// How the page is arrived at. Default: the policy's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) referer_strategy: Option<RefererStrategyField>,
}

/// An image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScreenshotFormatField {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// WebP. A firefox persona's browser encodes PNG and JPEG only: its
    /// screenshot is a PNG, and the artifact's `Content-Type` says so.
    #[default]
    Webp,
}

/// How a screenshot is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScreenshotField {
    /// False, the default: the visible viewport, which no page can
    /// observe. True: the whole document, which fires `resize` in the page
    /// on chromium; the result names that side effect.
    #[serde(default)]
    pub(crate) full_page: bool,
    /// The image format; WebP unless said. A firefox persona's browser
    /// encodes PNG and JPEG only, so its WebP is a PNG: the artifact's
    /// `Content-Type` says what it is.
    #[serde(default)]
    pub(crate) format: ScreenshotFormatField,
    /// Quality for `jpeg` and `webp`, 1 to 100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 100)]
    pub(crate) quality: Option<u8>,
}

/// A paper size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PaperFormatField {
    /// A4.
    #[default]
    A4,
    /// US Letter.
    Letter,
}

/// How a PDF is printed. Printing fires `beforeprint` and `afterprint` in
/// the page, as printing by hand does; the result names both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PdfField {
    /// The paper size.
    #[serde(default)]
    pub(crate) format: PaperFormatField,
    /// Landscape orientation.
    #[serde(default)]
    pub(crate) landscape: bool,
    /// Print background graphics.
    #[serde(default = "yes")]
    pub(crate) print_background: bool,
}

const fn yes() -> bool {
    true
}

impl Default for PdfField {
    fn default() -> Self {
        Self {
            format: PaperFormatField::default(),
            landscape: false,
            print_background: true,
        }
    }
}

/// Which responses of the page's scripts are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct XhrField {
    /// Media types to keep, for example `application/json`.
    #[serde(default = "json_only")]
    #[schema(max_items = 16)]
    pub(crate) content_types: Vec<String>,
    /// The largest body kept per response, in bytes; a longer one is cut.
    #[serde(default = "two_megabytes")]
    #[schema(minimum = 1, maximum = 50_000_000)]
    pub(crate) max_bytes: u64,
    /// The most responses kept per page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 500)]
    pub(crate) max_responses: Option<u64>,
}

fn json_only() -> Vec<String> {
    vec!["application/json".to_owned()]
}

const fn two_megabytes() -> u64 {
    2_000_000
}

impl Default for XhrField {
    fn default() -> Self {
        Self {
            content_types: json_only(),
            max_bytes: two_megabytes(),
            max_responses: None,
        }
    }
}

/// What to do about the site's `robots.txt`. Whether the file is asked
/// for at all is the site's policy (`ignore` unless the operator says
/// otherwise): a reader never asks for it, so asking marks an address. A
/// request may ask for more care than the policy takes and never for
/// less, and never makes Kage ask a site its policy leaves alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RobotsField {
    /// What the domain's policy says.
    #[default]
    Policy,
    /// Report the verdict, fetch either way. Refused with
    /// `robots_enforced` where the policy enforces; where the policy
    /// ignores, nothing is asked and the verdict says `fetched: false`.
    Report,
    /// Do not fetch what `robots.txt` disallows, and wait for the file
    /// when it is not known yet. Refused with `robots_ignored` where the
    /// policy never asks for the file.
    Enforce,
    /// Do not look at `robots.txt`. Refused with `robots_enforced` where
    /// the policy enforces.
    Ignore,
}

impl From<RobotsField> for RobotsWish {
    fn from(robots: RobotsField) -> Self {
        match robots {
            RobotsField::Policy => Self::Policy,
            RobotsField::Report => Self::Report,
            RobotsField::Enforce => Self::Enforce,
            RobotsField::Ignore => Self::Ignore,
        }
    }
}

/// What of a page is extracted; with `include: ["extract"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtractField {
    /// Whether the main content is also written as markdown.
    #[serde(default = "yes")]
    pub(crate) markdown: bool,
    /// Whether the links of the page are listed.
    #[serde(default = "yes")]
    pub(crate) links: bool,
}

impl Default for ExtractField {
    fn default() -> Self {
        Self {
            markdown: true,
            links: true,
        }
    }
}

/// One fetch (spec §16.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({
    "url": "https://example.org/listing?page=2",
    "level": 3,
    "include": ["dom", "headers", "xhr"],
    "persona": { "countries": ["DE", "AT"], "engines": ["chromium"] },
    "behavior": { "scroll_depth": 0.3, "dwell_ms": 3000 },
    "wait": { "until": "network_idle" },
    "timeout_ms": 60000
}))]
pub(crate) struct FetchRequest {
    /// The page: an `http` or `https` URL of at most 8192 bytes. Hosts
    /// that name no public site are refused (`ssrf_denied`).
    #[schema(max_length = 8192, example = "https://example.org/listing?page=2")]
    pub(crate) url: String,
    /// `GET`, `HEAD` or `POST`. Anything but `GET` needs level 0 or 1.
    #[serde(default = "get")]
    #[schema(example = "GET")]
    pub(crate) method: String,
    /// Headers of the request's own; at most 32, and only at level 0 or 1.
    /// A browser sends its own headers and cannot be given others without a
    /// page noticing. At level 1 headers that are the persona's identity or
    /// the transport's are refused by name (`User-Agent`,
    /// `Accept-Language`, `Accept-Encoding`, `Cookie`, `Origin`, `Sec-*`,
    /// `Proxy-*`, `Host` and others), and a `Referer` names the page the
    /// request comes from. At level 0 they are the caller's request as
    /// given, `User-Agent`, `Cookie` and `Authorization` included: only the
    /// transport's, `Proxy-*`, the forwarding family and `Accept-Encoding`
    /// are refused. A caller's `Cookie` and `Authorization` go to the
    /// request's origin only, never across a redirect to another, and are
    /// neither stored nor logged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) headers: BTreeMap<String, String>,
    /// The body of a `POST`, as text; needs level 0 or 1 and a
    /// `Content-Type` header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) body: Option<String>,
    /// The camouflage level (spec §25), required: 0, a plain HTTP client
    /// that says it is Kage (`User-Agent: kage/<version>` unless the
    /// request names its own), with no persona and no cookie jar; 1, a
    /// persona's http client, a browser's wire fingerprint with no page
    /// behaviour (refused with `level_unavailable` while no browser
    /// version of this installation has a verified profile); 2, a
    /// persona's browser that loads the page, sends no input and leaves; 3,
    /// a persona's browser as a person. Below 3 Kage is not pretending to
    /// be a person. A site's policy may set a floor (`level_below_policy`);
    /// a request is never raised and never escalated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(required = true, value_type = u8, minimum = 0, maximum = 3, example = 3)]
    pub(crate) level: Option<u64>,
    /// What the request is on the http path; anything but `navigation`
    /// needs level 1 and a `Referer` header naming the page that makes the
    /// request.
    #[serde(default)]
    pub(crate) request_kind: RequestKindField,
    /// The parts of the result to return and to keep. Default: `dom` and
    /// `headers` at level 2 and 3, `raw` and `headers` at 0 and 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) include: Option<Vec<Include>>,
    /// An open session of this key: the request is made by the session's
    /// persona and by no other. Needs the `sessions` scope, and a level
    /// with a persona (`session_needs_persona` at 0). A fetch never opens a
    /// session; `POST /v1/sessions` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(
        max_length = 64,
        pattern = "^[A-Za-z0-9_.-]{1,64}$",
        example = "proj-a-42"
    )]
    pub(crate) session_id: Option<String>,
    /// Which personas the request would have; not at level 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) persona: Option<PersonaHints>,
    /// The pacing; ignored, and reported as ignored, when the domain's
    /// policy locks pacing. In a browser the gap is a floor under the time
    /// on the page, so a faster profile shortens nothing there. Not at
    /// level 0, which the site's own limits pace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pacing: Option<PacingField>,
    /// What a browser waits for; at level 2 and 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) wait: Option<WaitField>,
    /// What the persona does on the page, where less than the default is
    /// wanted; at level 3 only (a level-2 visit does nothing on the page).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) behavior: Option<BehaviorField>,
    /// How the screenshot is taken; with `include: ["screenshot"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) screenshot: Option<ScreenshotField>,
    /// How the PDF is printed; with `include: ["pdf"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pdf: Option<PdfField>,
    /// The longest one attempt may take, in milliseconds: at least 15000
    /// in a browser (level 2 and 3), at least 1000 at 0 and 1, at most
    /// 300000. Default: 60000 in a browser, 30000 on the http path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1000, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
    /// Retries after the first attempt, each by another persona. Default:
    /// the policy's (2). At level 0 only a failure that is not the site's
    /// answer (the network, a timeout, a server error) is tried again, from
    /// the same exit; a block is the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(maximum = 10)]
    pub(crate) max_retries: Option<u32>,
    /// For how many seconds the result may be served from the response
    /// cache: a later request that asks for the same is answered with it,
    /// without a visit, while it is no older than that request's own
    /// `cache_ttl_s`. 0 is never. Left out, the site's policy decides
    /// (never, unless the operator set `cache.ttl`). At most the site's
    /// `cache.max_ttl` (a day unless configured): more is refused with
    /// `cache_ttl_too_long`. Not in a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 600)]
    pub(crate) cache_ttl_s: Option<u64>,
    /// What to do about the site's `robots.txt`.
    #[serde(default)]
    pub(crate) robots: RobotsField,
    /// What is extracted; with `include: ["extract"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) extract: Option<ExtractField>,
    /// Which responses of the page's scripts are kept; with
    /// `include: ["xhr"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) xhr: Option<XhrField>,
    /// The largest body and document kept, in bytes; a longer one is cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 100_000_000)]
    pub(crate) max_body_bytes: Option<u64>,
}

fn get() -> String {
    "GET".to_owned()
}

/// The parts of the result a request asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Included(BTreeSet<Include>);

impl Included {
    pub(crate) fn has(&self, part: Include) -> bool {
        self.0.contains(&part)
    }

    /// The names of the parts, in the order of their names.
    pub(crate) fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = self.0.iter().map(|part| part.as_str()).collect();
        names.sort_unstable();
        names
    }

    /// What a request that was accepted earlier asked for; `None` when it
    /// would not be accepted as it stands (a 1.x request names no level).
    pub(crate) fn of(request: &FetchRequest) -> Option<Self> {
        let level = level_of(request.level, "level").ok()?;
        included(request, level).ok()
    }

    /// Every part there is: for a stored request that named none.
    pub(crate) fn everything() -> Self {
        Self(BTreeSet::from([
            Include::Dom,
            Include::Raw,
            Include::Headers,
            Include::Extract,
            Include::Xhr,
            Include::Screenshot,
            Include::Pdf,
            Include::Console,
        ]))
    }
}

/// A request that passed every check that needs no persona.
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    /// The order for the pipeline. Its `pin`, `api_key` and `spec` are the
    /// handler's to set.
    pub(crate) order: FetchOrder,
    /// What the result returns.
    pub(crate) include: Included,
    /// The session the request names.
    pub(crate) session: Option<SessionId>,
    /// The countries the request would rather be fetched from.
    pub(crate) countries: Vec<CountryCode>,
    /// The engines the request accepts.
    pub(crate) engines: Vec<Engine>,
    /// The level.
    pub(crate) level: Level,
}

/// What the guard's refusal of a URL is answered with.
pub(crate) fn url_refused(denial: &SsrfDenial) -> ApiError {
    let code = match denial {
        SsrfDenial::InvalidUrl(_)
        | SsrfDenial::Scheme(_)
        | SsrfDenial::NoHost
        | SsrfDenial::Credentials
        | SsrfDenial::Port
        | SsrfDenial::TooManyRedirects(_) => ErrorCode::InvalidUrl,
        SsrfDenial::IpLiteral(_)
        | SsrfDenial::Name { .. }
        | SsrfDenial::Address { .. }
        | SsrfDenial::Unresolved(_) => ErrorCode::SsrfDenied,
    };
    ApiError::new(code, denial.to_string()).with("field", "url")
}

/// What a refused header is answered with.
fn header_refused(error: &HeaderError) -> ApiError {
    let named =
        |code: ErrorCode, name: &str| ApiError::new(code, error.to_string()).with("header", name);
    match error {
        HeaderError::Denied { name, .. } => named(ErrorCode::HeaderDenied, name),
        HeaderError::Unplaced { name, .. } => named(ErrorCode::HeaderNotPlaceable, name),
        HeaderError::InvalidName(name)
        | HeaderError::InvalidValue { name }
        | HeaderError::Duplicate(name) => named(ErrorCode::HeaderInvalid, name),
        _ => ApiError::new(ErrorCode::HeaderInvalid, error.to_string()),
    }
}

fn parse_url(text: &str) -> ApiResult<Url> {
    if text.len() > MAX_URL_BYTES {
        return Err(ApiError::new(
            ErrorCode::InvalidUrl,
            format!("the URL is longer than {MAX_URL_BYTES} bytes"),
        )
        .with("field", "url"));
    }
    Url::parse(text).map_err(|error| {
        ApiError::new(
            ErrorCode::InvalidUrl,
            format!("the URL does not parse: {error}"),
        )
        .with("field", "url")
    })
}

fn parse_method(text: &str) -> ApiResult<http::Method> {
    match text.to_ascii_uppercase().as_str() {
        "GET" => Ok(http::Method::GET),
        "HEAD" => Ok(http::Method::HEAD),
        "POST" => Ok(http::Method::POST),
        _ => Err(ApiError::field("method", "the method is GET, HEAD or POST")),
    }
}

/// The headers of a request, checked against the denylist of its level
/// and, for a script's request, against what has a place in one.
fn check_headers(
    request: &FetchRequest,
    level: Level,
    kind: RequestKind,
    (target, method): (&Url, &http::Method),
) -> ApiResult<Vec<(String, String)>> {
    if request.headers.len() > MAX_HEADERS {
        return Err(ApiError::new(
            ErrorCode::HeaderInvalid,
            format!(
                "{} headers given, at most {MAX_HEADERS} are allowed",
                request.headers.len()
            ),
        ));
    }
    let pairs: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    // The refusal names the header and says why.
    let denied = if level == Level::Plain {
        plain_denied_reason
    } else {
        denied_reason
    };
    for (name, _) in &pairs {
        if let Some(reason) = denied(name) {
            return Err(header_refused(&HeaderError::Denied {
                name: name.clone(),
                reason,
            }));
        }
    }
    if level == Level::Plain {
        // The caller's request as given: its own Referer is a header like
        // any other.
        CallerHeaders::parse_plain(&pairs).map_err(|error| header_refused(&error))?;
        return Ok(pairs);
    }
    let caller = CallerHeaders::parse(&pairs).map_err(|error| header_refused(&error))?;
    let referer = match caller.referer() {
        None => None,
        Some(text) => Some(
            Url::parse(text)
                .ok()
                .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::HeaderInvalid,
                        "the Referer is not an http or https URL",
                    )
                    .with("header", "Referer")
                })?,
        ),
    };
    if kind.is_scripted() {
        let Some(page) = &referer else {
            return Err(ApiError::new(
                ErrorCode::RequestKindNeedsReferer,
                "a fetch() or XMLHttpRequest is made by a page: name it in the Referer header. \
                 Kage never invents the page",
            )
            .with("field", "headers"));
        };
        for (name, _) in &pairs {
            if !placed_in_scripted(name) {
                return Err(header_refused(&HeaderError::Unplaced {
                    name: name.clone(),
                    kind: if kind == RequestKind::Xhr {
                        "XMLHttpRequest"
                    } else {
                        "fetch()"
                    },
                }));
            }
        }
        if let Err(error) = kage_http::fetch::check_preflight(page, target, method, &caller) {
            let detail = error.to_string();
            return Err(match error {
                HttpError::PreflightRequired { .. } => {
                    ApiError::new(ErrorCode::PreflightRequired, detail)
                }
                _ => ApiError::new(ErrorCode::ValidationFailed, detail),
            });
        }
    }
    Ok(pairs)
}

/// The parts a request asks for: what it lists, else the default of its
/// level.
fn included(request: &FetchRequest, level: Level) -> ApiResult<Included> {
    let browser = level.render() == kage_core::domain::Render::Browser;
    let parts: BTreeSet<Include> = match &request.include {
        Some(parts) => parts.iter().copied().collect(),
        None if !browser => BTreeSet::from([Include::Raw, Include::Headers]),
        None => BTreeSet::from([Include::Dom, Include::Headers]),
    };
    if !browser && let Some(part) = parts.iter().find(|part| part.needs_browser()) {
        return Err(ApiError::new(
            ErrorCode::LevelIncludeConflict,
            format!(
                "include asks for {:?} and level {} has no browser: only a browser produces it \
                 (level 2 or 3)",
                part.as_str(),
                level.number()
            ),
        )
        .with("field", "include")
        .with("include", part.as_str()));
    }
    for (field, given, part) in [
        (
            "screenshot",
            request.screenshot.is_some(),
            Include::Screenshot,
        ),
        ("pdf", request.pdf.is_some(), Include::Pdf),
        ("xhr", request.xhr.is_some(), Include::Xhr),
        ("extract", request.extract.is_some(), Include::Extract),
    ] {
        if given && !parts.contains(&part) {
            return Err(ApiError::field(
                field,
                format!(
                    "{field} is given and include does not ask for {:?}",
                    part.as_str()
                ),
            ));
        }
    }
    Ok(Included(parts))
}

fn capture_of(request: &FetchRequest, include: &Included) -> ApiResult<CaptureOptions> {
    let screenshot = if include.has(Include::Screenshot) {
        let options = request.screenshot.unwrap_or_default();
        if options
            .quality
            .is_some_and(|quality| !(1..=100).contains(&quality))
        {
            return Err(ApiError::field(
                "screenshot.quality",
                "the quality is between 1 and 100",
            ));
        }
        Some(ScreenshotOptions {
            format: match options.format {
                ScreenshotFormatField::Png => ScreenshotFormat::Png,
                ScreenshotFormatField::Jpeg => ScreenshotFormat::Jpeg,
                ScreenshotFormatField::Webp => ScreenshotFormat::Webp,
            },
            full_page: options.full_page,
            quality: options.quality.map(u64::from),
        })
    } else {
        None
    };
    let pdf = include.has(Include::Pdf).then(|| {
        let options = request.pdf.unwrap_or_default();
        PdfOptions {
            format: Some(match options.format {
                PaperFormatField::A4 => PaperFormat::A4,
                PaperFormatField::Letter => PaperFormat::Letter,
            }),
            landscape: Some(options.landscape),
            print_background: Some(options.print_background),
        }
    });
    let xhr = if include.has(Include::Xhr) {
        let options = request.xhr.clone().unwrap_or_default();
        let types_ok = (1..=16).contains(&options.content_types.len())
            && options
                .content_types
                .iter()
                .all(|media| (1..=100).contains(&media.len()));
        if !types_ok {
            return Err(ApiError::field(
                "xhr.content_types",
                "1 to 16 media types of at most 100 characters each",
            ));
        }
        if !(1..=50_000_000).contains(&options.max_bytes) {
            return Err(ApiError::field("xhr.max_bytes", "between 1 and 50000000"));
        }
        if options
            .max_responses
            .is_some_and(|most| !(1..=500).contains(&most))
        {
            return Err(ApiError::field("xhr.max_responses", "between 1 and 500"));
        }
        Some(XhrCapture {
            content_types: options.content_types,
            max_bytes: options.max_bytes,
            max_responses: options.max_responses,
        })
    } else {
        None
    };
    Ok(CaptureOptions {
        console: include.has(Include::Console),
        // The extraction is read from the document the visit captured.
        dom: include.has(Include::Dom) || include.has(Include::Extract),
        links: None,
        pdf,
        raw: include.has(Include::Raw),
        screenshot,
        xhr,
    })
}

fn wait_of(request: &FetchRequest) -> ApiResult<Wait> {
    let wait = request.wait.clone().unwrap_or_default();
    if wait
        .selector
        .as_ref()
        .is_some_and(|selector| selector.is_empty() || selector.len() > 1000)
    {
        return Err(ApiError::field(
            "wait.selector",
            "a selector of 1 to 1000 characters",
        ));
    }
    if wait.extra_ms > 60_000 {
        return Err(ApiError::field("wait.extra_ms", "at most 60000"));
    }
    Ok(Wait {
        extra_ms: (wait.extra_ms > 0).then_some(wait.extra_ms),
        selector: wait.selector,
        until: match wait.until {
            WaitUntilField::Commit => WaitUntil::Commit,
            WaitUntilField::DomContentLoaded => WaitUntil::Domcontentloaded,
            WaitUntilField::Load => WaitUntil::Load,
            WaitUntilField::NetworkIdle => WaitUntil::NetworkIdle,
        },
    })
}

fn behavior_of(request: &FetchRequest) -> ApiResult<Option<BehaviorWish>> {
    let Some(behavior) = &request.behavior else {
        return Ok(None);
    };
    if behavior
        .scroll_depth
        .is_some_and(|depth| !(0.0..=1.0).contains(&depth))
    {
        return Err(ApiError::field(
            "behavior.scroll_depth",
            "between 0 (no scrolling) and 1 (the whole page)",
        ));
    }
    let longest = u64::try_from(MAX_DWELL.as_millis()).unwrap_or(u64::MAX);
    if behavior.dwell_ms.is_some_and(|dwell| dwell > longest) {
        return Err(ApiError::field(
            "behavior.dwell_ms",
            format!("at most {longest}; leave it out for the persona's own dwell"),
        ));
    }
    let wish = BehaviorWish {
        scroll_depth: behavior.scroll_depth,
        dwell: behavior.dwell_ms.map(Duration::from_millis),
        referer_strategy: behavior.referer_strategy.map(Into::into),
    };
    Ok((!wish.is_empty()).then_some(wish))
}

fn timeout_of(request: &FetchRequest, level: Level) -> ApiResult<Option<Duration>> {
    let Some(millis) = request.timeout_ms else {
        return Ok(None);
    };
    let timeout = Duration::from_millis(millis);
    if timeout > MAX_ATTEMPT_TIMEOUT {
        return Err(ApiError::field(
            "timeout_ms",
            format!("at most {}", MAX_ATTEMPT_TIMEOUT.as_millis()),
        ));
    }
    let (least, why) = if level.render() == kage_core::domain::Render::Http {
        (MIN_HTTP_TIMEOUT, "an attempt on the http path")
    } else {
        (
            MIN_BROWSER_TIMEOUT,
            "an attempt a browser may make: the page has to load and the persona has to stay on \
             it; a shorter budget would cut the visit down to a bare load",
        )
    };
    if timeout < least {
        return Err(ApiError::new(
            ErrorCode::TimeoutTooShort,
            format!("timeout_ms is at least {} for {why}", least.as_millis()),
        )
        .with("field", "timeout_ms")
        .with(
            "minimum_ms",
            u64::try_from(least.as_millis()).unwrap_or(u64::MAX),
        ));
    }
    Ok(Some(timeout))
}

fn pacing_of(
    request: &FetchRequest,
    profiles: &PacingProfiles,
) -> ApiResult<Option<PacingRequest>> {
    let Some(pacing) = &request.pacing else {
        return Ok(None);
    };
    let invalid =
        |detail: String| ApiError::new(ErrorCode::InvalidPacing, detail).with("field", "pacing");
    match (&pacing.profile, &pacing.custom) {
        (Some(_), Some(_)) => Err(invalid(
            "give a profile's name or a custom profile, not both".to_owned(),
        )),
        (None, None) => Ok(None),
        (Some(name), None) => {
            if profiles.get(name).is_none() {
                let known: Vec<&str> = profiles.names().collect();
                return Err(invalid(format!(
                    "there is no pacing profile {name:?}; defined are: {}",
                    known.join(", ")
                )));
            }
            Ok(Some(PacingRequest::Profile(name.clone())))
        }
        (None, Some(profile)) => {
            profile
                .validate(PacingProfiles::CUSTOM)
                .map_err(|error| invalid(error.to_string()))?;
            Ok(Some(PacingRequest::Custom(*profile)))
        }
    }
}

/// What a level accepts besides what every level does (spec §25):
///
/// | field | 0 | 1 | 2 | 3 |
/// |---|---|---|---|---|
/// | `method`, `body`, `headers` | yes (its own list) | yes | `level_needs_http` | `level_needs_http` |
/// | `request_kind` `fetch`, `xhr` | `level_kind_conflict` | yes | `level_kind_conflict` | `level_kind_conflict` |
/// | `session_id` | `session_needs_persona` | yes | yes | yes |
/// | `persona` | no | yes | yes | yes |
/// | `behavior` | no | no | no | yes |
/// | `wait` | no | no | yes | yes |
/// | `pacing` | no | yes | yes | yes |
/// | `include` `dom`, `xhr`, `screenshot`, `pdf`, `console` | `level_include_conflict` | `level_include_conflict` | yes | yes |
/// | `timeout_ms` at least | 1 s | 1 s | 15 s | 15 s |
///
/// "no" is `validation_failed` naming the field.
fn check_level(
    request: &FetchRequest,
    level: Level,
    kind: RequestKind,
    method: &http::Method,
) -> ApiResult<()> {
    let number = level.number();
    if kind.is_scripted() && level != Level::Emulated {
        return Err(ApiError::new(
            ErrorCode::LevelKindConflict,
            format!(
                "a fetch() or XMLHttpRequest is a browser's request emulated, which is level \
                 1's; this request is level {number}. A browser cannot be told to make one \
                 without running a script in the page, and a plain client is not a page"
            ),
        )
        .with("field", "request_kind"));
    }
    if level.render() == kage_core::domain::Render::Browser {
        let what = if *method != http::Method::GET {
            Some("method")
        } else if request.body.is_some() {
            Some("body")
        } else if !request.headers.is_empty() {
            Some("headers")
        } else {
            None
        };
        if let Some(field) = what {
            return Err(ApiError::new(
                ErrorCode::LevelNeedsHttp,
                format!(
                    "{field} is given at level {number}: a browser visit is a plain navigation \
                     and would be made without it; levels 0 and 1 send it"
                ),
            )
            .with("field", field));
        }
    }
    if level == Level::Plain && request.session_id.is_some() {
        return Err(ApiError::new(
            ErrorCode::SessionNeedsPersona,
            "a session is a persona's, and level 0 has no persona: name level 1, 2 or 3",
        )
        .with("field", "session_id"));
    }
    let refused: &[(&str, bool, &str)] = match level {
        Level::Plain => &[
            (
                "persona",
                request.persona.is_some(),
                "level 0 has no persona to choose",
            ),
            (
                "pacing",
                request.pacing.is_some(),
                "level 0 is paced by the site's own limits, not by a persona's",
            ),
            (
                "wait",
                request.wait.is_some(),
                "a wait is a browser's (level 2 or 3)",
            ),
            (
                "behavior",
                request.behavior.is_some(),
                "what a person does on a page is level 3's",
            ),
        ],
        Level::Emulated => &[
            (
                "wait",
                request.wait.is_some(),
                "a wait is a browser's (level 2 or 3)",
            ),
            (
                "behavior",
                request.behavior.is_some(),
                "what a person does on a page is level 3's",
            ),
        ],
        Level::Fast => &[(
            "behavior",
            request.behavior.is_some(),
            "a level-2 visit loads the page, does nothing on it and leaves after the policy's \
             dwell; what a person does on a page is level 3's",
        )],
        Level::Human => &[],
    };
    for (field, given, why) in refused {
        if *given {
            return Err(ApiError::field(
                field,
                format!("{field} is not taken at level {number}: {why}"),
            ));
        }
    }
    Ok(())
}

/// Checks a request and turns it into an order for the pipeline. Nothing
/// here needs a persona, a session or the network; the guard's lookup of
/// the URL's host, when it is on, and the site's floor are the handler's.
///
/// # Errors
///
/// The first thing about the request that is not acceptable.
// One request, checked top to bottom in the order of its fields.
#[allow(clippy::too_many_lines)]
pub(crate) fn plan(request: &FetchRequest, profiles: &PacingProfiles) -> ApiResult<Planned> {
    let level = level_of(request.level, "level")?;
    let url = parse_url(&request.url)?;
    let method = parse_method(&request.method)?;
    let kind = RequestKind::from(request.request_kind);
    check_level(request, level, kind, &method)?;

    let headers = check_headers(request, level, kind, (&url, &method))?;
    let body = match &request.body {
        None => None,
        Some(_) if method != http::Method::POST => {
            return Err(ApiError::field("body", "only a POST carries a body"));
        }
        Some(_)
            if !request
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("content-type")) =>
        {
            return Err(ApiError::field(
                "body",
                "a body needs a Content-Type header that says what it is",
            ));
        }
        Some(text) => Some(Bytes::from(text.clone())),
    };

    let include = included(request, level)?;
    let capture = capture_of(request, &include)?;
    let wait = wait_of(request)?;
    let behavior = behavior_of(request)?;
    let timeout = timeout_of(request, level)?;
    let pacing = pacing_of(request, profiles)?;

    if request
        .max_retries
        .is_some_and(|retries| retries > MAX_RETRIES)
    {
        return Err(ApiError::field(
            "max_retries",
            format!("at most {MAX_RETRIES}"),
        ));
    }
    if request
        .max_body_bytes
        .is_some_and(|bytes| !(1..=MAX_BODY_BYTES).contains(&bytes))
    {
        return Err(ApiError::field(
            "max_body_bytes",
            format!("between 1 and {MAX_BODY_BYTES}"),
        ));
    }

    let hints = request.persona.clone().unwrap_or_default();
    let mut countries = Vec::with_capacity(hints.countries.len());
    for country in &hints.countries {
        countries.push(country.parse::<CountryCode>().map_err(|_| {
            ApiError::field(
                "persona.countries",
                format!("{country:?} is not a two-letter country code"),
            )
        })?);
    }
    let engines: Vec<Engine> = hints.engines.iter().copied().map(Engine::from).collect();
    let session = request
        .session_id
        .as_deref()
        .map(SessionId::new)
        .transpose()
        .map_err(|error| {
            ApiError::new(ErrorCode::InvalidSessionId, error.to_string())
                .with("field", "session_id")
        })?;

    let keep = Keep {
        dom: include.has(Include::Dom),
        raw: include.has(Include::Raw),
        xhr: include.has(Include::Xhr),
        screenshot: include.has(Include::Screenshot),
        pdf: include.has(Include::Pdf),
    };
    let order = FetchOrder {
        method,
        headers,
        body,
        kind,
        level,
        engines: engines.clone(),
        prefer_countries: countries.clone(),
        pacing,
        timeout,
        max_retries: request.max_retries,
        capture,
        wait,
        behavior,
        keep,
        max_body_bytes: request.max_body_bytes,
        session: session.clone(),
        robots: request.robots.into(),
        // What counts as thin is the configuration's: `terms` sets it.
        extract: include.has(Include::Extract).then(|| {
            let asked = request.extract.unwrap_or_default();
            kage_extract::Options {
                markdown: asked.markdown,
                links: asked.links,
                ..kage_extract::Options::default()
            }
        }),
        ..FetchOrder::get(url, Priority::Sync)
    };
    Ok(Planned {
        order,
        include,
        session,
        countries,
        engines,
        level,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_browser::contract::{ScreenshotFormat, WaitUntil};
    use kage_core::domain::{Engine, Level};
    use kage_core::pacing::PacingProfiles;
    use kage_core::policy::{PacingRequest, RefererStrategy};
    use kage_core::ssrf::{DeniedRange, SsrfDenial};
    use kage_http::RequestKind;
    use kage_scheduler::RobotsWish;
    use serde_json::{Value, json};

    use super::{FetchRequest, Include, Planned, kept_spec, lost_credential, plan, url_refused};
    use crate::error::{ApiError, ErrorCode};

    const PROFILES: &str = "
human:
  gap_ms: { dist: lognormal, median: 4500, p95: 15000, min: 1500 }
  burst_pages: { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 60000, p95: 240000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
fast:
  gap_ms: { dist: uniform, min: 300, max: 1200 }
  burst_pages: { min: 50, max: 50 }
  burst_break_ms: { dist: uniform, min: 0, max: 0 }
  daily_cap_per_persona_per_domain: 5000
  domain_concurrency: 10
  domain_rpm: 300
";

    /// A request as it is planned, as written.
    fn plan_as_written(body: Value) -> Result<Planned, ApiError> {
        let request: FetchRequest = serde_json::from_value(body).unwrap();
        plan(&request, &PacingProfiles::from_yaml(PROFILES).unwrap())
    }

    /// A request at level 3 unless the body names another.
    fn planned(mut body: Value) -> Result<Planned, ApiError> {
        if body.get("level").is_none() {
            body["level"] = json!(3);
        }
        plan_as_written(body)
    }

    fn refused(body: Value) -> ApiError {
        planned(body).unwrap_err()
    }

    fn code(body: Value) -> ErrorCode {
        refused(body).code()
    }

    #[test]
    fn a_url_alone_is_a_navigation_with_every_default() {
        let planned = planned(json!({ "url": "https://example.org/a?b=1" })).unwrap();
        let order = &planned.order;

        assert_eq!(order.url.as_str(), "https://example.org/a?b=1");
        assert_eq!(order.method, http::Method::GET);
        assert_eq!((order.level, planned.level), (Level::Human, Level::Human));
        assert_eq!(order.kind, RequestKind::Document);
        assert!(order.headers.is_empty() && order.body.is_none());
        assert_eq!((order.timeout, order.max_retries), (None, None));
        assert!(order.pacing.is_none() && order.behavior.is_none());
        assert_eq!(order.wait.until, WaitUntil::Load);
        // The document and the headers, and nothing a page could notice.
        assert!(planned.include.has(Include::Dom) && planned.include.has(Include::Headers));
        assert!(!planned.include.has(Include::Raw));
        assert!(order.capture.dom && !order.capture.raw && !order.capture.console);
        assert!(order.capture.screenshot.is_none() && order.capture.pdf.is_none());
        assert!(order.keep.dom && !order.keep.raw && !order.keep.screenshot);
        assert!(planned.session.is_none());

        // On the http path, levels 0 and 1, the default is the body.
        for (number, level) in [(0, Level::Plain), (1, Level::Emulated)] {
            let http = planned_ok(json!({ "url": "https://example.org/", "level": number }));
            assert!(http.include.has(Include::Raw) && !http.include.has(Include::Dom));
            assert!(http.order.keep.raw && !http.order.keep.dom);
            assert_eq!(http.order.level, level);
        }
        let fast = planned_ok(json!({ "url": "https://example.org/", "level": 2 }));
        assert_eq!(fast.order.level, Level::Fast);
        assert!(fast.include.has(Include::Dom));
    }

    #[test]
    fn a_request_names_its_level_and_the_level_decides_what_else_it_may_name() {
        let url = "https://example.org/";
        // Nothing is decided for the caller.
        let missing = plan_as_written(json!({ "url": url })).unwrap_err();
        assert_eq!(missing.code(), ErrorCode::LevelRequired);
        assert_eq!(missing.problem().extensions["field"], "level");
        let four = refused(json!({ "url": url, "level": 4 }));
        assert_eq!(four.code(), ErrorCode::ValidationFailed);
        assert_eq!(four.problem().extensions["field"], "level");
        assert!(
            serde_json::from_value::<FetchRequest>(json!({ "url": url, "level": "3" })).is_err()
        );

        // A session is a persona's.
        let session = refused(json!({ "url": url, "level": 0, "session_id": "s1" }));
        assert_eq!(session.code(), ErrorCode::SessionNeedsPersona);
        assert_eq!(session.problem().extensions["field"], "session_id");
        for level in 1..=3 {
            planned_ok(json!({ "url": url, "level": level, "session_id": "s1" }));
        }
        // What only a persona has, and what only a browser does.
        for (level, field, value) in [
            (0, "persona", json!({ "countries": ["DE"] })),
            (0, "pacing", json!({ "profile": "fast" })),
            (0, "wait", json!({ "until": "load" })),
            (1, "wait", json!({ "until": "load" })),
            (0, "behavior", json!({ "dwell_ms": 3000 })),
            (1, "behavior", json!({ "dwell_ms": 3000 })),
            (2, "behavior", json!({ "dwell_ms": 3000 })),
        ] {
            let refused = refused(json!({ "url": url, "level": level, field: value }));
            assert_eq!(
                refused.code(),
                ErrorCode::ValidationFailed,
                "{level} {field}"
            );
            assert_eq!(
                refused.problem().extensions["field"],
                field,
                "{level} {field}"
            );
        }
        for (level, field, value) in [
            (1, "persona", json!({ "countries": ["DE"] })),
            (1, "pacing", json!({ "profile": "fast" })),
            (2, "wait", json!({ "until": "load" })),
            (2, "pacing", json!({ "profile": "fast" })),
            (3, "behavior", json!({ "dwell_ms": 3000 })),
        ] {
            planned_ok(json!({ "url": url, "level": level, field: value }));
        }
        // What every level takes.
        for level in 0..=3 {
            planned_ok(json!({
                "url": url, "level": level, "robots": "report", "cache_ttl_s": 60,
                "include": ["headers", "extract"],
            }));
        }
    }

    #[test]
    fn a_plain_client_sends_the_callers_own_headers_and_never_keeps_a_credential() {
        let url = "https://example.org/";
        let plain = |headers: Value| planned(json!({ "url": url, "level": 0, "headers": headers }));

        // The caller's request as given, its credentials included.
        let given = plain(json!({
            "User-Agent": "acme-bot/2 (+https://acme.example/bot)",
            "Cookie": "sid=abc", "Authorization": "Bearer t0k3n",
            "Accept": "application/json", "Referer": "https://example.org/list",
        }))
        .unwrap();
        assert_eq!(given.order.headers.len(), 5);
        // What is the transport's or the proxy's is not the caller's.
        for name in [
            "Proxy-Authorization",
            "X-Forwarded-For",
            "Forwarded",
            "Accept-Encoding",
            "Host",
            "Content-Length",
            "Connection",
            "Via",
        ] {
            let refused = plain(json!({ name: "x" })).unwrap_err();
            assert_eq!(refused.code(), ErrorCode::HeaderDenied, "{name}");
            assert_eq!(refused.problem().extensions["header"], name, "{name}");
        }
        // A method and a body, as at level 1.
        let post = planned_ok(json!({
            "url": url, "level": 0, "method": "POST", "body": "a=1",
            "headers": { "Content-Type": "application/x-www-form-urlencoded" },
        }));
        assert_eq!(post.order.method, http::Method::POST);

        // Kept, the request names the credential and not its value.
        let request: FetchRequest = serde_json::from_value(json!({
            "url": url, "level": 0,
            "headers": { "cookie": "sid=abc", "AUTHORIZATION": "Bearer t0k3n", "X-Api": "1" },
        }))
        .unwrap();
        let kept = kept_spec(&request).unwrap();
        assert_eq!(kept["headers"]["cookie"], "[redacted]");
        assert_eq!(kept["headers"]["AUTHORIZATION"], "[redacted]");
        assert_eq!(kept["headers"]["X-Api"], "1");
        assert!(!kept.to_string().contains("t0k3n") && !kept.to_string().contains("sid=abc"));
        assert!(!lost_credential(&request));
        let read_back: FetchRequest = serde_json::from_value(kept).unwrap();
        assert!(lost_credential(&read_back));
    }

    fn planned_ok(body: Value) -> Planned {
        planned(body).unwrap()
    }

    #[test]
    fn the_dropped_locale_field_and_other_unknown_fields_do_not_parse() {
        for body in [
            json!({ "url": "https://example.org/", "locale": "de-DE" }),
            json!({ "url": "https://example.org/", "behavior": { "wander": false } }),
            json!({ "url": "https://example.org/", "render": "direct" }),
            // The path of 1.x is gone: the level names it.
            json!({ "url": "https://example.org/", "level": 3, "render": "browser" }),
            json!({ "url": "https://example.org/", "include": ["cookies"] }),
            json!({ "render": "auto" }),
        ] {
            assert!(
                serde_json::from_value::<FetchRequest>(body.clone()).is_err(),
                "{body}"
            );
        }
    }

    #[test]
    fn a_url_that_cannot_be_fetched_is_refused() {
        assert_eq!(code(json!({ "url": "not a url" })), ErrorCode::InvalidUrl);
        let long = format!("https://example.org/{}", "a".repeat(8200));
        assert_eq!(code(json!({ "url": long })), ErrorCode::InvalidUrl);
        // At the limit it parses.
        let fits = format!("https://example.org/{}", "a".repeat(8192 - 20));
        assert_eq!(fits.len(), 8192);
        planned_ok(json!({ "url": fits }));

        // What the guard says is sorted into what is wrong with the URL
        // and where it points.
        for (denial, expected) in [
            (SsrfDenial::Scheme("ftp".to_owned()), ErrorCode::InvalidUrl),
            (SsrfDenial::Credentials, ErrorCode::InvalidUrl),
            (SsrfDenial::Port, ErrorCode::InvalidUrl),
            (
                SsrfDenial::IpLiteral("93.184.216.34".parse().unwrap()),
                ErrorCode::SsrfDenied,
            ),
            (
                SsrfDenial::Address {
                    address: "10.0.0.1".parse().unwrap(),
                    range: DeniedRange::Private,
                },
                ErrorCode::SsrfDenied,
            ),
            (
                SsrfDenial::Name {
                    host: "localhost".to_owned(),
                    reason: "it names this machine",
                },
                ErrorCode::SsrfDenied,
            ),
            (
                SsrfDenial::Unresolved("gone.example".to_owned()),
                ErrorCode::SsrfDenied,
            ),
        ] {
            let refused = url_refused(&denial);
            assert_eq!(refused.code(), expected, "{denial}");
            assert_eq!(refused.problem().extensions["field"], "url");
        }
    }

    #[test]
    fn what_a_browser_cannot_send_needs_level_zero_or_one() {
        let url = "https://example.org/api";
        for (body, field) in [
            (json!({ "url": url, "method": "POST" }), "method"),
            (
                json!({ "url": url, "method": "head", "level": 2 }),
                "method",
            ),
            (
                json!({ "url": url, "headers": { "Referer": "https://example.org/" } }),
                "headers",
            ),
            (
                json!({ "url": url, "headers": { "X-Requested-With": "XMLHttpRequest" }, "level": 2 }),
                "headers",
            ),
        ] {
            let refused = refused(body.clone());
            assert_eq!(refused.code(), ErrorCode::LevelNeedsHttp, "{body}");
            assert_eq!(refused.problem().extensions["field"], field, "{body}");
        }
        // A script's request is a browser's emulated: level 1's only.
        for level in [0, 2, 3] {
            assert_eq!(
                code(json!({
                    "url": url, "level": level, "request_kind": "fetch",
                    "headers": { "Referer": "https://example.org/" },
                })),
                ErrorCode::LevelKindConflict,
                "{level}"
            );
        }

        // At level 1 all of it is accepted as written.
        let post = planned_ok(json!({
            "url": url, "level": 1, "method": "post",
            "headers": { "Content-Type": "application/x-www-form-urlencoded", "Referer": "https://example.org/form" },
            "body": "q=1",
        }));
        assert_eq!(post.order.method, http::Method::POST);
        assert_eq!(post.order.body.as_deref(), Some(b"q=1".as_slice()));
        assert_eq!(post.order.headers.len(), 2);

        assert_eq!(
            code(json!({ "url": url, "method": "PUT", "level": 1 })),
            ErrorCode::ValidationFailed
        );
        // A body is a POST's and says what it is.
        let refused = refused(json!({
            "url": url, "level": 1, "body": "x", "headers": { "Content-Type": "text/plain" },
        }));
        assert_eq!(refused.problem().extensions["field"], "body");
        assert_eq!(
            code(json!({ "url": url, "level": 1, "method": "POST", "body": "x" })),
            ErrorCode::ValidationFailed
        );
    }

    #[test]
    fn a_header_that_is_the_personas_or_the_transports_is_refused_by_name() {
        let with = |name: &str| {
            refused(json!({
                "url": "https://example.org/", "level": 1, "headers": { name: "x" },
            }))
        };

        for name in [
            "User-Agent",
            "accept-language",
            "Accept-Encoding",
            "Cookie",
            "Origin",
            "Sec-Fetch-Site",
            "sec-ch-ua",
            "Proxy-Authorization",
            "Host",
            "Content-Length",
            "Upgrade-Insecure-Requests",
            "Priority",
        ] {
            let refused = with(name);
            assert_eq!(refused.code(), ErrorCode::HeaderDenied, "{name}");
            assert_eq!(refused.problem().extensions["header"], name, "{name}");
        }
        assert_eq!(with("bad name").code(), ErrorCode::HeaderInvalid);
        assert_eq!(
            code(json!({
                "url": "https://example.org/", "level": 1,
                "headers": { "X-A": "line\nbreak" },
            })),
            ErrorCode::HeaderInvalid
        );
        assert_eq!(
            code(json!({
                "url": "https://example.org/", "level": 1,
                "headers": { "X-Token": "a", "x-token": "b" },
            })),
            ErrorCode::HeaderInvalid
        );
        assert_eq!(
            code(json!({
                "url": "https://example.org/", "level": 1,
                "headers": { "Referer": "about:blank" },
            })),
            ErrorCode::HeaderInvalid
        );
        let many: serde_json::Map<String, Value> =
            (0..33).map(|n| (format!("X-H{n}"), json!("v"))).collect();
        assert_eq!(
            code(json!({ "url": "https://example.org/", "level": 1, "headers": many })),
            ErrorCode::HeaderInvalid
        );
        // What is neither is sent.
        planned_ok(json!({
            "url": "https://example.org/", "level": 1,
            "headers": { "X-Requested-With": "XMLHttpRequest", "Authorization": "Bearer t" },
        }));
    }

    #[test]
    fn a_scripts_request_needs_its_page_and_only_headers_with_a_known_place() {
        let api = "https://example.org/api/items";
        let fetch = |headers: Value, more: Value| {
            let mut body = json!({
                "url": api, "level": 1, "request_kind": "fetch", "headers": headers,
            });
            body.as_object_mut()
                .unwrap()
                .extend(more.as_object().unwrap().clone());
            planned(body)
        };

        assert_eq!(
            fetch(json!({}), json!({})).unwrap_err().code(),
            ErrorCode::RequestKindNeedsReferer
        );
        let ok = fetch(
            json!({ "Referer": "https://example.org/list", "Accept": "application/json" }),
            json!({}),
        )
        .unwrap();
        assert_eq!(ok.order.kind, RequestKind::Fetch);
        let xhr = planned_ok(json!({
            "url": api, "level": 1, "request_kind": "xhr",
            "headers": { "Referer": "https://example.org/list" },
        }));
        assert_eq!(xhr.order.kind, RequestKind::Xhr);

        // No capture says where Chrome puts this one in a fetch().
        let unplaced = fetch(
            json!({ "Referer": "https://example.org/list", "X-Requested-With": "XMLHttpRequest" }),
            json!({}),
        )
        .unwrap_err();
        assert_eq!(unplaced.code(), ErrorCode::HeaderNotPlaceable);
        assert_eq!(unplaced.problem().extensions["header"], "X-Requested-With");

        // A same-origin JSON POST is what a page's script sends every day.
        fetch(
            json!({ "Referer": "https://example.org/list", "Content-Type": "application/json" }),
            json!({ "method": "POST", "body": "{\"a\":1}" }),
        )
        .unwrap();
        // The same from another origin is preflighted by a browser.
        let preflighted = fetch(
            json!({ "Referer": "https://www.example.org/list", "Content-Type": "application/json" }),
            json!({ "method": "POST", "body": "{\"a\":1}" }),
        )
        .unwrap_err();
        assert_eq!(preflighted.code(), ErrorCode::PreflightRequired);
        // A simple request across origins is not.
        fetch(
            json!({ "Referer": "https://www.example.org/list" }),
            json!({}),
        )
        .unwrap();
        fetch(
            json!({ "Referer": "https://www.example.org/list", "Content-Type": "text/plain" }),
            json!({ "method": "POST", "body": "a=1" }),
        )
        .unwrap();
    }

    #[test]
    fn include_asks_only_for_what_the_level_and_this_release_can_give() {
        let url = "https://example.org/";
        for level in [0, 1] {
            for part in ["dom", "xhr", "screenshot", "pdf", "console"] {
                let refused = refused(json!({ "url": url, "level": level, "include": [part] }));
                assert_eq!(refused.code(), ErrorCode::LevelIncludeConflict, "{part}");
                assert_eq!(refused.problem().extensions["include"], part);
            }
        }
        planned_ok(json!({ "url": url, "level": 1, "include": ["raw", "headers"] }));

        // Extraction is read from the document the visit captured: in a
        // browser that is the DOM, which is captured for it and kept only
        // when the request asks for it too; on the http path it is the
        // body.
        let extracting = planned_ok(json!({ "url": url, "include": ["extract"] }));
        assert!(extracting.include.has(Include::Extract));
        assert!(extracting.order.capture.dom && !extracting.order.keep.dom);
        let options = extracting.order.extract.unwrap();
        assert!(options.markdown && options.links);
        let lean = planned_ok(json!({
            "url": url,
            "include": ["dom", "extract"],
            "extract": { "markdown": false, "links": false },
        }));
        assert!(lean.order.keep.dom);
        let options = lean.order.extract.unwrap();
        assert!(!options.markdown && !options.links);
        planned_ok(json!({ "url": url, "level": 1, "include": ["raw", "extract"] }));
        assert!(planned_ok(json!({ "url": url })).order.extract.is_none());
        assert!(
            serde_json::from_value::<FetchRequest>(
                json!({ "url": url, "include": ["extract"], "extract": { "summary": true } })
            )
            .is_err()
        );

        // What a request says of robots.txt and the cache is the order's;
        // whether the site's policy allows it is settled where the site is
        // known.
        for (robots, wish) in [
            ("policy", RobotsWish::Policy),
            ("report", RobotsWish::Report),
            ("enforce", RobotsWish::Enforce),
            ("ignore", RobotsWish::Ignore),
        ] {
            let planned = planned_ok(json!({ "url": url, "robots": robots, "cache_ttl_s": 60 }));
            assert_eq!(planned.order.robots, wish);
            assert!(planned.order.cache.is_none());
        }
        assert_eq!(
            planned_ok(json!({ "url": url })).order.robots,
            RobotsWish::Policy
        );

        // Options without the part they belong to are a mistake, not noise.
        for field in ["screenshot", "pdf", "xhr", "extract"] {
            let refused = refused(json!({ "url": url, field: {} }));
            assert_eq!(refused.code(), ErrorCode::ValidationFailed, "{field}");
            assert_eq!(refused.problem().extensions["field"], field);
        }
        // An empty list asks for nothing but the outcome.
        let bare = planned_ok(json!({ "url": url, "include": [] }));
        assert!(!bare.include.has(Include::Dom) && !bare.order.capture.dom);
        assert!(bare.order.keep.is_nothing());
    }

    #[test]
    fn captures_are_asked_of_the_browser_as_the_request_names_them() {
        let planned = planned_ok(json!({
            "url": "https://example.org/",
            "include": ["dom", "raw", "xhr", "screenshot", "pdf", "console"],
            "screenshot": { "full_page": true, "format": "jpeg", "quality": 70 },
            "pdf": { "format": "letter", "landscape": true },
            "xhr": { "content_types": ["application/json", "text/*"], "max_bytes": 5000, "max_responses": 7 },
            "wait": { "until": "network_idle", "selector": "#results", "extra_ms": 250 },
            "max_body_bytes": 2_000_000,
        }));
        let capture = &planned.order.capture;

        assert!(capture.dom && capture.raw && capture.console);
        let screenshot = capture.screenshot.as_ref().unwrap();
        assert!(screenshot.full_page);
        assert_eq!(screenshot.format, ScreenshotFormat::Jpeg);
        assert_eq!(screenshot.quality, Some(70));
        let pdf = capture.pdf.as_ref().unwrap();
        assert_eq!(pdf.landscape, Some(true));
        assert_eq!(pdf.print_background, Some(true));
        let xhr = capture.xhr.as_ref().unwrap();
        assert_eq!(xhr.content_types, ["application/json", "text/*"]);
        assert_eq!((xhr.max_bytes, xhr.max_responses), (5000, Some(7)));
        let wait = &planned.order.wait;
        assert_eq!(wait.until, WaitUntil::NetworkIdle);
        assert_eq!(wait.selector.as_deref(), Some("#results"));
        assert_eq!(wait.extra_ms, Some(250));
        assert_eq!(planned.order.max_body_bytes, Some(2_000_000));
        let keep = planned.order.keep;
        assert!(keep.dom && keep.raw && keep.xhr && keep.screenshot && keep.pdf);

        // Defaults of the parts: a viewport shot no page can see, JSON only.
        let plain = planned_ok(json!({
            "url": "https://example.org/", "include": ["screenshot", "xhr", "pdf"],
        }));
        let screenshot = plain.order.capture.screenshot.as_ref().unwrap();
        assert!(!screenshot.full_page);
        assert_eq!(screenshot.format, ScreenshotFormat::Webp);
        let xhr = plain.order.capture.xhr.as_ref().unwrap();
        assert_eq!(xhr.content_types, ["application/json"]);
        assert_eq!(xhr.max_bytes, 2_000_000);

        for (body, field) in [
            (
                json!({ "include": ["screenshot"], "screenshot": { "quality": 0 } }),
                "screenshot.quality",
            ),
            (
                json!({ "include": ["xhr"], "xhr": { "content_types": [] } }),
                "xhr.content_types",
            ),
            (
                json!({ "include": ["xhr"], "xhr": { "max_bytes": 0 } }),
                "xhr.max_bytes",
            ),
            (
                json!({ "include": ["xhr"], "xhr": { "max_responses": 501 } }),
                "xhr.max_responses",
            ),
            (json!({ "wait": { "selector": "" } }), "wait.selector"),
            (json!({ "wait": { "extra_ms": 60001 } }), "wait.extra_ms"),
            (json!({ "max_body_bytes": 0 }), "max_body_bytes"),
            (json!({ "max_body_bytes": 100_000_001 }), "max_body_bytes"),
            (json!({ "max_retries": 11 }), "max_retries"),
        ] {
            let mut request = body.clone();
            request["url"] = json!("https://example.org/");
            let refused = refused(request);
            assert_eq!(refused.code(), ErrorCode::ValidationFailed, "{body}");
            assert_eq!(refused.problem().extensions["field"], field, "{body}");
        }
    }

    #[test]
    fn behavior_may_be_shortened_and_a_timeout_may_not_cut_it_to_nothing() {
        let url = "https://example.org/";
        let brisk = planned_ok(json!({
            "url": url,
            "behavior": { "scroll_depth": 0.3, "dwell_ms": 3000, "referer_strategy": "none" },
        }));
        let wish = brisk.order.behavior.unwrap();
        assert_eq!(wish.scroll_depth, Some(0.3));
        assert_eq!(wish.dwell, Some(Duration::from_secs(3)));
        assert_eq!(wish.referer_strategy, Some(RefererStrategy::None));
        // An empty object asks for nothing.
        assert!(
            planned_ok(json!({ "url": url, "behavior": {} }))
                .order
                .behavior
                .is_none()
        );
        // No scrolling is a wish; so is a dwell of "as short as allowed".
        let still =
            planned_ok(json!({ "url": url, "behavior": { "scroll_depth": 0, "dwell_ms": 0 } }));
        assert_eq!(still.order.behavior.unwrap().scroll_depth, Some(0.0));

        for (behavior, field) in [
            (json!({ "scroll_depth": 1.5 }), "behavior.scroll_depth"),
            (json!({ "scroll_depth": -0.1 }), "behavior.scroll_depth"),
            (json!({ "dwell_ms": 60001 }), "behavior.dwell_ms"),
        ] {
            let refused = refused(json!({ "url": url, "behavior": behavior }));
            assert_eq!(refused.problem().extensions["field"], field);
        }
        // Behavior and waits are a browser's.
        for field in ["behavior", "wait"] {
            let refused = refused(json!({ "url": url, "level": 1, field: {} }));
            assert_eq!(refused.problem().extensions["field"], field);
        }

        // A browser attempt gets at least 15 s: less would be a bare load.
        for level in [2, 3] {
            let refused = refused(json!({ "url": url, "level": level, "timeout_ms": 14999 }));
            assert_eq!(refused.code(), ErrorCode::TimeoutTooShort, "{level}");
            assert_eq!(refused.problem().extensions["minimum_ms"], 15000);
            let fits = planned_ok(json!({ "url": url, "level": level, "timeout_ms": 15000 }));
            assert_eq!(fits.order.timeout, Some(Duration::from_secs(15)));
        }
        for level in [0, 1] {
            assert_eq!(
                code(json!({ "url": url, "level": level, "timeout_ms": 999 })),
                ErrorCode::TimeoutTooShort
            );
            planned_ok(json!({ "url": url, "level": level, "timeout_ms": 1000 }));
        }
        // The hard cap of an attempt.
        assert_eq!(
            code(json!({ "url": url, "timeout_ms": 300_001 })),
            ErrorCode::ValidationFailed
        );
        planned_ok(json!({ "url": url, "timeout_ms": 300_000 }));
    }

    #[test]
    fn pacing_is_a_profiles_name_or_a_profile_of_the_requests_own() {
        let url = "https://example.org/";

        let named = planned_ok(json!({ "url": url, "pacing": { "profile": "fast" } }));
        assert_eq!(
            named.order.pacing,
            Some(PacingRequest::Profile("fast".to_owned()))
        );
        let unknown = refused(json!({ "url": url, "pacing": { "profile": "warp" } }));
        assert_eq!(unknown.code(), ErrorCode::InvalidPacing);
        assert!(
            unknown.detail().contains("fast, human"),
            "{}",
            unknown.detail()
        );

        let custom = json!({
            "gap_ms": { "dist": "uniform", "min": 2000, "max": 4000 },
            "burst_pages": { "min": 3, "max": 6 },
            "burst_break_ms": { "dist": "uniform", "min": 10000, "max": 20000 },
            "daily_cap_per_persona_per_domain": 100,
            "domain_concurrency": 2,
            "domain_rpm": 10,
        });
        let own = planned_ok(json!({ "url": url, "pacing": { "custom": custom } }));
        assert!(matches!(own.order.pacing, Some(PacingRequest::Custom(_))));
        assert_eq!(
            code(json!({ "url": url, "pacing": { "profile": "fast", "custom": custom } })),
            ErrorCode::InvalidPacing
        );
        let mut broken = custom.clone();
        broken["domain_rpm"] = json!(0);
        assert_eq!(
            code(json!({ "url": url, "pacing": { "custom": broken } })),
            ErrorCode::InvalidPacing
        );
        // Neither is no wish.
        assert!(
            planned_ok(json!({ "url": url, "pacing": {} }))
                .order
                .pacing
                .is_none()
        );
    }

    #[test]
    fn hints_and_a_session_are_read_and_checked_for_their_form() {
        let url = "https://example.org/";
        let planned = planned_ok(json!({
            "url": url,
            "session_id": "proj-a-42",
            "persona": { "countries": ["de", "AT"], "engines": ["firefox"] },
            "max_retries": 0,
        }));

        assert_eq!(planned.session.as_ref().unwrap().as_str(), "proj-a-42");
        assert_eq!(planned.order.session, planned.session);
        let countries: Vec<String> = planned.countries.iter().map(ToString::to_string).collect();
        assert_eq!(countries, ["DE", "AT"]);
        assert_eq!(planned.order.prefer_countries, planned.countries);
        assert_eq!(planned.engines, [Engine::Firefox]);
        assert_eq!(planned.order.engines, [Engine::Firefox]);
        assert_eq!(planned.order.max_retries, Some(0));

        assert_eq!(
            code(json!({ "url": url, "session_id": "has space" })),
            ErrorCode::InvalidSessionId
        );
        assert_eq!(
            code(json!({ "url": url, "session_id": "x".repeat(65) })),
            ErrorCode::InvalidSessionId
        );
        let refused = refused(json!({ "url": url, "persona": { "countries": ["Germany"] } }));
        assert_eq!(refused.problem().extensions["field"], "persona.countries");
    }
}
