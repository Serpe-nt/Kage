//! One attempt, on the http path or in the persona's browser (spec §8,
//! §9, §10).
//!
//! The scheduler decides who makes an attempt and when; an
//! [`AttemptExecutor`] makes it and says what the site answered.
//! [`PathExecutor`] is the real one.
//!
//! **Http path.** The persona's own client (its profile, its languages,
//! its cookie jar) fetches through the persona's egress. A later page of a
//! visit carries the persona's previous page on that site as `Referer`
//! when the policy's entry is `direct` and the caller gave none.
//!
//! **Browser path.** The persona's context is leased from the one owner of
//! contexts, the cookies its http jar holds for the origin are handed to
//! the browser, and the page is loaded with the behavior the domain policy
//! asks for: a human dwell, a scroll depth drawn from the attempt's seed,
//! a wandering pointer, and the policy's entry. A request may shorten that
//! ([`BehaviorWish`]) inside what the policy allows: less scrolling or
//! none, and a shorter dwell that is drawn anew for every attempt and
//! never falls below the policy's `min_dwell`. The pointer always wanders,
//! and a policy with `behavior_locked` ignores the request altogether. A detected interstitial is
//! given `browser.challenge_wait_ms` to resolve by itself. The redirect
//! chain is checked against the SSRF guard afterwards. After a page the
//! site served, the origin's cookies are handed to the http jar and the
//! browser stays open; after a block nothing is handed over and the
//! browser is closed: the persona leaves.
//!
//! **A flow** (spec §10.5) is a browser attempt that runs a script of steps
//! instead of loading one page: one visit by one persona, in its context,
//! under one grant. The sidecar is asked to pace it like a person (a dwell
//! with a wandering pointer after every page it arrives at, never below the
//! policy's `min_dwell`, and a drawn pause between its actions). Every
//! capture the flow brings back is checked against the SSRF guard like a
//! navigation's chain. The page it ends on is classified like any page: a
//! wall is a blocked attempt; a step that failed on a page that is no wall
//! is the script's failure, which no other persona would change.
//!
//! Both paths end at the classifier with the signatures in force. A
//! success postpones the next health probe of the persona's proxy.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use kage_browser::contract::{
    BehaviorOptions, CaptureOptions, DwellMode, Entry, EntryStrategy, FlowAction, FlowBehavior,
    FlowEvent, FlowPace, FlowRequest, NavigateRequest, PageResult, Wait, WaitUntil,
};
use kage_browser::{Navigation, ssrf};
use kage_core::classifier::{Classification, Observation};
use kage_core::config::{BrowserConfig, Config};
use kage_core::domain::{Header, Persona, Render, Timing};
use kage_core::ids::{PersonaId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::pacing::Lognormal;
use kage_core::policy::{EffectivePolicy, RefererStrategy};
use kage_core::schedule::politeness::Courtesy;
use kage_core::seed::Seed;
use kage_core::ssrf::SsrfPolicy;
use kage_http::{FetchSpec, Fetched, Guard, HttpError, RequestKind};
use kage_persona::identity::http_spec;
use kage_persona::{PersonaError, PersonaManager, Via};
use url::Url;

use crate::forwarder::Meter;
use crate::verdicts::{Verdicts, cookies_set, courtesy_of, page_failure, unanswered};

/// A scripted flow a browser attempt runs in place of loading one page
/// (spec §10.5).
#[derive(Debug, Clone, PartialEq)]
pub struct FlowPlan {
    /// The steps, as the sidecar takes them. The first is a `goto`.
    pub actions: Vec<FlowAction>,
    /// Whether typed text gets the occasional typo.
    pub typos: bool,
}

/// One finished step of a flow.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowStep {
    /// Its position in the script.
    pub index: u32,
    /// Its action's name.
    pub action: String,
    /// Whether it succeeded.
    pub ok: bool,
    /// How long it took, its pacing included.
    pub duration_ms: u64,
    /// How much of that was the persona's dwell and thinking time.
    pub paced_ms: Option<u64>,
    /// Why it failed.
    pub error: Option<String>,
    /// What an `extract`, `screenshot` or `pdf` step captured.
    pub capture: Option<Box<PageResult>>,
}

/// What a flow came to: the steps that ran, in order. The page it ended on
/// is the attempt's page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FlowRun {
    /// The steps that ran; a flow ends at the first that fails.
    pub steps: Vec<FlowStep>,
    /// Whether every step succeeded.
    pub ok: bool,
}

impl FlowRun {
    /// The step that ended the flow, when one failed.
    pub fn failed(&self) -> Option<&FlowStep> {
        self.steps.iter().find(|step| !step.ok)
    }
}

/// What a failed step of a flow means for the attempt.
struct StepFailure {
    outcome: Outcome,
    reason: &'static str,
    courtesy: Courtesy,
    retryable: bool,
}

/// Reads what a failed step means from its error. A `goto` that did not
/// arrive names the navigation error the sidecar classified; everything
/// else (a selector that is not there, an assertion, the step's own time)
/// is the script's, and another persona would not change it.
fn step_failure(error: &str) -> StepFailure {
    let code = error.split(':').next().unwrap_or_default();
    match code {
        "timeout" => StepFailure {
            outcome: Outcome::Timeout,
            reason: "timeout",
            courtesy: Courtesy::ServerError,
            retryable: true,
        },
        "proxy_failed" => StepFailure {
            outcome: Outcome::ProviderError,
            reason: "proxy_failed",
            courtesy: Courtesy::NotTheSite,
            retryable: true,
        },
        "navigation_failed" => StepFailure {
            outcome: Outcome::ProviderError,
            reason: "connection_failed",
            courtesy: Courtesy::ServerError,
            retryable: true,
        },
        _ => StepFailure {
            outcome: Outcome::ProviderError,
            reason: "flow_step_failed",
            courtesy: Courtesy::Answered,
            retryable: false,
        },
    }
}

/// A browser attempt that did not come to a page: what it ended with. Its
/// context is closed.
type Failed = Box<AttemptOutput>;

