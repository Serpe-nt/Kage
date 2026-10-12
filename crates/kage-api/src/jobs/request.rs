//! What `POST /v1/jobs` takes (spec §16.6), and what it is checked for.
//!
//! A **batch** is a list of fetches. Each item is a [`FetchRequest`] and
//! is checked by the code that checks a synchronous fetch, so an item is
//! refused for what a fetch would be refused for, with the same code and
//! the item's position. What the items share is given once as `defaults`.
//!
//! A **flow** is a script of steps one persona performs in its browser
//! (spec §10.5). It starts with a `goto`; every page it is sent to passes
//! the SSRF guard before the job is accepted; a step done by the browser's
//! driver instead of the persona's hands (`human: false`) is refused unless
//! the policy of the flow's site allows it.
//!
//! A **crawl** is a site read from its seeds along its links; what it
//! takes and what it is checked for is in [`super::crawl`].
//!
//! A job is accepted whole or not at all: nothing is stored for a job one
//! of whose items is refused.

use std::time::Duration;

use kage_browser::contract::{CaptureOptions, FlowAction};
use kage_core::config::{Config, MAX_ATTEMPT_TIMEOUT};
use kage_core::pacing::PacingProfiles;
use kage_core::policy::{DomainPolicies, PolicySet};
use kage_scheduler::{FetchOrder, FlowPlan, Keep};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use url::Url;
use utoipa::ToSchema;

use super::crawl::CrawlJob;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::fetch::request::{
    FetchRequest, Include, PacingField, PdfField, PersonaHints, Planned, ScreenshotField, XhrField,
    plan, url_refused,
};
use crate::resolve::{HostResolver, guard};
use crate::schema::FreeForm;

/// Most steps a flow may have: the contract's own limit.
pub(crate) const MAX_FLOW_STEPS: usize = 100;

/// The whole time a flow may take when it does not say: three minutes.
pub(crate) const DEFAULT_FLOW_TIMEOUT: Duration = Duration::from_secs(180);

/// The shortest secret a webhook may be signed with, in bytes.
pub(crate) const MIN_WEBHOOK_SECRET: usize = 16;

/// The longest secret a webhook may be signed with, in bytes.
pub(crate) const MAX_WEBHOOK_SECRET: usize = 256;

/// The longest `Idempotency-Key`.
pub(crate) const MAX_IDEMPOTENCY_KEY: usize = 255;

/// Where a job's events are posted (spec §16.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WebhookField {
    /// The receiver: an `http` or `https` URL whose origin the operator
    /// listed in `webhooks.allowed_origins`
    /// (`webhook_origin_not_allowed` otherwise). Kage posts from its own
    /// address, follows no redirect, and shows of this URL only the host
    /// afterwards.
    #[schema(example = "https://hooks.example.org/kage")]
    pub(crate) url: String,
    /// The secret every delivery is signed with: 16 to 256 bytes. Kept
    /// encrypted and never returned. `X-Kage-Signature` is `sha256=` and
    /// the hexadecimal HMAC-SHA-256 of `<X-Kage-Timestamp>.<body>` under
    /// it.
    #[schema(min_length = 16, max_length = 256)]
    pub(crate) secret: String,
    /// Post `job.progress` whenever this many more items are over. Left
    /// out: only the end of the job is reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1)]
    pub(crate) progress_every: Option<u32>,
}

/// A batch: pages fetched one after another the way a visitor reads them.
///
/// Per site a batch has one visitor: one persona reads its pages of that
/// site in the order given, in sittings, with the pauses a person makes,
/// and comes back after its breaks; another persona takes over only when
/// that one cannot go on (it was blocked there, reached its daily cap, or
/// is gone). A site therefore sees one household reading, not a list being
/// dealt out to several addresses. That costs time: under the `human`
/// pacing about an hour for 50 pages of one site and a day for 300, which
/// is the daily cap of one persona, and a batch that meets the night ends
/// in the morning. `visitors` trades that for speed (about 20 minutes for
/// 50 pages with three), and a faster `pacing` does where the policy does
/// not lock it. Sites of one batch are independent and are read side by
/// side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({
    "type": "batch",
    "urls": ["https://example.org/listing?page=1", "https://example.org/listing?page=2"],
    "defaults": { "include": ["dom", "headers"], "behavior": { "scroll_depth": 0.3, "dwell_ms": 3000 } },
    "webhook": { "url": "https://hooks.example.org/kage", "secret": "a secret of sixteen bytes or more", "progress_every": 10 }
}))]
pub(crate) struct BatchJob {
    /// Pages fetched as `defaults` says, in this order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(example = json!(["https://example.org/listing?page=1"]))]
    pub(crate) urls: Vec<String>,
    /// Pages that differ from `defaults`: each a fetch request of its own,
    /// with whatever it sets taking the place of the default. They come
    /// after `urls`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<FetchRequest>)]
    pub(crate) items: Vec<Value>,
    /// What every item is fetched with unless it says otherwise: any field
    /// of a fetch request but `url` and `session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<FreeForm>)]
    pub(crate) defaults: Option<Map<String, Value>>,
    /// How many personas read one site for the batch at a time; 1 unless
    /// said, at most the `domain_concurrency` of the pacing in force for
    /// the site (3 under `human`). More is refused, not reduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, example = 1)]
    pub(crate) visitors: Option<u32>,
    /// An open session of this key: every item is fetched by the
    /// session's persona and by no other, with a fetch's rules (the
    /// `sessions` scope, `session_not_found`, `session_persona_conflict`).
    /// The job counts as use of the session while it has items left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 64, pattern = "^[A-Za-z0-9_.-]{1,64}$")]
    pub(crate) session_id: Option<String>,
    /// Where the job's events are posted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) webhook: Option<WebhookField>,
}

/// Load a page. The first step of every flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GotoStep {
    /// The page: an `http` or `https` URL that passes the SSRF guard.
    pub(crate) url: String,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// A step that acts on one element: `click` or `hover`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetStep {
    /// The CSS selector of the element.
    #[schema(min_length = 1, max_length = 1000)]
    pub(crate) selector: String,
    /// False: done by the browser's driver instead of the persona's mouse
    /// and keyboard, which a page can tell apart. Refused
    /// (`driver_input_denied`) unless the policy of the flow's site has
    /// `driver_input: true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) human: Option<bool>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Type text into an element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeStep {
    /// The CSS selector of the input.
    #[schema(min_length = 1, max_length = 1000)]
    pub(crate) selector: String,
    /// The text.
    #[schema(max_length = 10_000)]
    pub(crate) text: String,
    /// See `click`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) human: Option<bool>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Press one key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PressStep {
    /// The key as `KeyboardEvent.key` names it, for example `Enter`.
    #[schema(min_length = 1, max_length = 40)]
    pub(crate) key: String,
    /// See `click`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) human: Option<bool>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Scroll the page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScrollStep {
    /// How far down to end, as a share of the document's height.
    #[schema(minimum = 0, maximum = 1)]
    pub(crate) depth: f64,
    /// See `click`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) human: Option<bool>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Wait for one condition: exactly one of the four is given.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitForStep {
    /// Until this CSS selector matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(min_length = 1, max_length = 1000)]
    pub(crate) selector: Option<String>,
    /// Until the page's address matches this pattern: `**` is anything,
    /// `*` anything but a `/`, everything else is itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 2000)]
    pub(crate) url: Option<String>,
    /// Until no request ran for half a second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) network_idle: Option<bool>,
    /// For this long, in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(maximum = 120_000)]
    pub(crate) ms: Option<u64>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Choose an option of a select element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectStep {
    /// The CSS selector of the select element.
    #[schema(min_length = 1, max_length = 1000)]
    pub(crate) selector: String,
    /// The value of the option.
    #[schema(max_length = 1000)]
    pub(crate) value: String,
    /// See `click`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) human: Option<bool>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Go back one page in the tab's history.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackStep {
    /// See `click`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) human: Option<bool>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Capture the page as it is at this point of the flow.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExtractStep {
    /// What to capture: `dom`, `raw`, `headers`, `xhr`, `console`. `dom`
    /// and `headers` unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) include: Option<Vec<Include>>,
    /// Which responses of the page's scripts are kept; with `xhr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) xhr: Option<XhrField>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Take a screenshot at this point of the flow.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScreenshotStep {
    /// How it is taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) options: Option<ScreenshotField>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// Print a PDF at this point of the flow.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct PdfStep {
    /// How it is printed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) options: Option<PdfField>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// End the flow unless a condition holds: exactly one of the two is given.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssertStep {
    /// This CSS selector must match an element.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(min_length = 1, max_length = 1000)]
    pub(crate) selector: Option<String>,
    /// The page's text must contain this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 2000)]
    pub(crate) text_contains: Option<String>,
    /// The step's own budget in milliseconds; 30000 unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 100, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
}