/// How much longer than its own budget a browser attempt is waited for:
/// the sidecar answers when the budget ran out, a little after it.
const BROWSER_GRACE: Duration = Duration::from_secs(20);

/// The longest dwell a request can fix: the contract's own limit.
const MAX_FIXED_DWELL: Duration = Duration::from_secs(120);

/// What a request asks of a persona's behavior on a browser page (spec
/// §16.4). What it leaves out is the domain policy's.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BehaviorWish {
    /// How far down the page to scroll, from 0 to 1; 0 for no scrolling.
    pub scroll_depth: Option<f64>,
    /// The median of the time spent on the page after it loaded.
    pub dwell: Option<Duration>,
    /// How the page is arrived at.
    pub referer_strategy: Option<RefererStrategy>,
}

impl BehaviorWish {
    /// Whether the request asks for anything at all.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// The behavior a browser attempt was asked for, and whose it was.
#[derive(Debug, Clone, PartialEq)]
pub struct BehaviorApplied {
    /// Whether any of it was the request's.
    pub from_request: bool,
    /// Whether the request asked for behavior that the policy's lock
    /// overrode.
    pub request_ignored: bool,
    /// How far the page is scrolled; `None` for no scrolling.
    pub scroll_depth: Option<f64>,
    /// The dwell drawn for this attempt; `None` for the persona's own
    /// human dwell, which the sidecar draws.
    pub dwell: Option<Duration>,
    /// How the page is arrived at.
    pub referer_strategy: RefererStrategy,
    /// The seed of the attempt, from which all of it was drawn.
    pub seed: Seed,
}

/// What one attempt is asked to do.
#[derive(Debug, Clone)]
pub struct AttemptJob {
    /// The request the attempt belongs to.
    pub request: RequestId,
    /// 1 for the first attempt, 2 for the second, and so on.
    pub n: u32,
    /// The persona that makes it.
    pub persona: Persona,
    /// The path.
    pub render: Render,
    /// The page.
    pub url: Url,
    /// `GET`, `HEAD` or `POST`; a browser attempt is always a navigation.
    pub method: http::Method,
    /// What the request is on the http path: a navigation, a `fetch()` or
    /// an `XMLHttpRequest`.
    pub kind: RequestKind,
    /// What the request asks of the behavior on a browser page.
    pub behavior: Option<BehaviorWish>,
    /// The caller's headers (http path).
    pub headers: Vec<(String, String)>,
    /// The request body (http path).
    pub body: Option<Bytes>,
    /// The host: the key of policy and pacing.
    pub domain: String,
    /// The seed of the attempt: the behavior of a browser attempt and the
    /// pauses of an http attempt are drawn from it and the persona's own.
    pub seed: Seed,
    /// Longest the attempt may take.
    pub timeout: Duration,
    /// The domain's policy.
    pub policy: EffectivePolicy,
    /// Whether this page starts a visit of the persona to the domain.
    pub starts_visit: bool,
    /// Whether the persona is still visiting the domain after this page.
    pub keeps_visiting: bool,
    /// What a browser attempt captures besides the document.
    pub capture: CaptureOptions,
    /// What a browser attempt waits for.
    pub wait: Wait,
    /// The largest body kept.
    pub max_body_bytes: Option<u64>,
    /// The flow a browser attempt runs in place of loading `url`, which
    /// is then where the flow starts.
    pub flow: Option<FlowPlan>,
}

/// What an attempt came to.
#[derive(Debug, Clone)]
pub struct AttemptOutput {
    /// The classifier's verdict, or the failure in its shape.
    pub classification: Classification,
    /// What the end says about the site's health.
    pub courtesy: Courtesy,
    /// Why the attempt ended as it did when the site did not say.
    pub reason: Option<String>,
    /// The status of the final response, when one arrived.
    pub status: Option<u16>,
    /// The URL after redirects, when a response arrived.
    pub final_url: Option<String>,
    /// The headers of the final response.
    pub headers: Vec<Header>,
    /// Every URL visited before the final one, in order.
    pub redirects: Vec<String>,
    /// Time per phase.
    pub timing: Timing,
    /// The page as the browser returned it (browser path).
    pub page: Option<Box<PageResult>>,
    /// What the http path fetched (http path).
    pub fetched: Option<Box<Fetched>>,
    /// Cookies handed from the http jar to the browser before the page.
    pub cookies_to_browser: Option<usize>,
    /// Cookies handed from the browser to the http jar after the page.
    pub cookies_to_http: Option<usize>,
    /// The behavior the browser was asked for (browser path).
    pub behavior: Option<BehaviorApplied>,
    /// The steps of the flow, when the attempt ran one.
    pub flow: Option<FlowRun>,
}

impl AttemptOutput {
    /// An attempt that ended without the site's answer.
    pub fn failed(outcome: Outcome, reason: &str, courtesy: Courtesy) -> Self {
        Self {
            classification: unanswered(outcome, reason),
            courtesy,
            reason: Some(reason.to_owned()),
            status: None,
            final_url: None,
            headers: Vec::new(),
            redirects: Vec::new(),
            timing: Timing::default(),
            page: None,
            fetched: None,
            cookies_to_browser: None,
            cookies_to_http: None,
            behavior: None,
            flow: None,
        }
    }

    /// What a page a browser returned came to, as the classifier judged
    /// it: for a capture that is no attempt of its own (a flow's capture
    /// point).
    pub fn of_page(page: Box<PageResult>, classification: Classification) -> Self {
        let status = page.status.and_then(|status| u16::try_from(status).ok());
        Self {
            courtesy: courtesy_of(classification.outcome, status),
            classification,
            reason: None,
            status,
            final_url: Some(page.final_url.clone()),
            headers: page
                .headers
                .iter()
                .map(|header| Header {
                    name: header.name.clone(),
                    value: header.value.clone(),
                })
                .collect(),
            redirects: page.redirects.iter().map(|hop| hop.url.clone()).collect(),
            timing: timing_of(&page),
            page: Some(page),
            fetched: None,
            cookies_to_browser: None,
            cookies_to_http: None,
            behavior: None,
            flow: None,
        }
    }

    /// The same failure, but one no other persona would change.
    #[must_use]
    pub fn finally(mut self) -> Self {
        self.classification.retryable = false;
        self
    }

    /// The outcome.
    pub const fn outcome(&self) -> Outcome {
        self.classification.outcome
    }
}

/// Makes attempts.
#[async_trait]
pub trait AttemptExecutor: Send + Sync + fmt::Debug {
    /// Makes one attempt and says what it came to. An attempt never fails
    /// as a call: what went wrong is its outcome.
    async fn run(&self, job: &AttemptJob) -> AttemptOutput;
}

/// The dwell of one attempt when a request names its median: drawn from
/// the attempt's seed as a log-normal around that median (its 95th
/// percentile at twice the median), never below `floor` and never the
/// same twice. A draw below the floor is drawn again, not raised to it:
/// a pile of pages left after exactly the floor would be a signature of
/// its own.
pub fn draw_dwell(median: Duration, floor: Duration, seed: &Seed) -> Duration {
    let cap = MAX_FIXED_DWELL.as_secs_f64() * 1000.0;
    let floor_ms = (floor.as_secs_f64() * 1000.0).min(cap / 2.0);
    let median_ms = (median.as_secs_f64() * 1000.0)
        .max(floor_ms)
        .max(1.0)
        .min(cap / 2.0);
    let shape = Lognormal {
        median: median_ms,
        p95: 2.0 * median_ms,
        min: None,
        max: Some(cap),
    };
    let mut rng = seed.child("dwell").rng();
    // At least half of the draws are above the floor.
    for _ in 0..64 {
        let value = shape.sample(&mut rng);
        if value >= floor_ms {
            return Duration::from_secs_f64(value / 1000.0);
        }
    }
    floor
}

/// The behavior of a browser on a page (spec §10.4), as the sidecar is
/// asked for it and as the result reports it.
///
/// Without a request's wish it is the domain policy's: a human dwell, a
/// scroll depth drawn from the attempt's seed inside the configured range,
/// a wandering pointer, and the policy's entry. A wish changes the scroll
/// depth, the dwell ([`draw_dwell`]) and the entry, unless the policy
/// locks behavior. The pointer wanders either way.
pub fn behavior_for(
    policy: &EffectivePolicy,
    browser: &BrowserConfig,
    seed: &Seed,
    wish: Option<&BehaviorWish>,
) -> (BehaviorOptions, BehaviorApplied) {
    let asked = wish.filter(|wish| !wish.is_empty());
    let wish = asked.filter(|_| !policy.behavior_locked);
    let scroll_depth = match wish.and_then(|wish| wish.scroll_depth) {
        Some(depth) if depth <= 0.0 => None,
        Some(depth) => Some(depth.min(1.0)),
        None => Some(
            seed.child("scroll-depth")
                .rng()
                .uniform(browser.scroll_depth.min, browser.scroll_depth.max),
        ),
    };
    let dwell = wish
        .and_then(|wish| wish.dwell)
        .map(|median| draw_dwell(median, policy.min_dwell.get(), seed));
    let referer_strategy = wish
        .and_then(|wish| wish.referer_strategy)
        .unwrap_or(policy.referer_strategy);
    let options = BehaviorOptions {
        dwell: Some(if dwell.is_some() {
            DwellMode::Fixed
        } else {
            DwellMode::Human
        }),
        entry: Some(Entry {
            fallback: None,
            from_url: None,
            strategy: match referer_strategy {
                RefererStrategy::None => EntryStrategy::None,
                // A search entry needs a results URL built from a
                // template, and none is configured: a typed navigation.
                RefererStrategy::Direct | RefererStrategy::SearchEngine => EntryStrategy::Direct,
            },
        }),
        scroll_depth,
        seed: Some(seed.to_string()),
        wander: Some(true),
    };
    let applied = BehaviorApplied {
        from_request: wish.is_some(),
        request_ignored: asked.is_some() && wish.is_none(),
        scroll_depth,
        dwell,
        referer_strategy,
        seed: *seed,
    };
    (options, applied)
}

/// The navigation of one browser attempt, and the behavior it asks for.
pub fn navigation_for(
    job: &AttemptJob,
    browser: &BrowserConfig,
) -> (NavigateRequest, BehaviorApplied) {
    let (behavior, applied) = behavior_for(&job.policy, browser, &job.seed, job.behavior.as_ref());
    let request = NavigateRequest {
        artifact_prefix: None,
        behavior: Some(behavior),
        capture: CaptureOptions {
            // The classifier reads the document, whatever the caller wants.
            dom: true,
            ..job.capture.clone()
        },
        challenge_wait_ms: Some(browser.challenge_wait_ms),
        dwell_ms: applied
            .dwell
            .and_then(|dwell| u64::try_from(dwell.as_millis()).ok()),
        max_body_bytes: job.max_body_bytes,
        timeout_ms: u64::try_from(job.timeout.as_millis()).ok(),
        url: job.url.to_string(),
        wait: job.wait.clone(),
    };
    (request, applied)
}

/// The flow of one browser attempt as the sidecar is asked for it, and the
/// behavior it asks for. The sidecar paces the flow like a person: the
/// persona's own dwell after every page it arrives at, or one around the
/// median a request names, never below the policy's `min_dwell`; a policy
/// that locks behavior ignores the request. The page the flow ends on is
/// always captured with its document, which the classifier reads.
pub fn flow_for(
    job: &AttemptJob,
    plan: &FlowPlan,
    browser: &BrowserConfig,
) -> (FlowRequest, BehaviorApplied) {
    let asked = job.behavior.as_ref().filter(|wish| !wish.is_empty());
    let wish = asked.filter(|_| !job.policy.behavior_locked);
    let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    let floor = job.policy.min_dwell.get();
    let dwell = wish
        .and_then(|wish| wish.dwell)
        .map(|median| median.max(floor));
    let request = FlowRequest {
        actions: plan.actions.clone(),
        behavior: Some(FlowBehavior {
            pace: Some(FlowPace {
                dwell_ms: dwell.map(millis),
                min_dwell_ms: Some(millis(floor)),
            }),
            seed: Some(job.seed.to_string()),
            typos: Some(plan.typos),
        }),
        capture: CaptureOptions {
            dom: true,
            ..job.capture.clone()
        },
        timeout_ms: Some(millis(job.timeout)),
    };
    // A flow scrolls where its steps say; the entry is a typed navigation.
    let _ = browser;
    let applied = BehaviorApplied {
        from_request: dwell.is_some(),
        request_ignored: asked.is_some() && wish.is_none(),
        scroll_depth: None,
        dwell,
        referer_strategy: RefererStrategy::Direct,
        seed: job.seed,
    };
    (request, applied)
}

/// What a navigation waits for when the request does not say: the load
/// event.
pub const fn default_wait() -> Wait {
    Wait {
        extra_ms: None,
        selector: None,
        until: WaitUntil::Load,
    }
}

/// What a navigation captures when the request does not say: the document.
pub const fn default_capture() -> CaptureOptions {
    CaptureOptions {
        console: false,
        dom: true,
        pdf: None,
        raw: false,
        screenshot: None,
        xhr: None,
    }
}

/// The origin of a URL as the handoff takes it.
fn origin_of(url: &Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

fn timing_of(page: &PageResult) -> Timing {
    Timing {
        dns_ms: None,
        connect_ms: None,
        tls_ms: None,
        ttfb_ms: page.timing.ttfb_ms,
        dom_content_loaded_ms: page.timing.dom_content_loaded_ms,
        load_ms: page.timing.load_ms,
        total_ms: page.timing.total_ms,
    }
}

/// The attempt executor over the real paths.
pub struct PathExecutor {
    manager: Arc<PersonaManager>,
    verdicts: Arc<Verdicts>,
    browser: BrowserConfig,
    security: SsrfPolicy,
    max_redirects: usize,
    meter: Option<Arc<Meter>>,
    /// The last page each persona fetched from each domain over the http
    /// path, and when: the `Referer` of its next one.
    last_pages: Mutex<LastPages>,
}

/// The last page of each persona on each domain, and when it was fetched.
type LastPages = HashMap<(PersonaId, String), (Url, DateTime<Utc>)>;

impl fmt::Debug for PathExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PathExecutor")
            .field("browser", &self.browser)
            .field("security", &self.security)
            .finish_non_exhaustive()
    }
}