/// One step of a flow, chosen by `action` (spec §10.5).
///
/// The steps that touch the page (`click`, `type`, `press`, `scroll`,
/// `select`, `hover`, `back`) are done with the persona's own mouse and
/// keyboard. The first step that fails ends the flow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum FlowStepField {
    /// Load a page.
    Goto(GotoStep),
    /// Click an element.
    Click(TargetStep),
    /// Type text into an element.
    Type(TypeStep),
    /// Press one key.
    Press(PressStep),
    /// Scroll the page.
    Scroll(ScrollStep),
    /// Wait for a condition.
    WaitFor(WaitForStep),
    /// Choose an option of a select element.
    Select(SelectStep),
    /// Move the pointer over an element.
    Hover(TargetStep),
    /// Go back one page.
    Back(BackStep),
    /// Capture the page.
    Extract(ExtractStep),
    /// Take a screenshot.
    Screenshot(ScreenshotStep),
    /// Print a PDF.
    Pdf(PdfStep),
    /// End the flow unless a condition holds.
    Assert(AssertStep),
}

impl FlowStepField {
    const fn timeout_ms(&self) -> Option<u64> {
        match self {
            Self::Goto(step) => step.timeout_ms,
            Self::Click(step) | Self::Hover(step) => step.timeout_ms,
            Self::Type(step) => step.timeout_ms,
            Self::Press(step) => step.timeout_ms,
            Self::Scroll(step) => step.timeout_ms,
            Self::WaitFor(step) => step.timeout_ms,
            Self::Select(step) => step.timeout_ms,
            Self::Back(step) => step.timeout_ms,
            Self::Extract(step) => step.timeout_ms,
            Self::Screenshot(step) => step.timeout_ms,
            Self::Pdf(step) => step.timeout_ms,
            Self::Assert(step) => step.timeout_ms,
        }
    }

    /// Whether the step asks to be done by the driver.
    const fn by_driver(&self) -> bool {
        let human = match self {
            Self::Click(step) | Self::Hover(step) => step.human,
            Self::Type(step) => step.human,
            Self::Press(step) => step.human,
            Self::Scroll(step) => step.human,
            Self::Select(step) => step.human,
            Self::Back(step) => step.human,
            Self::Goto(_)
            | Self::WaitFor(_)
            | Self::Extract(_)
            | Self::Screenshot(_)
            | Self::Pdf(_)
            | Self::Assert(_) => None,
        };
        matches!(human, Some(false))
    }
}

/// What a flow may change of how its persona behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct FlowBehaviorField {
    /// The median of the time the persona stays on a page after arriving
    /// there, before the next step, in milliseconds. Left out: the
    /// persona's own. Never below the policy's `min_dwell` (2 s unless
    /// set), and ignored where the policy locks behavior. A flow always
    /// dwells after it arrives and always pauses between its actions:
    /// nothing turns that off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(maximum = 60_000)]
    pub(crate) dwell_ms: Option<u64>,
    /// Now and then type a neighbouring key, notice, delete and retype.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) typos: Option<bool>,
}

/// A flow: a script of steps one persona performs in its browser, as one
/// visit.
///
/// The persona arrives by a typed navigation, stays on every page it comes
/// to for a while with its pointer moving, and pauses between its actions
/// as a person does, so a flow takes longer than its steps: plan about ten
/// seconds for every page it visits. A flow whose page turns out to be a
/// wall is a blocked attempt: the persona leaves, and another one runs the
/// flow again from its first step, inside the policy's retry budget (never
/// in a session). A step that fails on a page that is no wall is the
/// script's failure and is not retried. The job's items are the steps, in
/// order, and one last item for the page the flow ended on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(example = json!({
    "type": "flow",
    "actions": [
        { "action": "goto", "url": "https://example.org/" },
        { "action": "click", "selector": "#next" },
        { "action": "wait_for", "selector": "#items" },
        { "action": "extract", "include": ["dom"] }
    ],
    "include": ["dom", "headers"]
}))]
pub(crate) struct FlowJob {
    /// The steps, at most 100. The first is a `goto`.
    #[schema(min_items = 1, max_items = 100)]
    pub(crate) actions: Vec<FlowStepField>,
    /// What is returned and kept of the page the flow ends on: `dom` and
    /// `headers` unless said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) include: Option<Vec<Include>>,
    /// How the last page's screenshot is taken; with `include:
    /// ["screenshot"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) screenshot: Option<ScreenshotField>,
    /// How the last page's PDF is printed; with `include: ["pdf"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pdf: Option<PdfField>,
    /// Which responses of the last page's scripts are kept; with
    /// `include: ["xhr"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) xhr: Option<XhrField>,
    /// Which personas the flow would have.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) persona: Option<PersonaHints>,
    /// An open session of this key: its persona runs the flow, and no
    /// other; a flow in a session is never run again after a block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(max_length = 64, pattern = "^[A-Za-z0-9_.-]{1,64}$")]
    pub(crate) session_id: Option<String>,
    /// The pacing of the persona's arrival at the flow's site; ignored
    /// where the policy locks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pacing: Option<PacingField>,
    /// What the flow may change of how its persona behaves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) behavior: Option<FlowBehaviorField>,
    /// The whole time one run of the flow may take, in milliseconds:
    /// 180000 unless said, at least 15000, at most 300000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 15_000, maximum = 300_000)]
    pub(crate) timeout_ms: Option<u64>,
    /// How often the flow is run again by another persona after a block.
    /// Default: the policy's (2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(maximum = 10)]
    pub(crate) max_retries: Option<u32>,
    /// The largest body and document kept, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 100_000_000)]
    pub(crate) max_body_bytes: Option<u64>,
    /// Where the job's events are posted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) webhook: Option<WebhookField>,
}

/// A job, chosen by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum JobRequest {
    /// Pages fetched one after another.
    Batch(BatchJob),
    /// A script of steps in one browser.
    Flow(Box<FlowJob>),
    /// A site read from its seeds along its links.
    Crawl(Box<CrawlJob>),
}

impl JobRequest {
    /// The job's webhook, when it names one.
    pub(crate) const fn webhook(&self) -> Option<&WebhookField> {
        match self {
            Self::Batch(batch) => batch.webhook.as_ref(),
            Self::Flow(flow) => flow.webhook.as_ref(),
            Self::Crawl(crawl) => crawl.webhook.as_ref(),
        }
    }