/// How a failure of the persona's own plumbing is named.
fn plumbing(error: &PersonaError) -> (&'static str, bool) {
    match error {
        PersonaError::EgressUnsafe => ("egress_unsafe", false),
        PersonaError::ProxyNotUsable { .. } | PersonaError::ProxyNotFound(_) => {
            ("proxy_not_usable", true)
        }
        PersonaError::Retired(_) | PersonaError::NotFound(_) => ("persona_gone", true),
        PersonaError::Browser(browser) if browser.is_context_conflict() => {
            ("context_conflict", true)
        }
        PersonaError::Browser(browser) if browser.is_context_busy() => ("context_busy", true),
        PersonaError::Browser(browser) if browser.is_at_capacity() => ("context_limit", true),
        PersonaError::Browser(browser) if browser.is_unreachable() => ("sidecar_unreachable", true),
        PersonaError::Browser(_) => ("sidecar_error", true),
        PersonaError::Http(_) => ("http_client", true),
        _ => ("persona_error", true),
    }
}

impl PathExecutor {
    /// An executor over the persona manager's contexts, clients and
    /// handoff. With a `meter` the bytes of an attempt count toward its
    /// domain.
    pub fn new(
        manager: Arc<PersonaManager>,
        verdicts: Arc<Verdicts>,
        config: &Config,
        meter: Option<Arc<Meter>>,
    ) -> Self {
        Self {
            manager,
            verdicts,
            browser: config.browser,
            security: config.security.ssrf(),
            max_redirects: usize::try_from(config.http.max_redirects).unwrap_or(usize::MAX),
            meter,
            last_pages: Mutex::new(HashMap::new()),
        }
    }

    fn unavailable(error: &PersonaError) -> AttemptOutput {
        let (reason, retryable) = plumbing(error);
        tracing::warn!(%error, reason, "an attempt could not be made");
        let output = AttemptOutput::failed(Outcome::ProviderError, reason, Courtesy::NotTheSite);
        if retryable { output } else { output.finally() }
    }