    /// The session the job names.
    pub(crate) fn session_id(&self) -> Option<&str> {
        match self {
            Self::Batch(batch) => batch.session_id.as_deref(),
            Self::Flow(flow) => flow.session_id.as_deref(),
            Self::Crawl(crawl) => crawl.session_id.as_deref(),
        }
    }
}

/// What the checks of a job need of the server.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Checks<'a> {
    /// The configuration in force.
    pub(crate) config: &'a Config,
    /// The policies in force.
    pub(crate) policies: &'a PolicySet,
    /// The host-side lookup of the guard, when it is on.
    pub(crate) resolver: Option<&'a HostResolver>,
}

impl Checks<'_> {
    fn pacing(&self) -> &PacingProfiles {
        &self.policies.pacing
    }

    fn domains(&self) -> &DomainPolicies {
        &self.policies.domains
    }
}

/// One item of a batch that passed every check.
#[derive(Debug, Clone)]
pub(crate) struct PlannedItem {
    /// The fetch request as it is stored: complete, with the defaults in
    /// it.
    pub(crate) input: Value,
    /// The site the item belongs to: the host of its URL.
    pub(crate) site: String,
    /// The request as the pipeline takes it.
    pub(crate) planned: Planned,
}

/// A refusal that names the item it is about.
fn at_item(error: ApiError, index: usize) -> ApiError {
    error.with("item", u64::try_from(index).unwrap_or(u64::MAX))
}

/// The complete fetch request of one item: the defaults with what the item
/// sets in their place, and the job's session.
fn merged(
    defaults: Option<&Map<String, Value>>,
    item: Map<String, Value>,
    session: Option<&str>,
) -> Value {
    let mut request = defaults.cloned().unwrap_or_default();
    for (name, value) in item {
        request.insert(name, value);
    }
    if let Some(session) = session {
        request.insert("session_id".to_owned(), json!(session));
    }
    Value::Object(request)
}

/// Checks the items of a batch, each as the fetch it is.
///
/// # Errors
///
/// The first refusal, with the position of its item as `item`.
// One batch, checked item by item from top to bottom.
#[allow(clippy::too_many_lines)]
pub(crate) async fn plan_batch(
    batch: &BatchJob,
    checks: Checks<'_>,
) -> ApiResult<Vec<PlannedItem>> {
    let most = usize::try_from(checks.config.jobs.max_items).unwrap_or(usize::MAX);
    let count = batch.urls.len() + batch.items.len();
    if count == 0 {
        return Err(ApiError::field(
            "urls",
            "a batch has at least one item: give urls, items, or both",
        ));
    }
    if count > most {
        return Err(ApiError::field(
            "urls",
            format!("a job has at most {most} items (jobs.max_items); this one has {count}"),
        )
        .with("maximum", u64::try_from(most).unwrap_or(u64::MAX)));
    }
    if let Some(defaults) = &batch.defaults {
        for field in ["url", "session_id"] {
            if defaults.contains_key(field) {
                return Err(ApiError::field(
                    &format!("defaults.{field}"),
                    format!("{field} is not a default: it is the item's, or the job's"),
                ));
            }
        }
    }
    let from_urls = batch.urls.iter().map(|url| {
        let mut item = Map::new();
        item.insert("url".to_owned(), json!(url));
        Ok(item)
    });
    let from_items = batch.items.iter().map(|item| match item {
        Value::Object(item) if item.contains_key("session_id") => Err(ApiError::field(
            "session_id",
            "a session is the job's, not an item's: name it once, beside the items",
        )),
        Value::Object(item) => Ok(item.clone()),
        _ => Err(ApiError::field("items", "an item is a fetch request")),
    });

    let mut planned = Vec::with_capacity(count);
    for (index, item) in from_urls.chain(from_items).enumerate() {
        let item = item.map_err(|error| at_item(error, index))?;
        let input = merged(batch.defaults.as_ref(), item, batch.session_id.as_deref());
        let request: FetchRequest = serde_json::from_value(input).map_err(|error| {
            at_item(
                ApiError::new(ErrorCode::ValidationFailed, error.to_string()),
                index,
            )
        })?;
        let one = plan(&request, checks.pacing()).map_err(|error| at_item(error, index))?;
        guard(
            &one.order.url,
            checks.config.security.ssrf(),
            checks.resolver,
        )
        .await
        .map_err(|denial| at_item(url_refused(&denial), index))?;
        let site = one
            .order
            .url
            .host_str()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        // As it is stored: what was accepted, whole, in one spelling.
        let input = serde_json::to_value(&request).map_err(|error| ApiError::internal(&error))?;
        planned.push(PlannedItem {
            input,
            site,
            planned: one,
        });
    }

    let visitors = batch.visitors.unwrap_or(1);
    if visitors == 0 {
        return Err(ApiError::field("visitors", "at least 1"));
    }
    if visitors > 1 {
        for (index, item) in planned.iter().enumerate() {
            let policy = checks.domains().resolve(&item.site);
            let pacing = policy
                .pacing_for(item.planned.order.pacing.as_ref(), checks.pacing())
                .map_err(|error| {
                    at_item(
                        ApiError::new(ErrorCode::InvalidPacing, error.to_string()),
                        index,
                    )
                })?;
            let most = pacing.profile.domain_concurrency;
            if visitors > most {
                return Err(ApiError::field(
                    "visitors",
                    format!(
                        "{} takes at most {most} visitors at a time under the pacing in force \
                         for it; ask for no more, or for a pacing that allows more where the \
                         policy does not lock it",
                        item.site
                    ),
                )
                .with("maximum", most)
                .with("site", item.site.as_str()));
            }
        }
    }
    Ok(planned)
}

/// A flow that passed every check.
#[derive(Debug, Clone)]
pub(crate) struct PlannedFlow {
    /// The order: a browser attempt at the first page, with the flow in it.
    pub(crate) planned: Planned,
    /// What each capturing step keeps, by the step's position.
    pub(crate) keeps: Vec<Option<Keep>>,
    /// The site the flow starts on.
    pub(crate) site: String,
}

/// A refusal that names the step it is about.
fn at_step(error: ApiError, index: usize) -> ApiError {
    error.with("step", u64::try_from(index).unwrap_or(u64::MAX))
}

fn step_field(index: usize, field: &str, detail: impl Into<String>) -> ApiError {
    at_step(
        ApiError::field(&format!("actions[{index}].{field}"), detail),
        index,
    )
}

fn check_selector(index: usize, selector: &str) -> ApiResult<()> {
    if selector.is_empty() || selector.len() > 1000 {
        return Err(step_field(
            index,
            "selector",
            "a selector of 1 to 1000 characters",
        ));
    }
    Ok(())
}

/// What a capturing step captures and keeps, checked by the code that
/// checks a fetch's `include`.
fn capture_of(
    index: usize,
    start: &str,
    fields: Value,
    profiles: &PacingProfiles,
) -> ApiResult<(CaptureOptions, Keep)> {
    let mut request = Map::new();
    request.insert("url".to_owned(), json!(start));
    request.insert("render".to_owned(), json!("browser"));
    if let Value::Object(fields) = fields {
        request.extend(fields);
    }
    let request: FetchRequest = serde_json::from_value(Value::Object(request))
        .map_err(|error| ApiError::new(ErrorCode::ValidationFailed, error.to_string()))
        .map_err(|error| at_step(error, index))?;
    let planned = plan(&request, profiles).map_err(|error| at_step(error, index))?;
    Ok((planned.order.capture, planned.order.keep))
}

/// The steps as the sidecar takes them, with what each capturing step
/// keeps.
// One arm per kind of step, thirteen of them.
#[allow(clippy::too_many_lines)]
async fn steps_of(
    flow: &FlowJob,
    start: &str,
    checks: Checks<'_>,
) -> ApiResult<(Vec<FlowAction>, Vec<Option<Keep>>)> {
    let mut actions = Vec::with_capacity(flow.actions.len());
    let mut keeps = Vec::with_capacity(flow.actions.len());
    for (index, step) in flow.actions.iter().enumerate() {
        if step
            .timeout_ms()
            .is_some_and(|timeout| !(100..=300_000).contains(&timeout))
        {
            return Err(step_field(index, "timeout_ms", "between 100 and 300000"));
        }
        let timeout_ms = step.timeout_ms();
        let mut keep = None;
        let action = match step {
            FlowStepField::Goto(goto) => {
                let url = Url::parse(&goto.url)
                    .ok()
                    .filter(|_| goto.url.len() <= crate::fetch::request::MAX_URL_BYTES)
                    .ok_or_else(|| {
                        at_step(
                            ApiError::new(ErrorCode::InvalidUrl, "the URL does not parse")
                                .with("field", format!("actions[{index}].url")),
                            index,
                        )
                    })?;
                guard(&url, checks.config.security.ssrf(), checks.resolver)
                    .await
                    .map_err(|denial| {
                        at_step(
                            url_refused(&denial).with("field", format!("actions[{index}].url")),
                            index,
                        )
                    })?;
                FlowAction::Goto {
                    human: None,
                    timeout_ms,
                    url: url.to_string(),
                }
            }
            FlowStepField::Click(target) => {
                check_selector(index, &target.selector)?;
                FlowAction::Click {
                    human: target.human,
                    selector: target.selector.clone(),
                    timeout_ms,
                }
            }
            FlowStepField::Hover(target) => {
                check_selector(index, &target.selector)?;
                FlowAction::Hover {
                    human: target.human,
                    selector: target.selector.clone(),
                    timeout_ms,
                }
            }
            FlowStepField::Type(typed) => {
                check_selector(index, &typed.selector)?;
                if typed.text.len() > 10_000 {
                    return Err(step_field(index, "text", "at most 10000 characters"));
                }
                FlowAction::Type {
                    human: typed.human,
                    selector: typed.selector.clone(),
                    text: typed.text.clone(),
                    timeout_ms,
                }
            }
            FlowStepField::Press(press) => {
                if press.key.is_empty() || press.key.len() > 40 {
                    return Err(step_field(index, "key", "a key name of 1 to 40 characters"));
                }
                FlowAction::Press {
                    human: press.human,
                    key: press.key.clone(),
                    timeout_ms,
                }
            }
            FlowStepField::Scroll(scroll) => {
                if !(0.0..=1.0).contains(&scroll.depth) {
                    return Err(step_field(index, "depth", "between 0 and 1"));
                }
                FlowAction::Scroll {
                    depth: scroll.depth,
                    human: scroll.human,
                    timeout_ms,
                }
            }
            FlowStepField::WaitFor(wait) => {
                let given = [
                    wait.selector.is_some(),
                    wait.url.is_some(),
                    wait.network_idle == Some(true),
                    wait.ms.is_some(),
                ]
                .into_iter()
                .filter(|given| *given)
                .count();
                if given != 1 {
                    return Err(step_field(
                        index,
                        "selector",
                        "wait_for takes exactly one of selector, url, network_idle and ms",
                    ));
                }
                if let Some(selector) = &wait.selector {
                    check_selector(index, selector)?;
                }
                if wait.url.as_ref().is_some_and(|url| url.len() > 2000) {
                    return Err(step_field(index, "url", "at most 2000 characters"));
                }
                if wait.ms.is_some_and(|ms| ms > 120_000) {
                    return Err(step_field(index, "ms", "at most 120000"));
                }
                FlowAction::WaitFor {
                    human: None,
                    ms: wait.ms,
                    network_idle: wait.network_idle.filter(|idle| *idle),
                    selector: wait.selector.clone(),
                    timeout_ms,
                    url: wait.url.clone(),
                }
            }
            FlowStepField::Select(select) => {
                check_selector(index, &select.selector)?;
                if select.value.len() > 1000 {
                    return Err(step_field(index, "value", "at most 1000 characters"));
                }
                FlowAction::Select {
                    human: select.human,
                    selector: select.selector.clone(),
                    timeout_ms,
                    value: select.value.clone(),
                }
            }
            FlowStepField::Back(back) => FlowAction::Back {
                human: back.human,
                timeout_ms,
            },
            FlowStepField::Extract(extract) => {
                let include = extract
                    .include
                    .clone()
                    .unwrap_or_else(|| vec![Include::Dom, Include::Headers]);
                if include
                    .iter()
                    .any(|part| matches!(part, Include::Screenshot | Include::Pdf))
                {
                    return Err(step_field(
                        index,
                        "include",
                        "a screenshot and a PDF are steps of their own",
                    ));
                }
                let (capture, kept) = capture_of(
                    index,
                    start,
                    json!({ "include": include, "xhr": extract.xhr }),
                    checks.pacing(),
                )?;
                keep = Some(kept);
                FlowAction::Extract {
                    capture,
                    human: None,
                    timeout_ms,
                }
            }
            FlowStepField::Screenshot(shot) => {
                let (capture, kept) = capture_of(
                    index,
                    start,
                    json!({ "include": ["screenshot"], "screenshot": shot.options }),
                    checks.pacing(),
                )?;
                let Some(options) = capture.screenshot else {
                    return Err(at_step(
                        ApiError::internal(&"a screenshot step without options"),
                        index,
                    ));
                };
                keep = Some(kept);
                FlowAction::Screenshot {
                    human: None,
                    options,
                    timeout_ms,
                }
            }
            FlowStepField::Pdf(pdf) => {
                let (capture, kept) = capture_of(
                    index,
                    start,
                    json!({ "include": ["pdf"], "pdf": pdf.options }),
                    checks.pacing(),
                )?;
                let Some(options) = capture.pdf else {
                    return Err(at_step(
                        ApiError::internal(&"a pdf step without options"),
                        index,
                    ));
                };
                keep = Some(kept);
                FlowAction::Pdf {
                    human: None,
                    options,
                    timeout_ms,
                }
            }
            FlowStepField::Assert(assert) => {
                if assert.selector.is_some() == assert.text_contains.is_some() {
                    return Err(step_field(
                        index,
                        "selector",
                        "assert takes exactly one of selector and text_contains",
                    ));
                }
                if let Some(selector) = &assert.selector {
                    check_selector(index, selector)?;
                }
                if assert
                    .text_contains
                    .as_ref()
                    .is_some_and(|text| text.len() > 2000)
                {
                    return Err(step_field(
                        index,
                        "text_contains",
                        "at most 2000 characters",
                    ));
                }
                FlowAction::Assert {
                    human: None,
                    selector: assert.selector.clone(),
                    text_contains: assert.text_contains.clone(),
                    timeout_ms,
                }
            }
        };
        actions.push(action);
        keeps.push(keep);
    }
    Ok((actions, keeps))
}