    fn last_page(&self, persona: PersonaId, domain: &str) -> Option<Url> {
        self.last_pages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(persona, domain.to_owned()))
            .map(|(url, _)| url.clone())
    }

    fn remember_page(&self, persona: PersonaId, domain: &str, url: Url) {
        let now = self.manager.clock().now();
        let mut pages = self
            .last_pages
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // What nobody came back to for a day is forgotten.
        pages.retain(|_, (_, at)| now.signed_duration_since(*at) < chrono::Duration::days(1));
        pages.insert((persona, domain.to_owned()), (url, now));
    }

    async fn http(&self, job: &AttemptJob) -> AttemptOutput {
        let persona = &job.persona;
        let Ok(spec) = http_spec(persona) else {
            // The planner only sends a persona with an http path; one that
            // lost it meanwhile is another persona's turn.
            return AttemptOutput::failed(
                Outcome::ProviderError,
                "no_http_path",
                Courtesy::NotTheSite,
            );
        };
        let (proxy, endpoint) = match self.manager.egress(persona, Via::Http).await {
            Ok(route) => route,
            Err(error) => return Self::unavailable(&error),
        };
        let client = match self.manager.http().client(&spec, &endpoint).await {
            Ok(client) => client,
            Err(error) => return Self::unavailable(&PersonaError::Http(error)),
        };
        let mut headers = job.headers.clone();
        let has_referer = headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("referer"));
        if !job.starts_visit
            && !has_referer
            && job.policy.referer_strategy == RefererStrategy::Direct
            && let Some(previous) = self.last_page(persona.id, &job.domain)
        {
            // A later page of a visit arrives from the page before it.
            headers.push(("Referer".to_owned(), previous.to_string()));
        }
        let fetch = FetchSpec {
            url: job.url.clone(),
            method: job.method.clone(),
            headers,
            body: job.body.clone(),
            kind: job.kind,
            guard: Guard::Target(self.security),
            timeout: Some(job.timeout),
            max_body_bytes: job
                .max_body_bytes
                .and_then(|bytes| usize::try_from(bytes).ok()),
            max_redirects: None,
            fetch_subresources: None,
            seed: Some(job.seed),
        };
        let fetched = match client.fetch(&fetch).await {
            Ok(fetched) => fetched,
            Err(error) => return Self::http_failure(&error),
        };

        let classification = self.verdicts.classify(&Observation {
            render: Render::Http,
            status: fetched.status,
            url: fetched.final_url.to_string(),
            headers: fetched.headers.clone(),
            cookies: cookies_set(&fetched.headers),
            body: fetched.text(),
            browser: None,
        });
        if classification.outcome.is_success() {
            self.manager.pool().report_traffic_ok(proxy.id);
            self.remember_page(persona.id, &job.domain, fetched.final_url.clone());
        }
        AttemptOutput {
            courtesy: courtesy_of(classification.outcome, Some(fetched.status)),
            classification,
            reason: None,
            status: Some(fetched.status),
            final_url: Some(fetched.final_url.to_string()),
            headers: fetched.headers.clone(),
            redirects: fetched
                .redirects()
                .iter()
                .map(|hop| hop.url.to_string())
                .collect(),
            timing: fetched.timing,
            page: None,
            fetched: Some(Box::new(fetched)),
            cookies_to_browser: None,
            cookies_to_http: None,
            behavior: None,
            flow: None,
        }
    }

    fn http_failure(error: &HttpError) -> AttemptOutput {
        tracing::debug!(%error, "an http attempt failed");
        let mut output = match error {
            HttpError::Timeout { .. } => {
                AttemptOutput::failed(Outcome::Timeout, "timeout", Courtesy::ServerError)
            }
            HttpError::Denied(_) | HttpError::RedirectDenied(_) => {
                AttemptOutput::failed(Outcome::SsrfDenied, "ssrf_denied", Courtesy::NotTheSite)
            }
            HttpError::ProxyAuth | HttpError::Connect(_) => {
                AttemptOutput::failed(Outcome::ProviderError, "proxy_failed", Courtesy::NotTheSite)
            }
            // The site answered, with nothing that can be used.
            HttpError::TooManyRedirects { .. } => AttemptOutput::failed(
                Outcome::ProviderError,
                "too_many_redirects",
                Courtesy::Answered,
            )
            .finally(),
            HttpError::Tls | HttpError::Request(_) | HttpError::Body(_) => AttemptOutput::failed(
                Outcome::ProviderError,
                "connection_failed",
                Courtesy::ServerError,
            ),
            // The request itself cannot be sent: no persona changes that.
            HttpError::Header(_)
            | HttpError::InvalidUrl(_)
            | HttpError::InvalidReferer(_)
            | HttpError::Method(_)
            | HttpError::Kind(_)
            | HttpError::InitiatorRequired
            | HttpError::PreflightRequired { .. } => AttemptOutput::failed(
                Outcome::ProviderError,
                "invalid_request",
                Courtesy::NotTheSite,
            )
            .finally(),
            HttpError::Profile(_)
            | HttpError::Build(_)
            | HttpError::Cookies(_)
            | HttpError::Resolve { .. } => {
                AttemptOutput::failed(Outcome::ProviderError, "http_client", Courtesy::NotTheSite)
            }
        };
        output
            .classification
            .evidence
            .push(format!("error:{error}"));
        output
    }

    /// The flow of a browser attempt, run in the persona's context: the
    /// steps as the sidecar reported them and the page the flow ended on,
    /// every capture checked against the SSRF guard.
    async fn flow(
        &self,
        job: &AttemptJob,
        plan: &FlowPlan,
        context: uuid::Uuid,
    ) -> Result<(Box<PageResult>, BehaviorApplied, FlowRun), Failed> {
        let (request, behavior) = flow_for(job, plan, &self.browser);
        let sidecar = self.manager.sidecars().of(job.persona.engine);
        let running = sidecar.flow(context, &request);
        let events = match tokio::time::timeout(job.timeout + BROWSER_GRACE, running).await {
            Ok(Ok(events)) => events,
            Ok(Err(error)) => {
                return Err(Box::new(Self::unavailable(&PersonaError::Browser(error))));
            }
            Err(_) => {
                return Err(Box::new(AttemptOutput::failed(
                    Outcome::Timeout,
                    "timeout",
                    Courtesy::ServerError,
                )));
            }
        };
        let mut steps = Vec::new();
        let mut last = None;
        for event in events {
            match event {
                FlowEvent::Step {
                    action,
                    capture,
                    duration_ms,
                    error,
                    index,
                    ok,
                    paced_ms,
                } => steps.push(FlowStep {
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    action,
                    ok,
                    duration_ms,
                    paced_ms,
                    error,
                    capture: capture.map(Box::new),
                }),
                FlowEvent::Final { ok, result, .. } => last = Some((ok, Box::new(result))),
            }
        }
        let Some((ok, mut page)) = last else {
            // The client returns no events without the final one.
            return Err(Box::new(AttemptOutput::failed(
                Outcome::ProviderError,
                "sidecar_error",
                Courtesy::NotTheSite,
            )));
        };
        // Everything the flow brought back is checked like a navigation's
        // chain; one refused hop and nothing of the flow is kept. (A page
        // that is still the empty tab, after a `goto` that did not arrive,
        // has not been anywhere.)
        let denied = steps
            .iter()
            .filter_map(|step| step.capture.as_deref())
            .chain(std::iter::once(page.as_ref()))
            .find_map(|seen| ssrf::check_capture(seen, self.security, self.max_redirects).err());
        if let Some(denied) = denied {
            tracing::warn!(
                context = %context,
                hop = denied.hop,
                reason = %denied.reason,
                "a flow visited what the SSRF guard refuses"
            );
            ssrf::drop_captures(&mut page);
            let mut output =
                AttemptOutput::failed(Outcome::SsrfDenied, "ssrf_denied", Courtesy::NotTheSite);
            output.classification.evidence.push(denied.evidence());
            output.final_url = Some(denied.url);
            output.timing = timing_of(&page);
            output.page = Some(page);
            output.behavior = Some(behavior);
            return Err(Box::new(output));
        }
        Ok((page, behavior, FlowRun { steps, ok }))
    }

    #[allow(clippy::too_many_lines)] // One attempt in a browser, from its context to its verdict.
    async fn browser(&self, job: &AttemptJob) -> AttemptOutput {
        let persona = &job.persona;
        let (proxy, endpoint) = match self.manager.egress(persona, Via::Browser).await {
            Ok(route) => route,
            Err(error) => return Self::unavailable(&error),
        };
        let lease = match self.manager.contexts().acquire(persona, &endpoint).await {
            Ok(lease) => lease,
            Err(error) => return Self::unavailable(&error),
        };
        let context = lease.context();
        let origin = origin_of(&job.url);

        // What the http path earned for this origin goes into the browser
        // first. A persona without an http path has nothing to hand over.
        let mut cookies_to_browser = None;
        if let Some(origin) = &origin {
            match self.manager.http_to_browser(persona, context, origin).await {
                Ok(handoff) => cookies_to_browser = Some(handoff.moved),
                Err(PersonaError::HttpPathOff { .. }) => {}
                Err(error) => {
                    tracing::warn!(persona = %persona.id, %error, "cookies could not be handed to the browser");
                }
            }
        }

        let loaded = match &job.flow {
            Some(plan) => self.flow(job, plan, context).await,
            None => self
                .navigate(job, context)
                .await
                .map(|(page, behavior)| (page, behavior, FlowRun::default())),
        };
        let (page, behavior, flow) = match loaded {
            Ok(loaded) => loaded,
            // The browser is in a state nobody knows, or was somewhere it
            // must not be: it is closed.
            Err(output) => {
                lease.close().await;
                return *output;
            }
        };

        let failure = page_failure(&page);
        let mut classification = self.verdicts.judge_page(&page);
        let mut reason = failure
            .as_ref()
            .filter(|_| !classification.outcome.is_block())
            .map(|failure| failure.reason.to_owned());
        let status = page.status.and_then(|status| u16::try_from(status).ok());
        let mut courtesy = match &failure {
            Some(failure) if !classification.outcome.is_block() => failure.courtesy,
            _ => courtesy_of(classification.outcome, status),
        };
        // A flow whose step failed on a page that is no wall did not do
        // what was asked, whatever the page it stopped on looks like.
        if job.flow.is_some()
            && !flow.ok
            && !classification.outcome.is_block()
            && let Some(failed) = flow.failed()
        {
            let stopped = step_failure(failed.error.as_deref().unwrap_or_default());
            classification = unanswered(stopped.outcome, stopped.reason);
            classification.retryable = stopped.retryable;
            classification.evidence.push(format!(
                "step:{}:{}",
                failed.index,
                failed.error.as_deref().unwrap_or_default()
            ));
            reason = Some(stopped.reason.to_owned());
            courtesy = stopped.courtesy;
        }
        let outcome = classification.outcome;

        let mut cookies_to_http = None;
        if outcome.is_success() {
            self.manager.pool().report_traffic_ok(proxy.id);
            // What the browser earned goes to the persona's http jar.
            if let Some(origin) = &origin {
                match self.manager.browser_to_http(persona, context, origin).await {
                    Ok(handoff) => cookies_to_http = Some(handoff.moved),
                    Err(PersonaError::HttpPathOff { .. }) => {}
                    Err(error) => {
                        tracing::warn!(persona = %persona.id, %error, "cookies could not be handed to the http path");
                    }
                }
            }
            lease.keep(job.keeps_visiting);
        } else if outcome.is_block() {
            // It leaves: no reload, no cookies of the wall, the window
            // closed.
            lease.close().await;
        } else {
            lease.keep(false);
        }

        AttemptOutput {
            courtesy,
            reason,
            classification,
            status,
            final_url: Some(page.final_url.clone()),
            headers: page
                .headers
                .iter()
                .map(|header| Header {
                    name: header.name.clone(),
                    value: header.value.clone(),
                })
                .collect(),
            redirects: page.redirects.iter().map(|hop| hop.url.clone()).collect(),
            timing: timing_of(&page),
            page: Some(page),
            fetched: None,
            cookies_to_browser,
            cookies_to_http,
            behavior: Some(behavior),
            flow: job.flow.is_some().then_some(flow),
        }
    }

    /// The one page of a browser attempt, with its redirect chain checked.
    async fn navigate(
        &self,
        job: &AttemptJob,
        context: uuid::Uuid,
    ) -> Result<(Box<PageResult>, BehaviorApplied), Failed> {
        let (request, behavior) = navigation_for(job, &self.browser);
        let sidecar = self.manager.sidecars().of(job.persona.engine);
        let navigating =
            sidecar.navigate_checked(context, &request, self.security, self.max_redirects);
        let navigation = match tokio::time::timeout(job.timeout + BROWSER_GRACE, navigating).await {
            Ok(Ok(navigation)) => navigation,
            Ok(Err(error)) => {
                return Err(Box::new(Self::unavailable(&PersonaError::Browser(error))));
            }
            Err(_) => {
                return Err(Box::new(AttemptOutput::failed(
                    Outcome::Timeout,
                    "timeout",
                    Courtesy::ServerError,
                )));
            }
        };
        match navigation {
            Navigation::Page(page) => Ok((page, behavior)),
            Navigation::SsrfDenied { denied, result } => {
                let mut output =
                    AttemptOutput::failed(Outcome::SsrfDenied, "ssrf_denied", Courtesy::NotTheSite);
                output.classification.evidence.push(denied.evidence());
                output.final_url = Some(denied.url);
                output.timing = timing_of(&result);
                output.page = Some(result);
                output.behavior = Some(behavior);
                Err(Box::new(output))
            }
        }
    }
}