/// Checks a flow and turns it into the order of one browser attempt.
///
/// # Errors
///
/// The first refusal, with the position of its step as `step` when it is
/// about one.
pub(crate) async fn plan_flow(flow: &FlowJob, checks: Checks<'_>) -> ApiResult<PlannedFlow> {
    if flow.actions.is_empty() || flow.actions.len() > MAX_FLOW_STEPS {
        return Err(ApiError::field(
            "actions",
            format!("a flow has 1 to {MAX_FLOW_STEPS} steps"),
        ));
    }
    let Some(FlowStepField::Goto(first)) = flow.actions.first() else {
        return Err(step_field(
            0,
            "action",
            "a flow starts with a goto: without one it would act on whatever page the browser \
             happens to show",
        ));
    };
    if flow
        .timeout_ms
        .is_some_and(|timeout| Duration::from_millis(timeout) > MAX_ATTEMPT_TIMEOUT)
    {
        return Err(ApiError::field(
            "timeout_ms",
            format!("at most {}", MAX_ATTEMPT_TIMEOUT.as_millis()),
        ));
    }
    let timeout = flow
        .timeout_ms
        .unwrap_or_else(|| u64::try_from(DEFAULT_FLOW_TIMEOUT.as_millis()).unwrap_or(u64::MAX));

    // Everything a flow shares with a fetch is checked as a fetch's: the
    // first page in a browser, with the flow's own include, hints, session,
    // pacing and limits.
    let mut request = Map::new();
    request.insert("url".to_owned(), json!(first.url));
    request.insert("render".to_owned(), json!("browser"));
    request.insert("timeout_ms".to_owned(), json!(timeout));
    for (name, value) in [
        ("include", json!(flow.include)),
        ("screenshot", json!(flow.screenshot)),
        ("pdf", json!(flow.pdf)),
        ("xhr", json!(flow.xhr)),
        ("persona", json!(flow.persona)),
        ("session_id", json!(flow.session_id)),
        ("pacing", json!(flow.pacing)),
        ("max_retries", json!(flow.max_retries)),
        ("max_body_bytes", json!(flow.max_body_bytes)),
        (
            "behavior",
            json!(
                flow.behavior
                    .and_then(|behavior| behavior.dwell_ms)
                    .map(|dwell_ms| json!({ "dwell_ms": dwell_ms }))
            ),
        ),
    ] {
        if !value.is_null() {
            request.insert(name.to_owned(), value);
        }
    }
    let request: FetchRequest = serde_json::from_value(Value::Object(request))
        .map_err(|error| ApiError::new(ErrorCode::ValidationFailed, error.to_string()))?;
    let mut planned = plan(&request, checks.pacing())?;
    // A flow in a session is its persona's alone: nobody else may run it
    // again, and the same persona does not walk into the same wall twice.
    if flow.session_id.is_some() {
        planned.order.max_retries = Some(0);
    }
    let site = planned
        .order
        .url
        .host_str()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let (actions, keeps) = steps_of(flow, &first.url, checks).await?;
    driver_allowed(flow, &site, checks)?;
    planned.order = FetchOrder {
        flow: Some(FlowPlan {
            actions,
            typos: flow
                .behavior
                .and_then(|behavior| behavior.typos)
                .unwrap_or(false),
        }),
        ..planned.order
    };
    Ok(PlannedFlow {
        planned,
        keeps,
        site,
    })
}

/// Driver input is the operator's to allow, per site: refuses a flow with
/// a step done by the browser's driver on a site whose policy does not say
/// so.
fn driver_allowed(flow: &FlowJob, site: &str, checks: Checks<'_>) -> ApiResult<()> {
    if checks.domains().resolve(site).driver_input {
        return Ok(());
    }
    let Some(index) = flow.actions.iter().position(FlowStepField::by_driver) else {
        return Ok(());
    };
    Err(at_step(
        ApiError::new(
            ErrorCode::DriverInputDenied,
            format!(
                "step {index} asks to be done by the browser's driver (human: false), which a \
                 page can tell from a person's input; the policy for {site} does not allow it. \
                 Leave human out, or have the operator set driver_input for the site"
            ),
        )
        .with("field", format!("actions[{index}].human"))
        .with("site", site),
        index,
    ))
}

/// A webhook that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedWebhook {
    /// The receiver.
    pub(crate) url: Url,
    /// The receiver's origin, `scheme://host:port`.
    pub(crate) origin: String,
    /// The signing secret.
    pub(crate) secret: String,
    /// `job.progress` every so many items.
    pub(crate) progress_every: Option<u32>,
}

/// Checks a job's webhook: its receiver is one the operator listed, its
/// URL is one Kage may post to, its secret is long enough.
///
/// # Errors
///
/// `invalid_url`, `webhook_origin_not_allowed`, `ssrf_denied` or
/// `validation_failed`, each naming the field.
pub(crate) async fn plan_webhook(
    webhook: &WebhookField,
    checks: Checks<'_>,
) -> ApiResult<PlannedWebhook> {
    const FIELD: &str = "webhook.url";
    let invalid = |detail: &str| ApiError::new(ErrorCode::InvalidUrl, detail).with("field", FIELD);
    let url = Url::parse(&webhook.url).map_err(|_| invalid("the webhook URL does not parse"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("a webhook is posted over http or https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("a webhook URL carries no credentials"));
    }
    let origin = kage_core::config::origin_of(&url)
        .filter(|_| url.port() != Some(0))
        .ok_or_else(|| invalid("the webhook URL has no host, or no port Kage could post to"))?;
    let webhooks = &checks.config.webhooks;
    if !webhooks.allows(&url) {
        return Err(ApiError::new(
            ErrorCode::WebhookOriginNotAllowed,
            format!(
                "{origin} is not a receiver the operator listed (webhooks.allowed_origins); \
                 Kage posts from its own address to nothing else"
            ),
        )
        .with("field", FIELD)
        .with("origin", origin));
    }
    // A receiver the operator said is its own may sit where the guard lets
    // nobody else go; every other one passes the guard as a target does.
    if !webhooks.is_private(&url) {
        guard(&url, checks.config.security.ssrf(), checks.resolver)
            .await
            .map_err(|denial| {
                let refused = url_refused(&denial);
                ApiError::new(refused.code(), refused.detail()).with("field", FIELD)
            })?;
    }
    let length = webhook.secret.len();
    if !(MIN_WEBHOOK_SECRET..=MAX_WEBHOOK_SECRET).contains(&length) {
        return Err(ApiError::field(
            "webhook.secret",
            format!("a secret of {MIN_WEBHOOK_SECRET} to {MAX_WEBHOOK_SECRET} bytes"),
        ));
    }
    if webhook.progress_every == Some(0) {
        return Err(ApiError::field("webhook.progress_every", "at least 1"));
    }
    Ok(PlannedWebhook {
        url,
        origin,
        secret: webhook.secret.clone(),
        progress_every: webhook.progress_every,
    })
}

/// A JSON value with the members of every object in the order of their
/// names: two bodies that say the same thing are then the same bytes.
pub(crate) fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(members) => {
            let mut sorted: Vec<(&String, &Value)> = members.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(name, value)| (name.clone(), canonical(value)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// The job as it is stored: what was accepted, without the webhook's
/// secret and URL (kept apart, the secret sealed) and, for a batch,
/// without its items (kept as items).
pub(crate) fn stored_spec(request: &JobRequest, items: usize) -> ApiResult<Value> {
    let mut spec = serde_json::to_value(request).map_err(|error| ApiError::internal(&error))?;
    if let Value::Object(members) = &mut spec {
        let progress = request.webhook().and_then(|webhook| webhook.progress_every);
        members.remove("webhook");
        if let Some(every) = progress {
            members.insert("progress_every".to_owned(), json!(every));
        }
        if matches!(request, JobRequest::Batch(_)) {
            members.remove("urls");
            members.remove("items");
            members.insert("item_count".to_owned(), json!(items));
        }
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use kage_browser::contract::FlowAction;
    use kage_core::config::Config;
    use kage_core::domain::SessionId;
    use kage_core::pacing::PacingProfiles;
    use kage_core::policy::{DomainPolicies, PolicySet};
    use serde_json::{Value, json};

    use super::{
        Checks, FlowJob, JobRequest, canonical, plan_batch, plan_flow, plan_webhook, stored_spec,
    };
    use crate::error::{ApiError, ErrorCode};

    const PACING: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../policies/pacing-profiles.yaml"
    ));
    const SIGNATURES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../policies/block-signatures.yaml"
    ));

    fn config(overrides: &[(&str, &str)]) -> Config {
        let base = [
            ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
            ("KAGE__SIDECAR__TOKEN", "token"),
            (
                "KAGE__SECRETS__MASTER_KEY",
                "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
            ),
        ];
        Config::load(
            None,
            base.iter()
                .chain(overrides)
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        )
        .unwrap()
    }

    fn policies(domains: &str) -> PolicySet {
        PolicySet {
            domains: DomainPolicies::from_yaml(domains).unwrap(),
            pacing: PacingProfiles::from_yaml(PACING).unwrap(),
            signatures: kage_core::classifier::SignatureSet::from_yaml(SIGNATURES).unwrap(),
        }
    }

    fn job(body: Value) -> JobRequest {
        serde_json::from_value(body).unwrap()
    }

    async fn batch(body: Value, config: &Config, policies: &PolicySet) -> Result<usize, ApiError> {
        let JobRequest::Batch(batch) = job(body) else {
            panic!("not a batch");
        };
        let checks = Checks {
            config,
            policies,
            resolver: None,
        };
        plan_batch(&batch, checks).await.map(|items| items.len())
    }

    #[tokio::test]
    async fn a_flow_in_a_session_is_never_run_again() {
        let script = |session: Option<&str>| {
            let mut body = json!({
                "type": "flow",
                "actions": [{ "action": "goto", "url": "https://shop.example/" }],
                "max_retries": 3,
            });
            if let Some(session) = session {
                body["session_id"] = json!(session);
            }
            body
        };

        // By itself a flow is run again by another persona after a block,
        // as often as it says.
        let free = flow(script(None), &policies("")).await.unwrap();
        assert_eq!(free.planned.order.max_retries, Some(3));
        // In a session there is no other persona, whatever it says.
        let bound = flow(script(Some("proj-1")), &policies("")).await.unwrap();
        assert_eq!(bound.planned.order.max_retries, Some(0));
        assert_eq!(
            bound.planned.order.session.as_ref().map(SessionId::as_str),
            Some("proj-1")
        );
    }

    async fn flow(body: Value, policies: &PolicySet) -> Result<super::PlannedFlow, ApiError> {
        let config = config(&[]);
        let JobRequest::Flow(flow) = job(body) else {
            panic!("not a flow");
        };
        plan_flow(
            &flow,
            Checks {
                config: &config,
                policies,
                resolver: None,
            },
        )
        .await
    }

    #[tokio::test]
    async fn the_items_of_a_batch_are_fetches_with_the_defaults_in_them() {
        let config = config(&[]);
        let policies = policies("");
        let JobRequest::Batch(request) = job(json!({
            "type": "batch",
            "urls": ["https://shop.example/a", "https://Other.example/b"],
            "items": [{ "url": "https://shop.example/c", "include": ["dom", "screenshot"] }],
            "defaults": { "include": ["dom"], "behavior": { "scroll_depth": 0.3 } },
        })) else {
            panic!("not a batch");
        };
        let checks = Checks {
            config: &config,
            policies: &policies,
            resolver: None,
        };

        let items = plan_batch(&request, checks).await.unwrap();

        assert_eq!(items.len(), 3);
        // In the order given, urls first; the site is the host.
        assert_eq!(
            items
                .iter()
                .map(|item| item.site.as_str())
                .collect::<Vec<_>>(),
            ["shop.example", "other.example", "shop.example"]
        );
        // Stored whole: the defaults are in every item, and an item's own
        // value takes the default's place.
        assert_eq!(items[0].input["url"], "https://shop.example/a");
        assert_eq!(items[0].input["include"], json!(["dom"]));
        assert_eq!(items[0].input["behavior"]["scroll_depth"], 0.3);
        assert_eq!(items[2].input["include"], json!(["dom", "screenshot"]));
        assert_eq!(items[2].input["behavior"]["scroll_depth"], 0.3);
        assert!(items[2].planned.order.capture.screenshot.is_some());
        assert!(items[0].planned.order.capture.screenshot.is_none());
    }

    #[tokio::test]
    async fn an_item_is_refused_for_what_a_fetch_is_refused_for_and_says_which() {
        let config = config(&[("KAGE__JOBS__MAX_ITEMS", "3")]);
        let policies = policies("");
        let refused = |body: Value| {
            let (config, policies) = (config.clone(), policies.clone());
            async move { batch(body, &config, &policies).await.unwrap_err() }
        };

        // The guard, the header denylist, the render rules: a fetch's.
        let denied = refused(json!({
            "type": "batch",
            "urls": ["https://shop.example/a", "http://169.254.169.254/latest"],
        }))
        .await;
        assert_eq!(denied.code(), ErrorCode::SsrfDenied);
        assert_eq!(denied.problem().extensions["item"], 1);
        let header = refused(json!({
            "type": "batch",
            "items": [{ "url": "https://shop.example/a", "render": "http",
                        "headers": { "X-Forwarded-For": "203.0.113.7" } }],
        }))
        .await;
        assert_eq!(header.code(), ErrorCode::HeaderDenied);
        assert_eq!(header.problem().extensions["header"], "X-Forwarded-For");
        assert_eq!(header.problem().extensions["item"], 0);
        let render = refused(json!({
            "type": "batch",
            "urls": ["https://shop.example/a"],
            "defaults": { "method": "POST" },
        }))
        .await;
        assert_eq!(render.code(), ErrorCode::RenderNeedsHttp);
        let unknown = refused(json!({
            "type": "batch",
            "items": [{ "url": "https://shop.example/a", "locale": "de" }],
        }))
        .await;
        assert_eq!(unknown.code(), ErrorCode::ValidationFailed);
        assert_eq!(
            refused(json!({ "type": "batch", "items": ["https://shop.example/a"] }))
                .await
                .code(),
            ErrorCode::ValidationFailed
        );

        // What is the job's is not an item's or a default's.
        for body in [
            json!({ "type": "batch", "items": [{ "url": "https://shop.example/a", "session_id": "s" }] }),
            json!({ "type": "batch", "urls": ["https://shop.example/a"], "defaults": { "session_id": "s" } }),
            json!({ "type": "batch", "urls": ["https://shop.example/a"], "defaults": { "url": "https://x.example/" } }),
        ] {
            assert_eq!(refused(body).await.code(), ErrorCode::ValidationFailed);
        }
        // No items, and more than a job may have.
        assert_eq!(
            refused(json!({ "type": "batch" })).await.code(),
            ErrorCode::ValidationFailed
        );
        let many = refused(json!({
            "type": "batch",
            "urls": ["https://a.example/", "https://b.example/", "https://c.example/", "https://d.example/"],
        }))
        .await;
        assert_eq!(many.problem().extensions["maximum"], 3);
    }

    #[tokio::test]
    async fn a_batch_may_ask_for_as_many_visitors_as_the_pacing_allows_and_no_more() {
        let config = config(&[]);
        let human = policies("");
        let urls = json!(["https://shop.example/a", "https://shop.example/b"]);

        for visitors in [1, 3] {
            let body = json!({ "type": "batch", "urls": urls, "visitors": visitors });
            assert_eq!(batch(body, &config, &human).await.unwrap(), 2);
        }
        let refused = batch(
            json!({ "type": "batch", "urls": urls, "visitors": 4 }),
            &config,
            &human,
        )
        .await
        .unwrap_err();
        // Refused with the limit, not reduced to it.
        assert_eq!(refused.code(), ErrorCode::ValidationFailed);
        let problem = refused.problem();
        assert_eq!(problem.extensions["field"], "visitors");
        assert_eq!(problem.extensions["maximum"], 3);
        assert_eq!(problem.extensions["site"], "shop.example");
        assert_eq!(
            batch(
                json!({ "type": "batch", "urls": urls, "visitors": 0 }),
                &config,
                &human
            )
            .await
            .unwrap_err()
            .code(),
            ErrorCode::ValidationFailed
        );
        // A faster pacing allows more, where the policy does not lock it.
        let fast = json!({
            "type": "batch", "urls": urls, "visitors": 8,
            "defaults": { "pacing": { "profile": "fast" } },
        });
        assert_eq!(batch(fast.clone(), &config, &human).await.unwrap(), 2);
        let locked = policies("defaults:\n  pacing_locked: true\n");
        assert_eq!(
            batch(fast, &config, &locked)
                .await
                .unwrap_err()
                .problem()
                .extensions["maximum"],
            3
        );
    }

    #[tokio::test]
    async fn a_flow_becomes_one_browser_attempt_with_its_steps_as_the_sidecar_takes_them() {
        let planned = flow(
            json!({
                "type": "flow",
                "actions": [
                    { "action": "goto", "url": "https://shop.example/start" },
                    { "action": "click", "selector": "#next", "timeout_ms": 5000 },
                    { "action": "wait_for", "url": "**/listing?page=2" },
                    { "action": "type", "selector": "#q", "text": "blue" },
                    { "action": "press", "key": "Enter" },
                    { "action": "scroll", "depth": 0.5 },
                    { "action": "select", "selector": "#sort", "value": "price" },
                    { "action": "hover", "selector": "#menu" },
                    { "action": "back" },
                    { "action": "assert", "text_contains": "Listing" },
                    { "action": "extract", "include": ["dom", "xhr"] },
                    { "action": "screenshot", "options": { "full_page": true, "format": "png" } },
                    { "action": "pdf" },
                ],
                "include": ["dom", "headers", "screenshot"],
                "behavior": { "dwell_ms": 3000, "typos": true },
                "persona": { "engines": ["firefox"] },
                "timeout_ms": 240_000,
            }),
            &policies(""),
        )
        .await
        .unwrap();

        let order = &planned.planned.order;
        assert_eq!(planned.site, "shop.example");
        assert_eq!(order.url.as_str(), "https://shop.example/start");
        assert_eq!(order.render, kage_core::domain::RenderMode::Browser);
        assert_eq!(order.timeout, Some(std::time::Duration::from_secs(240)));
        assert_eq!(order.engines, [kage_core::domain::Engine::Firefox]);
        assert_eq!(
            order.behavior.unwrap().dwell,
            Some(std::time::Duration::from_secs(3))
        );
        assert!(order.capture.screenshot.is_some() && order.keep.screenshot);
        let plan = order.flow.as_ref().unwrap();
        assert!(plan.typos);
        assert_eq!(plan.actions.len(), 13);
        assert!(matches!(
            &plan.actions[0],
            FlowAction::Goto { url, human: None, .. } if url == "https://shop.example/start"
        ));
        assert!(matches!(
            &plan.actions[1],
            FlowAction::Click { selector, timeout_ms: Some(5000), human: None } if selector == "#next"
        ));
        assert!(matches!(
            &plan.actions[2],
            FlowAction::WaitFor { url: Some(url), selector: None, ms: None, .. }
                if url == "**/listing?page=2"
        ));
        assert!(matches!(
            &plan.actions[8],
            FlowAction::Back { human: None, .. }
        ));
        // The capturing steps say what they keep; the others keep nothing.
        let keeps: Vec<bool> = planned.keeps.iter().map(Option::is_some).collect();
        assert_eq!(
            keeps,
            [
                false, false, false, false, false, false, false, false, false, false, true, true,
                true
            ]
        );
        let extract = planned.keeps[10].unwrap();
        assert!(extract.dom && extract.xhr && !extract.screenshot);
        assert!(planned.keeps[11].unwrap().screenshot);
        assert!(planned.keeps[12].unwrap().pdf);
        assert!(matches!(
            &plan.actions[10],
            FlowAction::Extract { capture, .. } if capture.dom && capture.xhr.is_some()
        ));
        // Three minutes when the flow does not say.
        let unhurried = flow(
            json!({ "type": "flow", "actions": [{ "action": "goto", "url": "https://shop.example/" }] }),
            &policies(""),
        )
        .await
        .unwrap();
        assert_eq!(
            unhurried.planned.order.timeout,
            Some(std::time::Duration::from_secs(180))
        );
        assert!(!unhurried.planned.order.flow.unwrap().typos);
    }

    #[tokio::test]
    async fn a_flow_that_cannot_run_as_asked_is_refused_and_says_which_step() {
        let open = policies("");
        let refused = |body: Value| {
            let policies = open.clone();
            async move { flow(body, &policies).await.unwrap_err() }
        };
        let goto = json!({ "action": "goto", "url": "https://shop.example/" });

        // It starts with a goto, and has steps.
        let first = refused(
            json!({ "type": "flow", "actions": [{ "action": "click", "selector": "#a" }] }),
        )
        .await;
        assert_eq!(first.code(), ErrorCode::ValidationFailed);
        assert_eq!(first.problem().extensions["step"], 0);
        assert_eq!(
            refused(json!({ "type": "flow", "actions": [] }))
                .await
                .code(),
            ErrorCode::ValidationFailed
        );
        // Every page it is sent to passes the guard.
        let denied = refused(json!({
            "type": "flow",
            "actions": [goto, { "action": "goto", "url": "http://10.0.0.5/admin" }],
        }))
        .await;
        assert_eq!(denied.code(), ErrorCode::SsrfDenied);
        let problem = denied.problem();
        assert_eq!(problem.extensions["step"], 1);
        assert_eq!(problem.extensions["field"], "actions[1].url");
        assert_eq!(
            refused(json!({ "type": "flow", "actions": [goto, { "action": "goto", "url": "not a url" }] }))
                .await
                .code(),
            ErrorCode::InvalidUrl
        );
        // Conditions that are one of several.
        for step in [
            json!({ "action": "wait_for" }),
            json!({ "action": "wait_for", "selector": "#a", "ms": 5 }),
            json!({ "action": "wait_for", "ms": 500_000 }),
            json!({ "action": "assert" }),
            json!({ "action": "assert", "selector": "#a", "text_contains": "b" }),
            json!({ "action": "click", "selector": "" }),
            json!({ "action": "scroll", "depth": 1.5 }),
            json!({ "action": "press", "key": "" }),
            json!({ "action": "click", "selector": "#a", "timeout_ms": 5 }),
            json!({ "action": "extract", "include": ["screenshot"] }),
            json!({ "action": "extract", "include": ["extract"] }),
        ] {
            let error = refused(json!({ "type": "flow", "actions": [goto, step] })).await;
            assert!(
                matches!(
                    error.code(),
                    ErrorCode::ValidationFailed
                        | ErrorCode::NotAvailableYet
                        | ErrorCode::RenderIncludeConflict
                ),
                "{step}: {error}"
            );
            assert_eq!(error.problem().extensions["step"], 1, "{step}");
        }
        // The whole flow's time, and what it shares with a fetch.
        assert_eq!(
            refused(json!({ "type": "flow", "actions": [goto], "timeout_ms": 5000 }))
                .await
                .code(),
            ErrorCode::TimeoutTooShort
        );
        assert_eq!(
            refused(json!({ "type": "flow", "actions": [goto], "timeout_ms": 900_000 }))
                .await
                .code(),
            ErrorCode::ValidationFailed
        );
        assert_eq!(
            refused(
                json!({ "type": "flow", "actions": [goto], "behavior": { "dwell_ms": 900_000 } })
            )
            .await
            .code(),
            ErrorCode::ValidationFailed
        );
    }

    #[tokio::test]
    async fn a_step_done_by_the_driver_is_refused_unless_the_sites_policy_allows_it() {
        let body = json!({
            "type": "flow",
            "actions": [
                { "action": "goto", "url": "https://shop.example/" },
                { "action": "click", "selector": "#a" },
                { "action": "back", "human": false },
            ],
        });

        let denied = flow(body.clone(), &policies("")).await.unwrap_err();
        assert_eq!(denied.code(), ErrorCode::DriverInputDenied);
        let problem = denied.problem();
        assert_eq!(problem.extensions["step"], 2);
        assert_eq!(problem.extensions["field"], "actions[2].human");
        assert_eq!(problem.extensions["site"], "shop.example");

        // Where the operator said the site does not look, it is taken as
        // written; `human: true` is what a step is anyway.
        let allowed = policies("domains:\n  shop.example:\n    driver_input: true\n");
        let planned = flow(body, &allowed).await.unwrap();
        let actions = planned.planned.order.flow.unwrap().actions;
        assert!(matches!(
            actions[2],
            FlowAction::Back {
                human: Some(false),
                ..
            }
        ));
        let explicit = json!({
            "type": "flow",
            "actions": [
                { "action": "goto", "url": "https://shop.example/" },
                { "action": "click", "selector": "#a", "human": true },
            ],
        });
        assert!(flow(explicit, &policies("")).await.is_ok());
        // The steps that send no input have no such switch at all.
        let unknown = serde_json::from_value::<FlowJob>(json!({
            "actions": [{ "action": "goto", "url": "https://shop.example/", "human": false }],
        }));
        assert!(unknown.is_err());
    }

    #[tokio::test]
    async fn a_webhook_goes_only_to_a_receiver_the_operator_listed() {
        let listed = config(&[
            (
                "KAGE__WEBHOOKS__ALLOWED_ORIGINS",
                "[\"https://hooks.example.org\", \"http://app:3000\", \"https://10.0.0.9\"]",
            ),
            ("KAGE__WEBHOOKS__PRIVATE_ORIGINS", "[\"http://app:3000\"]"),
        ]);
        let none = config(&[]);
        let policies = policies("");
        let check = |config: &Config, url: &str, secret: &str| {
            let (config, policies) = (config.clone(), policies.clone());
            let field = super::WebhookField {
                url: url.to_owned(),
                secret: secret.to_owned(),
                progress_every: Some(10),
            };
            async move {
                plan_webhook(
                    &field,
                    Checks {
                        config: &config,
                        policies: &policies,
                        resolver: None,
                    },
                )
                .await
            }
        };
        let secret = "a secret of sixteen bytes or more";

        let public = check(&listed, "https://hooks.example.org/kage?t=1", secret)
            .await
            .unwrap();
        assert_eq!(public.origin, "https://hooks.example.org:443");
        assert_eq!(public.progress_every, Some(10));
        // The operator's own receiver may be where nobody else may go.
        let private = check(&listed, "http://app:3000/hooks", secret)
            .await
            .unwrap();
        assert_eq!(private.origin, "http://app:3000");

        // Nothing is listed by default, so no job has a webhook.
        let unlisted = check(&none, "https://hooks.example.org/kage", secret)
            .await
            .unwrap_err();
        assert_eq!(unlisted.code(), ErrorCode::WebhookOriginNotAllowed);
        assert_eq!(
            unlisted.problem().extensions["origin"],
            "https://hooks.example.org:443"
        );
        // Another origin than the listed one: a target, say.
        for url in [
            "https://target.example/hook",
            "http://hooks.example.org/kage",
            "https://hooks.example.org:8443/kage",
            "http://app:3001/hooks",
        ] {
            assert_eq!(
                check(&listed, url, secret).await.unwrap_err().code(),
                ErrorCode::WebhookOriginNotAllowed,
                "{url}"
            );
        }
        // Listed, but not as the operator's own: the guard holds.
        assert_eq!(
            check(&listed, "https://10.0.0.9/hook", secret)
                .await
                .unwrap_err()
                .code(),
            ErrorCode::SsrfDenied
        );
        // Not a URL Kage posts to at all.
        for url in [
            "hooks.example.org/kage",
            "ftp://hooks.example.org/kage",
            "https://user:pw@hooks.example.org/kage",
        ] {
            assert_eq!(
                check(&listed, url, secret).await.unwrap_err().code(),
                ErrorCode::InvalidUrl,
                "{url}"
            );
        }
        // The secret has a length.
        for secret in ["short", &"x".repeat(300)] {
            let refused = check(&listed, "https://hooks.example.org/kage", secret)
                .await
                .unwrap_err();
            assert_eq!(refused.problem().extensions["field"], "webhook.secret");
        }
    }

    #[test]
    fn a_job_is_stored_without_its_secret_and_hashed_whatever_the_order_of_its_members() {
        let request = job(json!({
            "type": "batch",
            "urls": ["https://shop.example/a"],
            "visitors": 2,
            "webhook": { "url": "https://hooks.example.org/kage?token=t", "secret": "a secret of sixteen bytes or more",
                         "progress_every": 5 },
        }));

        let spec = stored_spec(&request, 1).unwrap();

        let text = spec.to_string();
        assert!(
            !text.contains("a secret of sixteen bytes or more")
                && !text.contains("hooks.example.org")
        );
        assert_eq!(spec["type"], "batch");
        assert_eq!(spec["visitors"], 2);
        assert_eq!(spec["progress_every"], 5);
        assert_eq!(spec["item_count"], 1);
        assert!(spec.get("urls").is_none());
        assert_eq!(request.webhook().unwrap().progress_every, Some(5));
        assert_eq!(request.session_id(), None);
        // A flow keeps its steps: they are what is run.
        let flow = job(json!({
            "type": "flow",
            "session_id": "s-1",
            "actions": [{ "action": "goto", "url": "https://shop.example/" }],
        }));
        assert_eq!(
            stored_spec(&flow, 2).unwrap()["actions"][0]["action"],
            "goto"
        );
        assert_eq!(flow.session_id(), Some("s-1"));

        let one: Value =
            serde_json::from_str(r#"{"b":{"y":1,"x":[{"k":2,"a":1}]},"a":true}"#).unwrap();
        let other: Value =
            serde_json::from_str(r#"{"a":true,"b":{"x":[{"a":1,"k":2}],"y":1}}"#).unwrap();
        assert_eq!(canonical(&one).to_string(), canonical(&other).to_string());
        assert_eq!(
            canonical(&one).to_string(),
            r#"{"a":true,"b":{"x":[{"a":1,"k":2}],"y":1}}"#
        );
    }
}