#[async_trait]
impl AttemptExecutor for PathExecutor {
    async fn run(&self, job: &AttemptJob) -> AttemptOutput {
        // The bytes of the attempt count toward its domain.
        let _label = self
            .meter
            .as_ref()
            .map(|meter| meter.label(job.persona.id, &job.domain));
        match job.render {
            Render::Http => self.http(job).await,
            Render::Browser => self.browser(job).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_browser::contract::{DwellMode, EntryStrategy};
    use kage_core::config::BrowserConfig;
    use kage_core::outcome::Outcome;
    use kage_core::policy::{DomainPolicies, EffectivePolicy, RefererStrategy};
    use kage_core::schedule::politeness::Courtesy;
    use kage_core::seed::Seed;
    use kage_http::{HttpError, Stage};
    use url::Url;

    use super::{AttemptOutput, BehaviorWish, PathExecutor, behavior_for, draw_dwell, origin_of};

    fn policy(yaml: &str) -> EffectivePolicy {
        DomainPolicies::from_yaml(yaml)
            .unwrap()
            .resolve("example.org")
    }

    #[test]
    fn a_browser_render_asks_for_human_behavior() {
        let browser = BrowserConfig::default();
        let seed = Seed::from_u64(5);

        let (behavior, applied) = behavior_for(&policy(""), &browser, &seed, None);

        assert_eq!(behavior.dwell, Some(DwellMode::Human));
        assert_eq!(behavior.wander, Some(true));
        assert_eq!(behavior.seed.as_deref(), Some(seed.to_string().as_str()));
        let entry = behavior.entry.clone().unwrap();
        assert_eq!(entry.strategy, EntryStrategy::Direct);
        assert_eq!((entry.from_url, entry.fallback), (None, None));
        // What was asked for is what is reported, and it was the policy's.
        assert!(!applied.from_request && !applied.request_ignored);
        assert_eq!(applied.scroll_depth, behavior.scroll_depth);
        assert_eq!(applied.dwell, None);
        assert_eq!(applied.referer_strategy, RefererStrategy::Direct);
        assert_eq!(applied.seed, seed);
        // The same seed scrolls as deep every time; other seeds cover the
        // range and never leave it.
        assert_eq!(behavior, behavior_for(&policy(""), &browser, &seed, None).0);
        let depths: Vec<f64> = (0..400)
            .map(|n| {
                behavior_for(&policy(""), &browser, &Seed::from_u64(n), None)
                    .0
                    .scroll_depth
                    .unwrap()
            })
            .collect();
        assert!(depths.iter().all(|depth| (0.6..0.9).contains(depth)));
        assert!(depths.iter().any(|depth| *depth < 0.63));
        assert!(depths.iter().any(|depth| *depth > 0.87));

        // `none` is asked for as written; a search entry has no template
        // and is a typed navigation.
        let none = policy("defaults:\n  referer_strategy: none\n");
        assert_eq!(
            behavior_for(&none, &browser, &seed, None)
                .0
                .entry
                .unwrap()
                .strategy,
            EntryStrategy::None
        );
        let search = policy("defaults:\n  referer_strategy: search_engine\n");
        assert_eq!(
            behavior_for(&search, &browser, &seed, None)
                .0
                .entry
                .unwrap()
                .strategy,
            EntryStrategy::Direct
        );
        // A wish that asks for nothing is no wish.
        let (empty, applied) =
            behavior_for(&policy(""), &browser, &seed, Some(&BehaviorWish::default()));
        assert_eq!(empty, behavior);
        assert!(!applied.from_request && !applied.request_ignored);
    }

    #[test]
    fn a_request_shortens_the_behavior_but_never_to_a_bare_load() {
        let browser = BrowserConfig::default();
        let seed = Seed::from_u64(11);
        let wish = BehaviorWish {
            scroll_depth: Some(0.3),
            dwell: Some(Duration::from_secs(3)),
            referer_strategy: Some(RefererStrategy::None),
        };

        let (behavior, applied) = behavior_for(&policy(""), &browser, &seed, Some(&wish));

        assert_eq!(behavior.scroll_depth, Some(0.3));
        assert_eq!(behavior.dwell, Some(DwellMode::Fixed));
        assert_eq!(behavior.entry.unwrap().strategy, EntryStrategy::None);
        // The pointer wanders whatever the request says: there is no way
        // to ask for a page nobody touches.
        assert_eq!(behavior.wander, Some(true));
        assert!(applied.from_request && !applied.request_ignored);
        let dwell = applied.dwell.unwrap();
        assert!(dwell >= Duration::from_secs(2), "{dwell:?}");
        assert_eq!(applied.referer_strategy, RefererStrategy::None);

        // No scrolling is `0`, and is asked of the sidecar as no depth.
        let still = BehaviorWish {
            scroll_depth: Some(0.0),
            ..BehaviorWish::default()
        };
        let (behavior, applied) = behavior_for(&policy(""), &browser, &seed, Some(&still));
        assert_eq!(behavior.scroll_depth, None);
        assert_eq!(applied.scroll_depth, None);
        // Only the depth was the request's: the dwell stays the persona's.
        assert_eq!(behavior.dwell, Some(DwellMode::Human));
        assert_eq!(applied.dwell, None);
        assert!(applied.from_request);
        // More than the whole page is the whole page.
        let deep = BehaviorWish {
            scroll_depth: Some(7.0),
            ..BehaviorWish::default()
        };
        assert_eq!(
            behavior_for(&policy(""), &browser, &seed, Some(&deep))
                .0
                .scroll_depth,
            Some(1.0)
        );
    }

    #[test]
    fn a_dwell_is_drawn_around_the_median_and_never_below_the_floor() {
        let floor = Duration::from_secs(2);
        let draw = |median_ms: u64, n: u64| {
            draw_dwell(Duration::from_millis(median_ms), floor, &Seed::from_u64(n))
        };
        // The same attempt dwells as long every time.
        assert_eq!(draw(3000, 1), draw(3000, 1));

        let mut dwells: Vec<Duration> = (0..2000).map(|n| draw(3000, n)).collect();
        dwells.sort_unstable();
        assert!(dwells[0] >= floor, "{:?}", dwells[0]);
        // No pile at the floor, and no two attempts alike: a draw below
        // the floor is drawn again, not raised to it.
        let distinct: std::collections::BTreeSet<Duration> = dwells.iter().copied().collect();
        assert!(distinct.len() > 1990, "{}", distinct.len());
        let at_the_floor = dwells
            .iter()
            .filter(|dwell| **dwell < floor + Duration::from_millis(20))
            .count();
        assert!(at_the_floor < 40, "{at_the_floor}");
        let median = dwells[dwells.len() / 2];
        assert!(
            (Duration::from_millis(3000)..Duration::from_millis(3700)).contains(&median),
            "{median:?}"
        );
        let p95 = dwells[dwells.len() * 95 / 100];
        assert!(
            (Duration::from_millis(5200)..Duration::from_millis(7000)).contains(&p95),
            "{p95:?}"
        );

        // A median below the floor is the floor's; nothing is ever shorter.
        let short: Vec<Duration> = (0..500).map(|n| draw(0, n)).collect();
        assert!(short.iter().all(|dwell| *dwell >= floor));
        assert!(short.iter().any(|dwell| *dwell > Duration::from_secs(3)));
        // A very long one stays inside what the contract takes.
        let long: Vec<Duration> = (0..500).map(|n| draw(600_000, n)).collect();
        assert!(long.iter().all(|dwell| *dwell <= Duration::from_mins(2)));
    }

    #[test]
    fn a_policy_that_locks_behavior_ignores_the_request_and_says_so() {
        let browser = BrowserConfig::default();
        let seed = Seed::from_u64(3);
        let locked = policy("defaults:\n  behavior_locked: true\n  min_dwell: 5s\n");
        let wish = BehaviorWish {
            scroll_depth: Some(0.0),
            dwell: Some(Duration::from_secs(1)),
            referer_strategy: Some(RefererStrategy::None),
        };

        let (behavior, applied) = behavior_for(&locked, &browser, &seed, Some(&wish));

        assert_eq!(behavior, behavior_for(&locked, &browser, &seed, None).0);
        assert_eq!(behavior.dwell, Some(DwellMode::Human));
        assert!((0.6..0.9).contains(&behavior.scroll_depth.unwrap()));
        assert!(applied.request_ignored && !applied.from_request);
        assert_eq!(applied.referer_strategy, RefererStrategy::Direct);

        // Without the lock the policy's own floor holds.
        let floored = policy("defaults:\n  min_dwell: 5s\n");
        let (_, applied) = behavior_for(&floored, &browser, &seed, Some(&wish));
        assert!(applied.dwell.unwrap() >= Duration::from_secs(5));
        assert!(!applied.request_ignored);
    }

    #[test]
    fn an_origin_is_a_scheme_a_host_and_a_port_that_is_not_the_default() {
        let origin = |text: &str| origin_of(&Url::parse(text).unwrap());

        assert_eq!(
            origin("https://shop.example/cart?x=1#top").as_deref(),
            Some("https://shop.example")
        );
        assert_eq!(
            origin("http://shop.example:8080/").as_deref(),
            Some("http://shop.example:8080")
        );
        assert_eq!(
            origin("https://shop.example:443/").as_deref(),
            Some("https://shop.example")
        );
        assert_eq!(origin("data:text/plain,hi"), None);
    }

    #[test]
    fn a_failed_http_attempt_says_what_failed_and_whose_fault_it_was() {
        let told = |error: HttpError| {
            let output = PathExecutor::http_failure(&error);
            (
                output.outcome(),
                output.reason.clone().unwrap(),
                output.courtesy,
                output.classification.retryable,
            )
        };

        assert_eq!(
            told(HttpError::Timeout {
                stage: Stage::ReadingBody,
                after: Duration::from_secs(30),
            }),
            (
                Outcome::Timeout,
                "timeout".to_owned(),
                Courtesy::ServerError,
                true
            )
        );
        assert_eq!(
            told(HttpError::ProxyAuth),
            (
                Outcome::ProviderError,
                "proxy_failed".to_owned(),
                Courtesy::NotTheSite,
                true
            )
        );
        assert_eq!(
            told(HttpError::Connect("no tunnel")).1,
            "proxy_failed".to_owned()
        );
        assert_eq!(
            told(HttpError::Tls),
            (
                Outcome::ProviderError,
                "connection_failed".to_owned(),
                Courtesy::ServerError,
                true
            )
        );
        // What no other persona would change is not retried.
        assert_eq!(
            told(HttpError::Method("PUT".to_owned())),
            (
                Outcome::ProviderError,
                "invalid_request".to_owned(),
                Courtesy::NotTheSite,
                false
            )
        );
        assert_eq!(
            told(HttpError::TooManyRedirects {
                max: 10,
                location: "/again".to_owned(),
            }),
            (
                Outcome::ProviderError,
                "too_many_redirects".to_owned(),
                Courtesy::Answered,
                false
            )
        );
        assert_eq!(
            told(HttpError::Build("no".to_owned())).1,
            "http_client".to_owned()
        );
        // The error itself is part of the evidence.
        let output = PathExecutor::http_failure(&HttpError::Tls);
        assert_eq!(
            output.classification.evidence,
            [
                "failure:connection_failed",
                "error:the TLS handshake with the target failed"
            ]
        );
    }

    #[test]
    fn a_failure_can_be_made_final() {
        let output =
            AttemptOutput::failed(Outcome::ProviderError, "why", Courtesy::NotTheSite).finally();

        assert!(!output.classification.retryable);
        assert_eq!(output.outcome(), Outcome::ProviderError);
        assert_eq!(output.status, None);
        assert!(output.page.is_none() && output.fetched.is_none());
    }
}
