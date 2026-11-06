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
//! **Level 2** (spec §25) is the browser path as a still visit: the
//! persona's browser loads the page and nothing else (contract 1.5.0,
//! `behavior.mode: still`): a typed entry, no input of any kind, no
//! scrolling, no patience, and a dwell drawn above the policy's
//! `min_dwell` ([`still_dwell`]). A crawl's next page at level 2 is still
//! clicked (the hand travels to the link and clicks, and nothing else).
//!
//! **Level 0** is no persona's: the plain client ([`kage_http::PlainClient`])
//! fetches through the forwarder under a plain credential, from the exit
//! the pipeline names: the host's connection, or the site's one plain exit
//! of the pool.
//!
//! Both paths end at the classifier with the signatures in force. A
//! success postpones the next health probe of the proxy it went through.

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use kage_browser::contract::{
    BehaviorMode, BehaviorOptions, CaptureOptions, DwellMode, Entry, EntryFallback, EntryOutcome,
    EntryStrategy, FlowAction, FlowBehavior, FlowEvent, FlowPace, FlowRequest, NavigateRequest,
    PageResult, Wait, WaitUntil,
};
use kage_browser::ssrf::DeniedHop;
use kage_browser::{Navigation, navigation_bound, ssrf};
use kage_core::classifier::{Classification, Observation, without_document};
use kage_core::config::{BrowserConfig, Config};
use kage_core::domain::{Exit, Header, Level, Persona, Render, Timing};
use kage_core::ids::{PersonaId, ProxyId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::pacing::Lognormal;
use kage_core::persona::pacing_factor;
use kage_core::policy::{EffectivePolicy, RefererStrategy};
use kage_core::schedule::politeness::Courtesy;
use kage_core::seed::Seed;
use kage_core::ssrf::{SsrfDenial, SsrfPolicy, check_url};
use kage_http::{
    FetchSpec, Fetched, Guard, HopExits, HttpError, PlainClient, PlainSpec, RequestKind,
};
use kage_persona::identity::http_spec;
use kage_persona::{PersonaError, PersonaManager, Via};
use url::Url;

use crate::forwarder::{Caller, Denial, Denials, ForwarderEgress, Meter, PlainRoute};
use crate::verdicts::{
    NoDocument, Verdicts, cookies_set, courtesy_of, no_document, page_failure, unanswered,
};

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
    /// The addresses of the tabs the flow's page opened while the step ran
    /// (F184): the flow's, kept behind its page and closed with it.
    pub opened_tabs: Vec<String>,
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

/// How much longer than its own budget a flow is waited for: the sidecar
/// answers when the budget ran out, a little after it.
const BROWSER_GRACE: Duration = Duration::from_secs(20);

/// How much longer a navigation is waited for than the sidecar's client
/// waits for it ([`navigation_bound`]): the client gives up first and says
/// why.
const NAVIGATION_GRACE: Duration = Duration::from_secs(5);

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

/// How a persona arrives at a page when whoever asks knows what it has
/// been reading (a crawl's next page, spec §13).
///
/// Nothing of it is forged: a link is followed only from a page that
/// holds it, and a policy whose `referer_strategy` is `none` types every
/// address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arrival {
    /// Nothing the persona has been reading leads to the page: its address
    /// is typed, and on neither path the request names a page it came
    /// from.
    Typed,
    /// A page the persona has been reading holds a link to it. In a
    /// browser the persona clicks that link: on the page that is open, or
    /// after going back to it. A tab that does not hold the page, or a
    /// page that does not hold the link, and the address is typed instead.
    /// On the http path, which has no tab, the request carries the page as
    /// its `Referer`, as the click would.
    Link {
        /// The page that holds the link.
        from: Url,
        /// How many entries of its tab the browser goes back at most to
        /// find that page; 0 for the open page only.
        max_backs: u32,
    },
}

/// How an attempt reached its page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reached {
    /// The address was typed: no `Referer`, `Sec-Fetch-Site: none`.
    Typed,
    /// A link on a page the persona had been reading was followed.
    Link {
        /// How many entries of its tab the browser went back before the
        /// click, 0 when the open page held the link; `None` on the http
        /// path, which has no tab to go back in.
        backs: Option<u32>,
    },
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
    /// How long the persona goes on scrolling this page at most: drawn for
    /// this attempt from its seed, at the persona's pace (follow-up F44).
    /// `None` for a flow, which scrolls where its steps say.
    pub patience: Option<Duration>,
    /// How the page is arrived at.
    pub referer_strategy: RefererStrategy,
    /// The seed of the attempt, from which all of it was drawn.
    pub seed: Seed,
}

/// What one attempt of a plain client (level 0) is asked to do.
#[derive(Debug, Clone)]
pub struct PlainJob {
    /// The request the attempt belongs to.
    pub request: RequestId,
    /// 1 for the first attempt, 2 for the second, and so on.
    pub n: u32,
    /// Where it leaves from: the host's connection, or the site's plain
    /// exit of the pool.
    pub route: PlainRoute,
    /// Where every other site's plain traffic leaves from: a redirect hop
    /// to another site is asked for from that site's exit, or not at all
    /// (F166). `None` asks every hop from `route`.
    pub sites: Option<Arc<dyn SiteExits>>,
    /// The page.
    pub url: Url,
    /// `GET`, `HEAD` or `POST`.
    pub method: http::Method,
    /// The caller's headers, as given.
    pub headers: Vec<(String, String)>,
    /// The caller's body, as given.
    pub body: Option<Bytes>,
    /// The host.
    pub domain: String,
    /// Longest the attempt may take.
    pub timeout: Duration,
    /// The largest body kept.
    pub max_body_bytes: Option<u64>,
}

/// Where a site's plain traffic leaves from (spec §25, D50): under the pool
/// the site's own plain exit, else its scout's proxy; the host's connection
/// under `egress.plain: host`. The pipeline knows it; a plain attempt asks
/// it for the exit of a redirect hop to another site (F166).
#[async_trait]
pub trait SiteExits: Send + Sync + fmt::Debug {
    /// The route of `site`'s plain traffic, whose page `url` is; `None` when
    /// that address cannot carry traffic now.
    ///
    /// # Errors
    ///
    /// Why it could not be found out.
    async fn route_of(&self, site: &str, url: &Url) -> Result<Option<PlainRoute>, String>;
}

/// The exits of one plain attempt's redirect hops (F166): a hop to the
/// request's own site leaves where the request does; a hop to another site
/// from that site's own plain exit, through a plain client of that exit.
struct AttemptExits<'a> {
    executor: &'a PathExecutor,
    egress: &'a ForwarderEgress,
    sites: &'a dyn SiteExits,
    /// The request's own site and route.
    home: (String, PlainRoute),
    /// The clients of the other routes a hop left by.
    used: Mutex<Vec<(PlainRoute, Arc<PlainClient>)>>,
}

impl AttemptExits<'_> {
    /// The routes other than the request's own that a hop left by.
    fn routes(&self) -> Vec<PlainRoute> {
        self.used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(route, _)| *route)
            .collect()
    }
}

#[async_trait]
impl HopExits for AttemptExits<'_> {
    async fn client_for(&self, url: &Url) -> Result<Option<Arc<PlainClient>>, String> {
        let Some(host) = host_of(url) else {
            return Ok(None);
        };
        let site = kage_crawl::site::of_host(&host);
        if site == self.home.0 {
            return Ok(None);
        }
        let route = self
            .sites
            .route_of(&site, url)
            .await?
            .ok_or_else(|| format!("the plain exit of {site} cannot carry traffic now"))?;
        if route == self.home.1 {
            return Ok(None);
        }
        if let Some((_, client)) = self
            .used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(used, _)| *used == route)
        {
            return Ok(Some(Arc::clone(client)));
        }
        let endpoint = self.egress.plain_endpoint(route);
        let client = self
            .executor
            .manager
            .http()
            .plain(&endpoint, &self.executor.plain_user_agent)
            .map_err(|error| error.to_string())?;
        let client = Arc::new(client);
        tracing::debug!(
            site,
            exit = route.exit().as_str(),
            "a redirect hop to another site leaves from that site's exit"
        );
        self.used
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((route, Arc::clone(&client)));
        Ok(Some(client))
    }
}

impl PlainJob {
    /// The connection the attempt leaves through.
    pub const fn exit(&self) -> Exit {
        self.route.exit()
    }
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
    /// The level of the request: 1 to 3 (level 0 is a [`PlainJob`]).
    pub level: Level,
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
    /// How the persona arrives, when whoever asks knows what it has been
    /// reading; `None` leaves it to the path: a typed address in a
    /// browser, the page before as `Referer` on the http path.
    pub arrival: Option<Arrival>,
    /// The tab a browser attempt runs in (F96): a crawl's pages name their
    /// job and share one tab, so that the next page is reached by a click
    /// on the page before, which the tab still holds. `None`, as for every
    /// other request, and the attempt starts in a fresh tab that holds
    /// nothing of what came before it.
    pub tab: Option<String>,
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
    /// How the page was reached; `None` when the attempt asked the site
    /// for nothing, and for a flow.
    pub reached: Option<Reached>,
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
            reached: None,
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
            reached: None,
        }
    }

    /// The body of the main response as the site sent it: the raw body of
    /// the page a browser loaded, else the body the http path fetched
    /// (levels 0 and 1). `None` when the attempt brought neither. What
    /// reads a file the site sent (a `robots.txt`, a sitemap) reads it from
    /// here, at every level (F99).
    pub fn sent_body(&self) -> Option<Vec<u8>> {
        match self.page.as_deref() {
            Some(page) => page.raw_body.as_ref().and_then(crate::archive::bytes_of),
            None => self.fetched.as_deref().map(|fetched| fetched.body.to_vec()),
        }
    }

    /// Whether the body of the response was cut at `max_body_bytes`
    /// (spec §9, F122): the document the browser serialized, or the body
    /// it or the http path read.
    pub fn body_cut(&self) -> bool {
        match self.page.as_deref() {
            Some(page) => {
                page.dom_truncated || page.raw_body.as_ref().is_some_and(|body| body.truncated)
            }
            None => self
                .fetched
                .as_deref()
                .is_some_and(|fetched| fetched.truncated),
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

    /// Makes one attempt of a plain client (level 0), as [`Self::run`].
    async fn plain(&self, job: &PlainJob) -> AttemptOutput;
}

/// The dwell of a still visit (level 2, spec §25): drawn from the
/// attempt's seed above the policy's `min_dwell` (`floor`), a median of one
/// and a half times the floor and a 95th percentile of three times it, so
/// that no two pages are left at the same instant and the visit replays. A
/// draw below the floor is drawn again, never raised to it.
pub fn still_dwell(floor: Duration, seed: &Seed) -> Duration {
    let cap = MAX_FIXED_DWELL.as_secs_f64() * 1000.0;
    let floor_ms = (floor.as_secs_f64() * 1000.0).clamp(1.0, cap / 3.0);
    let shape = Lognormal {
        median: 1.5 * floor_ms,
        p95: 3.0 * floor_ms,
        min: None,
        max: Some(cap),
    };
    let mut rng = seed.child("still-dwell").rng();
    for _ in 0..64 {
        let value = shape.sample(&mut rng);
        if value >= floor_ms {
            return Duration::from_secs_f64(value / 1000.0);
        }
    }
    floor
}

/// The behavior of a level-2 page (spec §25): a still visit, typed, with
/// the dwell of [`still_dwell`]. With `link`, the page is a crawl's next
/// one and is reached by a click there (the hand travels to the link and
/// clicks, and nothing else: no reading scroll, no wander, no patience).
/// No request changes any of it.
pub fn still_behavior(
    policy: &EffectivePolicy,
    seed: &Seed,
    link: Option<(&Url, u32)>,
) -> (BehaviorOptions, BehaviorApplied) {
    let dwell = still_dwell(policy.min_dwell.get(), seed);
    let (mode, entry) = match link {
        // A click is the hand's: the one way of reaching a page that
        // sends input at this level.
        Some((from, max_backs)) if policy.referer_strategy != RefererStrategy::None => {
            (BehaviorMode::Human, entry_from(from, max_backs))
        }
        _ => (
            BehaviorMode::Still,
            Entry {
                fallback: None,
                from_open_page: None,
                from_url: None,
                max_backs: None,
                strategy: EntryStrategy::Direct,
            },
        ),
    };
    let options = BehaviorOptions {
        dwell: Some(DwellMode::Fixed),
        entry: Some(entry),
        mode: Some(mode),
        patience_ms: None,
        scroll_depth: None,
        seed: Some(seed.to_string()),
        wander: Some(false),
    };
    let applied = BehaviorApplied {
        from_request: false,
        request_ignored: false,
        scroll_depth: None,
        dwell: Some(dwell),
        patience: None,
        referer_strategy: RefererStrategy::Direct,
        seed: *seed,
    };
    (options, applied)
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
///
/// How long the scrolling goes on at most is nobody's wish: a patience
/// drawn for this attempt from its seed (`browser.patience`), at the pace
/// of the persona (`pace`, [`pacing_factor`]). No request and no policy
/// bounds a visit by a time that is the same for every page.
pub fn behavior_for(
    policy: &EffectivePolicy,
    browser: &BrowserConfig,
    seed: &Seed,
    pace: f64,
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
    let referer_strategy = arrives_as(
        wish.and_then(|wish| wish.referer_strategy)
            .unwrap_or(policy.referer_strategy),
    );
    let patience = browser.patience.draw(pace, seed);
    let options = BehaviorOptions {
        dwell: Some(if dwell.is_some() {
            DwellMode::Fixed
        } else {
            DwellMode::Human
        }),
        entry: Some(Entry {
            fallback: None,
            from_open_page: None,
            from_url: None,
            max_backs: None,
            strategy: match referer_strategy {
                RefererStrategy::None => EntryStrategy::None,
                RefererStrategy::Direct | RefererStrategy::SearchEngine => EntryStrategy::Direct,
            },
        }),
        mode: Some(BehaviorMode::Human),
        patience_ms: u64::try_from(patience.as_millis()).ok(),
        scroll_depth,
        seed: Some(seed.to_string()),
        wander: Some(true),
    };
    let applied = BehaviorApplied {
        from_request: wish.is_some(),
        request_ignored: asked.is_some() && wish.is_none(),
        scroll_depth,
        dwell,
        patience: Some(patience),
        referer_strategy,
        seed: *seed,
    };
    (options, applied)
}

/// How a visit arrives under the strategy a policy or a request names. A
/// search entry needs a results page built from a template, and none can be
/// configured yet (`policies/domains.yaml`): the visit is typed, and what
/// applied says so, never `search_engine` for a visit no results page led
/// to (F143).
const fn arrives_as(strategy: RefererStrategy) -> RefererStrategy {
    match strategy {
        RefererStrategy::SearchEngine => RefererStrategy::Direct,
        other => other,
    }
}

/// The entry of a navigation that arrives from a page the persona has been
/// reading (`from`): the link there is clicked, on the open page or after
/// going back to it, at most `max_backs` entries; when the tab does not
/// hold the page or the page does not hold the link, the address is typed.
fn entry_from(from: &Url, max_backs: u32) -> Entry {
    Entry {
        fallback: Some(EntryFallback::None),
        from_open_page: Some(true),
        from_url: Some(from.to_string()),
        max_backs: Some(u64::from(max_backs)),
        strategy: EntryStrategy::Link,
    }
}

/// How a page the browser returned was reached, as its entry reports it.
fn reached_of(page: &PageResult) -> Reached {
    let entry = page
        .behavior
        .as_ref()
        .and_then(|behavior| behavior.entry.as_ref());
    match entry {
        Some(entry)
            if entry.outcome == EntryOutcome::Arrived
                && matches!(
                    entry.strategy,
                    EntryStrategy::Link | EntryStrategy::SearchEngine
                ) =>
        {
            Reached::Link {
                backs: Some(
                    entry
                        .backs
                        .and_then(|backs| u32::try_from(backs).ok())
                        .unwrap_or(0),
                ),
            }
        }
        _ => Reached::Typed,
    }
}

/// The navigation of one browser attempt, and the behavior it asks for.
///
/// `timeout_ms` is what the attempt has to reach its page; how long it
/// stays there is the patience and the dwell of [`behavior_for`]. An
/// attempt with an [`Arrival`] clicks its way there, unless the entry in
/// force is `none`.
pub fn navigation_for(
    job: &AttemptJob,
    browser: &BrowserConfig,
) -> (NavigateRequest, BehaviorApplied) {
    let link = match &job.arrival {
        Some(Arrival::Link { from, max_backs }) => Some((from, *max_backs)),
        Some(Arrival::Typed) | None => None,
    };
    let (behavior, applied) = if job.level == Level::Fast {
        still_behavior(&job.policy, &job.seed, link)
    } else {
        let (mut behavior, applied) = behavior_for(
            &job.policy,
            browser,
            &job.seed,
            pacing_factor(&job.persona.seed),
            job.behavior.as_ref(),
        );
        if let Some((from, max_backs)) = link
            && applied.referer_strategy != RefererStrategy::None
        {
            behavior.entry = Some(entry_from(from, max_backs));
        }
        (behavior, applied)
    };
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
        tab: job.tab.clone(),
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
    // At level 2 a flow dwells the policy's own time drawn upward: a median
    // of one and a half times `min_dwell` and, by the sidecar's draw, a
    // 95th percentile of three times it, never below it (spec §25).
    let still = (job.level == Level::Fast).then(|| floor.mul_f64(1.5));
    let paced = dwell.or(still);
    let request = FlowRequest {
        actions: plan.actions.clone(),
        behavior: Some(FlowBehavior {
            pace: Some(FlowPace {
                dwell_ms: paced.map(millis),
                min_dwell_ms: Some(millis(floor)),
                // At level 2 the hand rests while it dwells.
                wander: (job.level == Level::Fast).then_some(false),
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
        dwell: paced,
        patience: None,
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
        links: None,
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

/// The host of a URL, lower case.
fn host_of(url: &Url) -> Option<String> {
    url.host_str().map(str::to_ascii_lowercase)
}

/// The hosts of every address a page result names: the URL asked for, the
/// redirect hops with where each pointed, and where the page ended.
fn hosts_of(page: &PageResult) -> Vec<String> {
    let mut urls: Vec<Url> = Vec::new();
    urls.extend(Url::parse(&page.url).ok());
    for hop in &page.redirects {
        let Ok(from) = Url::parse(&hop.url) else {
            continue;
        };
        urls.extend(hop.location.as_deref().and_then(|to| from.join(to).ok()));
        urls.push(from);
    }
    urls.extend(Url::parse(&page.final_url).ok());
    urls.iter().filter_map(host_of).collect()
}

/// What an attempt comes to whose address the forwarder refused by the
/// SSRF guard (F102): the guard's outcome, `ssrf_denied`, never retried,
/// and nothing of it kept, whatever the client made of the refusal (a
/// browser shows a failed connection; a plain client fails to connect).
fn denied_by_forwarder(denial: &Denial) -> AttemptOutput {
    tracing::info!(
        host = %denial.host,
        reason = %denial.reason,
        "the forwarder refused the address of an attempt by the SSRF guard"
    );
    let mut output =
        AttemptOutput::failed(Outcome::SsrfDenied, "ssrf_denied", Courtesy::NotTheSite);
    output
        .classification
        .evidence
        .push(format!("refused:{} ({})", denial.host, denial.reason));
    output
}

/// What a browser attempt comes to whose chain, or a flow's page, reached a
/// hop the SSRF guard refuses (spec §19): `ssrf_denied` at the hop, never
/// retried, and nothing the browser captured kept.
fn denied_hop(
    denied: DeniedHop,
    mut page: Box<PageResult>,
    behavior: BehaviorApplied,
) -> AttemptOutput {
    ssrf::drop_captures(&mut page);
    let mut output =
        AttemptOutput::failed(Outcome::SsrfDenied, "ssrf_denied", Courtesy::NotTheSite);
    output.classification.evidence.push(denied.evidence());
    output.final_url = Some(denied.url);
    output.timing = timing_of(&page);
    output.page = Some(page);
    output.behavior = Some(behavior);
    output
}

/// The hops of every page a flow captured and of the page it ended on.
fn flow_hops(steps: &[FlowStep], page: &PageResult) -> Vec<(usize, Url)> {
    steps
        .iter()
        .filter_map(|step| step.capture.as_deref())
        .chain(std::iter::once(page))
        .flat_map(ssrf::capture_hops)
        .collect()
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
    /// The forwarder's endpoints of plain clients; `None` makes no plain
    /// attempt.
    plain: Option<ForwarderEgress>,
    /// What a plain client says unless its caller names another.
    plain_user_agent: String,
    /// What the forwarder refused by the SSRF guard, per caller (F102).
    denials: Option<Arc<Denials>>,
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

/// The reason of an attempt that failed at its proxy.
const PROXY_FAILED: &str = "proxy_failed";

/// The reason of an attempt whose target the proxy would not carry: it
/// refused the tunnel (F189).
const PROXY_REFUSED: &str = "proxy_refused";

/// An attempt that got no answer from the site and counts as the site's
/// failure (a server error, a timeout) while the forwarder could not relay
/// its connection (`unrelayed`) is the proxy's failure: `provider_error`,
/// `proxy_failed`, nothing against the site (F108). Anything else is as it
/// was.
fn blame_the_proxy(mut output: AttemptOutput, unrelayed: bool) -> AttemptOutput {
    if !unrelayed || output.status.is_some() || output.courtesy != Courtesy::ServerError {
        return output;
    }
    let mut evidence = std::mem::take(&mut output.classification.evidence);
    if let Some(reason) = &output.reason {
        evidence.push(format!("site_saw_nothing:{reason}"));
    }
    output.classification = unanswered(Outcome::ProviderError, PROXY_FAILED);
    output.classification.evidence.extend(evidence);
    output.courtesy = Courtesy::NotTheSite;
    output.reason = Some(PROXY_FAILED.to_owned());
    output
}

/// An attempt that got no answer from the site while the proxy refused a
/// tunnel of it to one of its own targets with a `4xx` (`refused`) ended
/// at that refusal: `provider_error`, `proxy_refused`, never retried,
/// nothing against the site and nothing against the proxy, which works and
/// only will not carry the target, as no proxy of its provider will (F189). The browser names such a refusal a failed proxy
/// (`proxy_failed`), or a failed navigation; anything that has the site's
/// answer is as it was.
fn blame_the_refusal(mut output: AttemptOutput, refused: Option<u16>) -> AttemptOutput {
    let Some(status) = refused else {
        return output;
    };
    let unanswered_by_the_site =
        output.courtesy == Courtesy::ServerError || output.reason.as_deref() == Some(PROXY_FAILED);
    if output.status.is_some() || !unanswered_by_the_site {
        return output;
    }
    let mut evidence = std::mem::take(&mut output.classification.evidence);
    if let Some(reason) = &output.reason {
        evidence.push(format!("site_saw_nothing:{reason}"));
    }
    output.classification = unanswered(Outcome::ProviderError, PROXY_REFUSED);
    output
        .classification
        .evidence
        .push(format!("proxy_refused_tunnel:{status}"));
    output.classification.evidence.extend(evidence);
    output.courtesy = Courtesy::NotTheSite;
    output.reason = Some(PROXY_REFUSED.to_owned());
    // The provider refuses the target for every proxy of its alike.
    output.finally()
}

/// An attempt its client reported as a failed proxy (`proxy_failed`), with
/// no answer of the site's, while a tunnel of it to one of its own targets
/// was open (`relayed`: opened while it ran, or kept open from an earlier
/// page, as a browser keeps it) and the forwarder noted no failure or
/// refusal of the proxy, ended at the site: the proxy did its part, and what ended
/// the page came through the tunnel from the site. Firefox names a site's
/// own `407` inside TLS a proxy error (QA round two's tls-03), and a
/// connection the site broke off is a failed connection to the client
/// (F170). It is the site's failure: `navigation_failed` for a browser's
/// page, as Chrome names the same page, `connection_failed` on the http
/// path, a server error's courtesy; the pool is not told. Anything else is
/// as it was.
fn blame_the_site(mut output: AttemptOutput, relayed: bool) -> AttemptOutput {
    if !relayed || output.status.is_some() || output.reason.as_deref() != Some(PROXY_FAILED) {
        return output;
    }
    let reason = if output.page.is_some() {
        "navigation_failed"
    } else {
        "connection_failed"
    };
    let retryable = output.classification.retryable;
    let mut evidence = std::mem::take(&mut output.classification.evidence);
    evidence.push(format!("tunnel_opened:{PROXY_FAILED}"));
    output.classification = unanswered(Outcome::ProviderError, reason);
    output.classification.evidence.extend(evidence);
    output.classification.retryable = retryable;
    output.courtesy = Courtesy::ServerError;
    output.reason = Some(reason.to_owned());
    output
}

/// The hosts and ports an attempt asked for itself: its page, the
/// redirects it followed and where it ended. A third party's host the page
/// loaded something from is none of them.
fn targets_of(url: &Url, output: &AttemptOutput) -> Vec<(String, u16)> {
    let hops = output
        .redirects
        .iter()
        .chain(output.final_url.iter())
        .filter_map(|hop| Url::parse(hop).ok());
    std::iter::once(url.clone())
        .chain(hops)
        .filter_map(|url| {
            let host = url.host_str()?.to_ascii_lowercase();
            Some((host, url.port_or_known_default()?))
        })
        .collect()
}

/// Whose failure an attempt of `caller` for `url` that began at `started`
/// was: the proxy's (`blame_the_proxy`) when the forwarder noted, while it
/// ran, that the proxy failed a connection of `caller` to one of the
/// attempt's own targets (`targets_of`); else the proxy's refusal
/// (`blame_the_refusal`) when it noted that the proxy refused a tunnel of
/// `caller` to one of them; else the site's (`blame_the_site`) when the
/// client named a failed proxy although a tunnel of it to one of them was
/// open.
fn whose_failure(
    meter: Option<&Meter>,
    output: AttemptOutput,
    caller: Caller,
    url: &Url,
    started: tokio::time::Instant,
) -> AttemptOutput {
    let Some(meter) = meter else {
        return output;
    };
    let targets = targets_of(url, &output);
    let targets = || targets.iter().map(|(host, port)| (host.as_str(), *port));
    if meter.unrelayed_since(caller, targets(), started) {
        return blame_the_proxy(output, true);
    }
    let refused = meter.refused_since(caller, targets(), started);
    if refused.is_some() {
        return blame_the_refusal(output, refused);
    }
    let relayed = meter.relayed_since(caller, targets(), started);
    blame_the_site(output, relayed)
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
            plain: None,
            plain_user_agent: config.egress.plain_user_agent().to_owned(),
            denials: None,
        }
    }

    /// The same executor reading what the forwarder refused (F102): an
    /// attempt that failed because the forwarder refused the address it
    /// went to by the SSRF guard ends `ssrf_denied`, not as a failed
    /// network.
    #[must_use]
    pub fn with_denials(mut self, denials: Arc<Denials>) -> Self {
        self.denials = Some(denials);
        self
    }

    /// The mark an attempt takes before it leaves.
    fn denials_mark(&self) -> Option<u64> {
        self.denials.as_ref().map(|denials| denials.mark())
    }

    /// The forwarder's refusal, since `mark`, of one of the hosts an
    /// attempt of `caller` went to.
    fn refused(&self, caller: Caller, mark: Option<u64>, hosts: &[String]) -> Option<Denial> {
        self.denials.as_ref()?.since(caller, mark?, hosts)
    }

    /// The guard's outcome for an attempt of the http path that failed,
    /// when the forwarder refused one of the addresses its client asked for
    /// since `mark` (F102). A failure does not say which hop it was: every
    /// address asked for is judged, the redirect's hops included, and the
    /// result names the last one refused.
    fn refused_hop(
        &self,
        caller: Caller,
        mark: Option<u64>,
        asked: &[Url],
    ) -> Option<AttemptOutput> {
        let hosts: Vec<String> = asked.iter().filter_map(host_of).collect();
        let denial = self.refused(caller, mark, &hosts)?;
        let mut output = denied_by_forwarder(&denial);
        output.final_url = asked
            .iter()
            .rev()
            .find(|url| host_of(url).as_deref() == Some(denial.host.as_str()))
            .map(Url::to_string);
        Some(output)
    }

    /// The after-the-fact address check of a browser's chain under
    /// `security.resolve_on_host` (spec §19, F57): every name of `hops` is
    /// resolved with the http path's own check
    /// ([`kage_http::check_target_addresses`]), and the first with an
    /// address in a denied range, or with none, is the refused hop. The
    /// forwarder refused such a name before a request to it left; this
    /// holds when its lookup and this one disagree, and for a page that
    /// arrived all the same. `None` without the setting: the proxy
    /// resolves.
    async fn denied_by_address(&self, hops: Vec<(usize, Url)>) -> Option<DeniedHop> {
        let resolver = self.manager.http().resolver()?;
        let mut judged = HashSet::new();
        for (hop, url) in hops {
            // What the URL check refuses was refused before this.
            let Ok(target) = check_url(&url, self.security) else {
                continue;
            };
            let Some(name) = target.name() else {
                continue;
            };
            if !judged.insert(name.to_owned()) {
                continue;
            }
            if let Err(reason) = kage_http::check_target_addresses(resolver.as_ref(), &target).await
            {
                tracing::warn!(
                    hop,
                    %url,
                    %reason,
                    "a browser reached a name the SSRF guard refuses by its addresses"
                );
                return Some(DeniedHop {
                    hop,
                    url: url.to_string(),
                    reason,
                });
            }
        }
        None
    }

    /// The same executor making plain attempts (level 0) through the
    /// forwarder of `egress`.
    #[must_use]
    pub fn with_plain(mut self, egress: ForwarderEgress) -> Self {
        self.plain = Some(egress);
        self
    }

    /// One attempt of a plain client: its caller's request as given, from
    /// its exit, through the forwarder.
    async fn plain_attempt(&self, job: &PlainJob) -> AttemptOutput {
        let Some(egress) = &self.plain else {
            return AttemptOutput::failed(
                Outcome::ProviderError,
                "plain_unavailable",
                Courtesy::NotTheSite,
            )
            .finally();
        };
        let endpoint = egress.plain_endpoint(job.route);
        let client = match self.manager.http().plain(&endpoint, &self.plain_user_agent) {
            Ok(client) => client,
            Err(error) => return Self::unavailable(&PersonaError::Http(error)),
        };
        let spec = PlainSpec {
            url: job.url.clone(),
            method: job.method.clone(),
            headers: job.headers.clone(),
            body: job.body.clone(),
            guard: Guard::Target(self.security),
            timeout: Some(job.timeout),
            max_body_bytes: job
                .max_body_bytes
                .and_then(|bytes| usize::try_from(bytes).ok()),
            max_redirects: None,
        };
        let exits = job.sites.as_deref().map(|sites| AttemptExits {
            executor: self,
            egress,
            sites,
            home: (
                host_of(&job.url)
                    .map(|host| kage_crawl::site::of_host(&host))
                    .unwrap_or_default(),
                job.route,
            ),
            used: Mutex::new(Vec::new()),
        });
        let mark = self.denials_mark();
        let mut asked = Vec::new();
        let routed = exits.as_ref().map(|exits| {
            let exits: &dyn HopExits = exits;
            exits
        });
        let fetched = client.fetch_routed(&spec, &mut asked, routed).await;
        let hop_routes = exits.as_ref().map(AttemptExits::routes).unwrap_or_default();
        let fetched = match fetched {
            Ok(fetched) => fetched,
            Err(error) => {
                // A hop the forwarder refused, under whichever exit it left by.
                return std::iter::once(job.route)
                    .chain(hop_routes)
                    .find_map(|route| self.refused_hop(Caller::Plain(route), mark, &asked))
                    .unwrap_or_else(|| Self::http_failure(&error));
            }
        };
        let classification = self.verdicts.classify(&Observation {
            render: Render::Http,
            status: fetched.status,
            url: fetched.final_url.to_string(),
            headers: fetched.headers.clone(),
            cookies: cookies_set(&fetched.headers),
            body: fetched.text(),
            document: job.method != http::Method::HEAD && !without_document(fetched.status),
            browser: None,
        });
        if classification.outcome.is_success()
            && let PlainRoute::Pool(proxy) = job.route
        {
            self.manager.pool().report_traffic_ok(proxy);
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
            reached: Some(Reached::Typed),
        }
    }

    /// Whose failure an attempt that got no answer from the site was: the
    /// proxy's, when the forwarder could not relay a connection of the
    /// caller to the attempt's own page because the proxy failed while the
    /// attempt ran (F108, `whose_failure`). A browser told a plain
    /// `http://` request that the forwarder had to close without an answer
    /// reports `navigation_failed`, as for a site that does not answer.
    /// The pool is told, so that the proxy is probed now.
    fn whose(
        &self,
        output: AttemptOutput,
        (caller, url): (Caller, &Url),
        proxy: Option<ProxyId>,
        started: tokio::time::Instant,
    ) -> AttemptOutput {
        let output = whose_failure(self.meter.as_deref(), output, caller, url, started);
        if output.reason.as_deref() == Some(PROXY_FAILED)
            && let Some(proxy) = proxy
        {
            self.manager.pool().report_traffic_failed(proxy);
        }
        output
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
        let mut reached = Reached::Typed;
        if !job.starts_visit
            && !has_referer
            && arrives_as(job.policy.referer_strategy) == RefererStrategy::Direct
        {
            // A later page of a visit arrives from a page before it: the
            // one that holds the link to it when the attempt names one,
            // else the last one read.
            let from = match &job.arrival {
                Some(Arrival::Link { from, .. }) => Some(from.clone()),
                // Nothing it read leads here: it comes from nowhere.
                Some(Arrival::Typed) => None,
                None => self.last_page(persona.id, &job.domain),
            };
            if let Some(previous) = from {
                headers.push(("Referer".to_owned(), previous.to_string()));
                reached = Reached::Link { backs: None };
            }
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
        let mark = self.denials_mark();
        let mut asked = Vec::new();
        let fetched = match client.fetch_traced(&fetch, &mut asked).await {
            Ok(fetched) => fetched,
            Err(error) => {
                return self
                    .refused_hop(Caller::Persona(persona.id), mark, &asked)
                    .unwrap_or_else(|| Self::http_failure(&error));
            }
        };

        let classification = self.verdicts.classify(&Observation {
            render: Render::Http,
            status: fetched.status,
            url: fetched.final_url.to_string(),
            headers: fetched.headers.clone(),
            cookies: cookies_set(&fetched.headers),
            body: fetched.text(),
            document: job.method != http::Method::HEAD && !without_document(fetched.status),
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
            reached: Some(reached),
        }
    }

    fn http_failure(error: &HttpError) -> AttemptOutput {
        tracing::debug!(error = %loggable(error), "an http attempt failed");
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
            HttpError::Tls(_) | HttpError::Request(_) | HttpError::Body(_) => {
                AttemptOutput::failed(
                    Outcome::ProviderError,
                    "connection_failed",
                    Courtesy::ServerError,
                )
            }
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
            // A hop to another site whose own exit cannot be had now (F166):
            // nothing of the hop was asked for, and no other address may.
            HttpError::HopExitUnavailable { .. } => AttemptOutput::failed(
                Outcome::ProviderError,
                "hop_exit_unavailable",
                Courtesy::NotTheSite,
            ),
            HttpError::Profile(_) | HttpError::Build(_) | HttpError::Cookies(_) => {
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
        let mark = self.denials_mark();
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
                    opened_tabs,
                    paced_ms,
                } => steps.push(FlowStep {
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    action,
                    ok,
                    duration_ms,
                    paced_ms,
                    error,
                    capture: capture.map(Box::new),
                    opened_tabs: opened_tabs.unwrap_or_default(),
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
        // A step or the end that did not arrive because the forwarder
        // refused its address is the guard's refusal too (F102).
        let arrived = ok && page.error.is_none() && page.status.is_some();
        let refused = if arrived {
            None
        } else {
            let mut hosts = hosts_of(&page);
            for seen in steps.iter().filter_map(|step| step.capture.as_deref()) {
                hosts.extend(hosts_of(seen));
            }
            for action in &plan.actions {
                if let FlowAction::Goto { url, .. } = action {
                    hosts.extend(Url::parse(url).ok().as_ref().and_then(host_of));
                }
            }
            self.refused(Caller::Persona(job.persona.id), mark, &hosts)
        };
        if let Some(denial) = refused {
            ssrf::drop_captures(&mut page);
            let mut output = denied_by_forwarder(&denial);
            output.final_url = Some(page.final_url.clone());
            output.timing = timing_of(&page);
            output.page = Some(page);
            output.behavior = Some(behavior);
            return Err(Box::new(output));
        }
        // Under `security.resolve_on_host` every name the flow's pages
        // reached is judged by its addresses too (F57).
        let denied = if let Some(denied) = denied {
            Some(denied)
        } else {
            self.denied_by_address(flow_hops(&steps, &page)).await
        };
        if let Some(denied) = denied {
            tracing::warn!(
                context = %context,
                hop = denied.hop,
                reason = %denied.reason,
                "a flow visited what the SSRF guard refuses"
            );
            return Err(Box::new(denied_hop(denied, page, behavior)));
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
        let (lease, moved) = match self.manager.open_browser(persona, &endpoint).await {
            Ok(opened) => opened,
            Err(error) => return Self::unavailable(&error),
        };
        // A persona whose engine runs a newer browser goes on as it was
        // moved and stored.
        let persona = moved.as_ref().unwrap_or(persona);
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
        let (mut page, behavior, flow) = match loaded {
            Ok(loaded) => loaded,
            // The browser is in a state nobody knows, or was somewhere it
            // must not be: it is closed.
            Err(output) => {
                lease.close().await;
                return *output;
            }
        };

        // An answer that holds no page (no content, a file the browser
        // saved) is the site's: the document the tab still shows is not
        // its, and nothing of it is kept or read (F116, F120).
        let without_page = no_document(&page);
        if without_page.is_some() {
            page.dom_html = None;
            page.links = None;
            page.links_truncated = false;
        }
        // The bytes a browser handed over of a file it saved are the file's
        // (Firefox: a gzipped sitemap is read from them); a 204's are none.
        if without_page == Some(NoDocument::Empty) {
            page.raw_body = None;
        }
        let failure = page_failure(&page);
        let mut classification = self.verdicts.judge_page(&page);
        let mut reason = failure
            .as_ref()
            .filter(|_| !classification.outcome.is_block())
            .map(|failure| failure.reason.to_owned());
        if without_page == Some(NoDocument::File) {
            reason = Some("download".to_owned());
        }
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
            reached: job.flow.is_none().then(|| reached_of(&page)),
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
        let mark = self.denials_mark();
        let navigating =
            sidecar.navigate_checked(context, &request, self.security, self.max_redirects);
        // The attempt is waited for as long as this page may take: the time
        // to reach it, the patience drawn for it and its dwell. No page is
        // left after a time that is the same for every page.
        let wall = navigation_bound(&request) + NAVIGATION_GRACE;
        let navigation = match tokio::time::timeout(wall, navigating).await {
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
            Navigation::Page(mut page) => {
                // A page that did not arrive because the forwarder refused
                // its address is the guard's refusal, not a failed network.
                let arrived = page.error.is_none() && page.status.is_some();
                if !arrived
                    && let Some(denial) =
                        self.refused(Caller::Persona(job.persona.id), mark, &hosts_of(&page))
                {
                    ssrf::drop_captures(&mut page);
                    let mut output = denied_by_forwarder(&denial);
                    output.final_url = Some(page.final_url.clone());
                    output.timing = timing_of(&page);
                    output.page = Some(page);
                    output.behavior = Some(behavior);
                    return Err(Box::new(output));
                }
                // Under `security.resolve_on_host` every name of the chain is
                // judged by its addresses too, as the http path judges its
                // hops (F57).
                if let Some(denied) = self.denied_by_address(ssrf::chain_hops(&page)).await {
                    return Err(Box::new(denied_hop(denied, page, behavior)));
                }
                Ok((page, behavior))
            }
            Navigation::SsrfDenied { denied, result } => {
                Err(Box::new(denied_hop(denied, result, behavior)))
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
        let started = tokio::time::Instant::now();
        let output = match job.render {
            Render::Http => self.http(job).await,
            Render::Browser => self.browser(job).await,
        };
        self.whose(
            output,
            (Caller::Persona(job.persona.id), &job.url),
            Some(job.persona.proxy_id),
            started,
        )
    }

    async fn plain(&self, job: &PlainJob) -> AttemptOutput {
        // A plain client's bytes count toward the host they went to, at
        // the forwarder.
        let started = tokio::time::Instant::now();
        let output = self.plain_attempt(job).await;
        let proxy = match job.route {
            PlainRoute::Pool(proxy) => Some(proxy),
            PlainRoute::Host => None,
        };
        self.whose(output, (Caller::Plain(job.route), &job.url), proxy, started)
    }
}

/// An http attempt's error as it goes into the log: a redirect target the
/// server sent is named by its origin only, never with its path or query,
/// where signed URLs and tokens travel (F144, secrecy-12). Every other
/// error names no target the caller did not give.
fn loggable(error: &HttpError) -> String {
    match error {
        HttpError::RedirectDenied(denied) => {
            // A location that does not parse is named in its reason whole.
            let reason = match &denied.reason {
                SsrfDenial::InvalidUrl(_) => "the location is not a valid URL".to_owned(),
                other => other.to_string(),
            };
            format!(
                "redirect hop {} to {} denied: {reason}",
                denied.hop,
                sent_origin(&denied.location),
            )
        }
        HttpError::TooManyRedirects { max, location } => format!(
            "more than {max} redirects; the last one pointed at {}",
            sent_origin(location)
        ),
        HttpError::HopExitUnavailable { url, reason } => format!(
            "the redirect to {} was not followed: {reason}",
            sent_origin(url)
        ),
        other => other.to_string(),
    }
}

/// The scheme, host and port of a URL a server sent, or a placeholder for
/// one that has none.
fn sent_origin(location: &str) -> String {
    Url::parse(location)
        .ok()
        .and_then(|url| origin_of(&url))
        .unwrap_or_else(|| "a location without a host".to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_browser::contract::{
        DwellMode, EXAMPLES_JSON, EntryFallback, EntryStrategy, PageResult,
    };
    use kage_core::config::BrowserConfig;
    use kage_core::outcome::Outcome;
    use kage_core::policy::{DomainPolicies, EffectivePolicy, RefererStrategy};
    use kage_core::schedule::politeness::Courtesy;
    use kage_core::seed::Seed;
    use kage_http::{HttpError, Stage};
    use url::Url;

    use super::{
        AttemptOutput, BehaviorWish, PathExecutor, Reached, behavior_for, blame_the_proxy,
        denied_by_forwarder, draw_dwell, entry_from, hosts_of, origin_of, reached_of,
        whose_failure,
    };
    use crate::forwarder::{Caller, Denials, PlainRoute};

    #[test]
    fn a_redirect_targets_path_and_query_never_reach_the_log() {
        // F144 (secrecy-12): at debug a failed http attempt is logged; the
        // hop it names keeps its origin only.
        use kage_core::ssrf::{RedirectDenied, SsrfDenial};
        let denied = HttpError::RedirectDenied(RedirectDenied {
            hop: 1,
            location: "http://192.0.2.1/cb?token=secret-value".to_owned(),
            reason: SsrfDenial::Scheme("ftp".to_owned()),
        });
        let too_many = HttpError::TooManyRedirects {
            max: 10,
            location: "https://shop.kage.test:8443/next?tok=secret-value".to_owned(),
        };
        let unparsed = HttpError::RedirectDenied(RedirectDenied {
            hop: 2,
            location: "http://[::1/x?token=secret-value".to_owned(),
            reason: SsrfDenial::InvalidUrl("http://[::1/x?token=secret-value".to_owned()),
        });
        let hop = HttpError::HopExitUnavailable {
            url: "https://other.kage.test/a/b?token=secret-value".to_owned(),
            reason: "its exit cannot carry traffic now".to_owned(),
        };
        let lines: Vec<String> = [&denied, &too_many, &unparsed, &hop]
            .into_iter()
            .map(super::loggable)
            .collect();
        assert_eq!(
            lines,
            [
                "redirect hop 1 to http://192.0.2.1 denied: scheme \"ftp\" is not allowed, only http and https are",
                "more than 10 redirects; the last one pointed at https://shop.kage.test:8443",
                "redirect hop 2 to a location without a host denied: the location is not a valid URL",
                "the redirect to https://other.kage.test was not followed: its exit cannot carry traffic now",
            ]
        );
        for (error, line) in [&denied, &too_many, &unparsed, &hop]
            .into_iter()
            .zip(&lines)
        {
            assert!(error.to_string().contains("secret-value"));
            assert!(
                !line.contains("secret-value") && !line.contains("/cb"),
                "{line}"
            );
        }
        assert_eq!(
            super::loggable(&HttpError::Request("reset")),
            HttpError::Request("reset").to_string()
        );
    }

    /// The real path of F108: a forwarder on loopback in front of one
    /// proxy per persona, its meter, and `whose_failure`.
    mod at_the_proxy {
        use std::collections::HashSet;
        use std::net::SocketAddr;
        use std::sync::Arc;
        use std::time::Duration;

        use async_trait::async_trait;
        use base64::Engine as _;
        use kage_core::domain::{ProxyEndpoint, ProxyScheme};
        use kage_core::ids::{PersonaId, ProxyId};
        use kage_core::outcome::Outcome;
        use kage_core::schedule::politeness::Courtesy;
        use kage_core::secret::Secret;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        use url::Url;

        use super::{AttemptOutput, whose_failure};
        use crate::forwarder::{
            Caller, Forwarder, ForwarderSettings, PlainRoute, RouteRefusal, Routes, SystemResolver,
            Tokens, Upstream,
        };

        const KEY: [u8; 32] = [9; 32];

        /// Persona `dead` leaves through a proxy nobody listens on;
        /// persona `refusing` through one that answers every tunnel with a
        /// status of its own (`refusing_proxy`).
        #[derive(Debug)]
        struct Two {
            dead: (PersonaId, ProxyEndpoint),
            refusing: (PersonaId, ProxyEndpoint),
        }

        #[async_trait]
        impl Routes for Two {
            async fn upstream(&self, persona: PersonaId) -> Result<Upstream, RouteRefusal> {
                [&self.dead, &self.refusing]
                    .into_iter()
                    .find(|(one, _)| *one == persona)
                    .map(|(_, endpoint)| Upstream::Proxy(endpoint.clone()))
                    .ok_or(RouteRefusal::UnknownPersona)
            }

            async fn plain(&self, _: PlainRoute) -> Result<Upstream, RouteRefusal> {
                Err(RouteRefusal::NotConfigured)
            }
        }

        fn endpoint(at: SocketAddr) -> ProxyEndpoint {
            ProxyEndpoint {
                id: ProxyId::new(),
                host: at.ip().to_string(),
                port: at.port(),
                username: Secret::new("user"),
                password: Secret::new("pass"),
                scheme: ProxyScheme::Http,
            }
        }

        /// A proxy that cannot reach the target, as it says to every tunnel.
        const BAD_GATEWAY: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n";

        /// A provider that will not carry the target, as it says to every tunnel.
        const FORBIDDEN: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n";

        /// A proxy that opens every tunnel, to a site that hangs up at once.
        const OPENED: &[u8] = b"HTTP/1.1 200 Connection established\r\n\r\n";

        /// A proxy that reads each request head and answers `answer`.
        async fn refusing_proxy(answer: &'static [u8]) -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let at = listener.local_addr().unwrap();
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let mut head = Vec::new();
                        let mut byte = [0_u8; 1];
                        while !head.ends_with(b"\r\n\r\n") {
                            match stream.read(&mut byte).await {
                                Ok(1) => head.push(byte[0]),
                                _ => return,
                            }
                        }
                        let _ = stream.write_all(answer).await;
                    });
                }
            });
            at
        }

        /// An address nobody listens on, and the socket that holds it: bound
        /// but never listening, so a connection to it is refused and no other
        /// test's listener takes its port while the test runs (a port merely
        /// freed could be, and the dead proxy would answer).
        fn dead_proxy() -> (SocketAddr, tokio::net::TcpSocket) {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
            (socket.local_addr().unwrap(), socket)
        }

        /// Sends `head` through the forwarder as `persona` and reads
        /// whatever comes back until the forwarder closes.
        async fn ask(forwarder: &Forwarder, persona: PersonaId, head: &str) -> String {
            let tokens = Tokens::new(&KEY);
            let credential = base64::engine::general_purpose::STANDARD
                .encode(format!("{persona}:{}", tokens.token(persona)));
            let mut client = TcpStream::connect(forwarder.local_addr()).await.unwrap();
            let request = head.replace(
                "\r\n\r\n",
                &format!("\r\nProxy-Authorization: Basic {credential}\r\n\r\n"),
            );
            client.write_all(request.as_bytes()).await.unwrap();
            let mut answer = Vec::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut answer)).await;
            String::from_utf8_lossy(&answer).into_owned()
        }

        fn unanswered(outcome: Outcome, reason: &str) -> AttemptOutput {
            AttemptOutput::failed(outcome, reason, Courtesy::ServerError)
        }

        /// A forwarder on loopback over `routes`, serving until aborted.
        async fn forwarder(routes: Arc<Two>) -> (Arc<Forwarder>, tokio::task::JoinHandle<()>) {
            let settings = ForwarderSettings {
                advertised: ("kage".to_owned(), 0),
                connect_timeout: Duration::from_secs(2),
                idle_timeout: Duration::from_secs(5),
                max_connections: 16,
                operator_hosts: HashSet::new(),
                private_names: HashSet::new(),
                resolver: Arc::new(SystemResolver),
                resolve_names: false,
            };
            let (forwarder, listener) = Forwarder::bind(
                "127.0.0.1:0".parse().unwrap(),
                settings,
                Tokens::new(&KEY),
                routes,
            )
            .await
            .unwrap();
            let served = Arc::clone(&forwarder);
            let serving = tokio::spawn(async move {
                forwarder
                    .serve(listener, std::future::pending::<()>())
                    .await;
            });
            (served, serving)
        }

        #[tokio::test]
        async fn only_a_proxy_that_failed_the_attempts_own_page_is_blamed() {
            let (dead, refusing) = (PersonaId::new(), PersonaId::new());
            let (gone, _held) = dead_proxy();
            let (forwarder, serving) = forwarder(Arc::new(Two {
                dead: (dead, endpoint(gone)),
                refusing: (refusing, endpoint(refusing_proxy(BAD_GATEWAY).await)),
            }))
            .await;
            let meter = Arc::clone(forwarder.meter());
            let page = Url::parse("http://shop.kage.test/a").unwrap();

            // The true positive: the page's plain request reached no proxy,
            // and the forwarder closed it without an answer.
            let started = tokio::time::Instant::now();
            let answer = ask(
                &forwarder,
                dead,
                "GET http://shop.kage.test/a HTTP/1.1\r\nHost: shop.kage.test\r\n\r\n",
            )
            .await;
            assert_eq!(answer, "");
            let output = whose_failure(
                Some(&meter),
                unanswered(Outcome::ProviderError, "navigation_failed"),
                Caller::Persona(dead),
                &page,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("proxy_failed"));
            assert_eq!(output.courtesy, Courtesy::NotTheSite);
            // Its page on another host is not blamed for it.
            let other = Url::parse("http://other.kage.test/").unwrap();
            let output = whose_failure(
                Some(&meter),
                unanswered(Outcome::Timeout, "timeout"),
                Caller::Persona(dead),
                &other,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("timeout"));
            assert_eq!(output.courtesy, Courtesy::ServerError);

            // The false positive: the proxy answered a tunnel to a third
            // party's host, and to the page's own, with `502`. The page timed
            // out: that is the site's, not the proxy's, and no refusal of
            // the target either (F189).
            let started = tokio::time::Instant::now();
            for authority in ["cdn.kage.test:443", "shop.kage.test:443"] {
                let answer = ask(
                    &forwarder,
                    refusing,
                    &format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"),
                )
                .await;
                assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
            }
            let secure = Url::parse("https://shop.kage.test/a").unwrap();
            for url in [&page, &secure] {
                let output = whose_failure(
                    Some(&meter),
                    unanswered(Outcome::Timeout, "timeout"),
                    Caller::Persona(refusing),
                    url,
                    started,
                );
                assert_eq!(output.reason.as_deref(), Some("timeout"), "{url}");
                assert_eq!(output.courtesy, Courtesy::ServerError, "{url}");
            }
            // And another persona's failure is never this one's.
            let output = whose_failure(
                Some(&meter),
                unanswered(Outcome::ProviderError, "navigation_failed"),
                Caller::Persona(refusing),
                &page,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("navigation_failed"));
            serving.abort();
        }

        /// QA round two's tls-03 and tls-01 (F170): the proxy opened the
        /// page's own tunnel, and the client still named a failed proxy:
        /// Firefox does for a site's own `407` inside TLS. That was the
        /// site's answer, a failed navigation (a failed connection on the
        /// http path) with a server error's courtesy, and the pool is not
        /// told; a tunnel to another target, or another persona's, changes
        /// nothing, nor does an answer the site gave.
        #[tokio::test]
        async fn a_failed_proxy_whose_tunnel_was_open_was_the_sites_answer() {
            let (dead, opening) = (PersonaId::new(), PersonaId::new());
            let (gone, _held) = dead_proxy();
            let (forwarder, serving) = forwarder(Arc::new(Two {
                dead: (dead, endpoint(gone)),
                refusing: (opening, endpoint(refusing_proxy(OPENED).await)),
            }))
            .await;
            let meter = Arc::clone(forwarder.meter());
            let secure = Url::parse("https://www.tlsf4.test/p407").unwrap();
            let proxy_failed = || {
                AttemptOutput::failed(Outcome::ProviderError, "proxy_failed", Courtesy::NotTheSite)
            };

            let started = tokio::time::Instant::now();
            // Before any tunnel: the proxy's, as it was.
            let output = whose_failure(
                Some(&meter),
                proxy_failed(),
                Caller::Persona(opening),
                &secure,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("proxy_failed"));
            let answer = ask(
                &forwarder,
                opening,
                "CONNECT www.tlsf4.test:443 HTTP/1.1\r\nHost: www.tlsf4.test:443\r\n\r\n",
            )
            .await;
            assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

            // A browser's page.
            let mut browser = proxy_failed();
            let examples: serde_json::Value =
                serde_json::from_str(kage_browser::contract::EXAMPLES_JSON).unwrap();
            browser.page = Some(Box::new(
                serde_json::from_value::<kage_browser::contract::PageResult>(
                    examples["PageResult"][0].clone(),
                )
                .unwrap(),
            ));
            let output = whose_failure(
                Some(&meter),
                browser,
                Caller::Persona(opening),
                &secure,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("navigation_failed"));
            assert_eq!(output.classification.outcome, Outcome::ProviderError);
            assert_eq!(output.courtesy, Courtesy::ServerError);
            assert!(
                output
                    .classification
                    .evidence
                    .contains(&"tunnel_opened:proxy_failed".to_owned()),
                "{:?}",
                output.classification.evidence
            );
            // The http path.
            let output = whose_failure(
                Some(&meter),
                proxy_failed(),
                Caller::Persona(opening),
                &secure,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("connection_failed"));
            assert_eq!(output.courtesy, Courtesy::ServerError);

            // Another target, another persona, an attempt that began after
            // the tunnel closed, an answer: nothing changes. The forwarder
            // lets go of the tunnel just after the client saw it close.
            let mut after = tokio::time::Instant::now();
            for _ in 0..100 {
                if !meter.relayed_since(Caller::Persona(opening), [("www.tlsf4.test", 443)], after)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                after = tokio::time::Instant::now();
            }
            for (caller, url, since) in [
                (opening, "https://other.tlsf4.test/", started),
                (dead, "https://www.tlsf4.test/p407", started),
                (opening, "https://www.tlsf4.test/p407", after),
            ] {
                let output = whose_failure(
                    Some(&meter),
                    proxy_failed(),
                    Caller::Persona(caller),
                    &Url::parse(url).unwrap(),
                    since,
                );
                assert_eq!(output.reason.as_deref(), Some("proxy_failed"), "{url}");
            }
            let mut answered = proxy_failed();
            answered.status = Some(407);
            let output = whose_failure(
                Some(&meter),
                answered,
                Caller::Persona(opening),
                &secure,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("proxy_failed"));
            serving.abort();
        }

        #[tokio::test]
        async fn a_page_whose_own_tunnel_the_proxy_refused_ended_at_the_refusal() {
            let (dead, refusing) = (PersonaId::new(), PersonaId::new());
            let (gone, _held) = dead_proxy();
            let (forwarder, serving) = forwarder(Arc::new(Two {
                dead: (dead, endpoint(gone)),
                refusing: (refusing, endpoint(refusing_proxy(FORBIDDEN).await)),
            }))
            .await;
            let meter = Arc::clone(forwarder.meter());
            let page = Url::parse("http://shop.kage.test/a").unwrap();
            let secure = Url::parse("https://shop.kage.test/a").unwrap();

            // The provider refused a tunnel to a third party's host, and to
            // the page's own, with `403`. That is no failure of the proxy: a
            // page whose own tunnel was refused ended at the proxy's
            // refusal, nothing against the site or the proxy and no other
            // persona asked (F189); a page on another port timed out by
            // itself.
            let started = tokio::time::Instant::now();
            for authority in ["cdn.kage.test:443", "shop.kage.test:443"] {
                let answer = ask(
                    &forwarder,
                    refusing,
                    &format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n"),
                )
                .await;
                assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
            }
            let output = whose_failure(
                Some(&meter),
                unanswered(Outcome::Timeout, "timeout"),
                Caller::Persona(refusing),
                &page,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("timeout"));
            assert_eq!(output.courtesy, Courtesy::ServerError);
            // The browser names the refused tunnel a failed proxy, or a
            // failed navigation: either is the proxy's refusal.
            for failed in [
                AttemptOutput::failed(Outcome::ProviderError, "proxy_failed", Courtesy::NotTheSite),
                unanswered(Outcome::ProviderError, "navigation_failed"),
                unanswered(Outcome::Timeout, "timeout"),
            ] {
                let named = failed.reason.clone();
                let output = whose_failure(
                    Some(&meter),
                    failed,
                    Caller::Persona(refusing),
                    &secure,
                    started,
                );
                assert_eq!(
                    output.classification.outcome,
                    Outcome::ProviderError,
                    "{named:?}"
                );
                assert_eq!(output.reason.as_deref(), Some("proxy_refused"), "{named:?}");
                assert_eq!(output.courtesy, Courtesy::NotTheSite, "{named:?}");
                assert!(!output.classification.retryable, "{named:?}");
                assert!(
                    output
                        .classification
                        .evidence
                        .contains(&"proxy_refused_tunnel:403".to_owned()),
                    "{:?}",
                    output.classification.evidence
                );
            }
            // A page that has the site's answer is the site's all the same.
            let mut answered = unanswered(Outcome::ProviderError, "navigation_failed");
            answered.status = Some(503);
            let output = whose_failure(
                Some(&meter),
                answered,
                Caller::Persona(refusing),
                &secure,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("navigation_failed"));
            // Another persona's tunnel is never this one's.
            let output = whose_failure(
                Some(&meter),
                unanswered(Outcome::ProviderError, "navigation_failed"),
                Caller::Persona(dead),
                &secure,
                started,
            );
            assert_eq!(output.reason.as_deref(), Some("navigation_failed"));
            serving.abort();
        }
    }

    fn policy(yaml: &str) -> EffectivePolicy {
        DomainPolicies::from_yaml(yaml)
            .unwrap()
            .resolve("example.org")
    }

    #[test]
    fn the_sidecar_answers_a_flow_before_kage_stops_waiting_for_it() {
        // The sidecar's own number, read from its source: how much longer
        // than a flow's budget its server lets the flow run (F103).
        let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../sidecar/packages/server/src/app.ts");
        let source = std::fs::read_to_string(app).unwrap();
        let slack_ms: u64 = source
            .lines()
            .find_map(|line| line.strip_prefix("export const FLOW_SLACK_MS = "))
            .and_then(|rest| rest.strip_suffix(';'))
            .map(|number| number.replace('_', ""))
            .and_then(|number| number.parse().ok())
            .unwrap();
        // At least 5 s left for the typed answer and the closed context
        // before the executor gives up and a retry may find it busy.
        assert!(slack_ms > 0);
        assert!(
            Duration::from_millis(slack_ms) + Duration::from_secs(5) <= super::BROWSER_GRACE,
            "the sidecar bounds a flow {slack_ms} ms past its budget, Kage waits {:?}",
            super::BROWSER_GRACE
        );
    }

    /// QA round one's policy-10 (F143): a policy that names the search
    /// engine strategy, which no results page can serve yet, gets a typed
    /// visit, and what applied says `direct`, never `search_engine`.
    #[test]
    fn a_search_entry_that_cannot_be_made_is_typed_and_says_so() {
        let browser = BrowserConfig::default();
        let seed = Seed::from_u64(5);
        let searched = policy("domains:\n  example.org: { referer_strategy: search_engine }\n");
        assert_eq!(searched.referer_strategy, RefererStrategy::SearchEngine);
        let (behavior, applied) = behavior_for(&searched, &browser, &seed, 1.0, None);
        assert_eq!(behavior.entry.unwrap().strategy, EntryStrategy::Direct);
        assert_eq!(applied.referer_strategy, RefererStrategy::Direct);
        // The other strategies are what they say.
        assert_eq!(
            super::arrives_as(RefererStrategy::None),
            RefererStrategy::None
        );
        assert_eq!(
            super::arrives_as(RefererStrategy::Direct),
            RefererStrategy::Direct
        );
    }

    #[test]
    fn a_browser_render_asks_for_human_behavior() {
        let browser = BrowserConfig::default();
        let seed = Seed::from_u64(5);

        let (behavior, applied) = behavior_for(&policy(""), &browser, &seed, 1.0, None);

        assert_eq!(behavior.dwell, Some(DwellMode::Human));
        assert_eq!(behavior.wander, Some(true));
        assert_eq!(behavior.seed.as_deref(), Some(seed.to_string().as_str()));
        let entry = behavior.entry.clone().unwrap();
        assert_eq!(entry.strategy, EntryStrategy::Direct);
        assert_eq!((entry.from_url, entry.fallback), (None, None));
        assert_eq!((entry.from_open_page, entry.max_backs), (None, None));
        // What was asked for is what is reported, and it was the policy's.
        assert!(!applied.from_request && !applied.request_ignored);
        assert_eq!(applied.scroll_depth, behavior.scroll_depth);
        assert_eq!(applied.dwell, None);
        assert_eq!(applied.referer_strategy, RefererStrategy::Direct);
        assert_eq!(applied.seed, seed);
        // The same seed scrolls as deep every time; other seeds cover the
        // range and never leave it.
        assert_eq!(
            behavior,
            behavior_for(&policy(""), &browser, &seed, 1.0, None).0
        );
        let depths: Vec<f64> = (0..400)
            .map(|n| {
                behavior_for(&policy(""), &browser, &Seed::from_u64(n), 1.0, None)
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
            behavior_for(&none, &browser, &seed, 1.0, None)
                .0
                .entry
                .unwrap()
                .strategy,
            EntryStrategy::None
        );
        let search = policy("defaults:\n  referer_strategy: search_engine\n");
        assert_eq!(
            behavior_for(&search, &browser, &seed, 1.0, None)
                .0
                .entry
                .unwrap()
                .strategy,
            EntryStrategy::Direct
        );
        // A wish that asks for nothing is no wish.
        let (empty, applied) = behavior_for(
            &policy(""),
            &browser,
            &seed,
            1.0,
            Some(&BehaviorWish::default()),
        );
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

        let (behavior, applied) = behavior_for(&policy(""), &browser, &seed, 1.0, Some(&wish));

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
        let (behavior, applied) = behavior_for(&policy(""), &browser, &seed, 1.0, Some(&still));
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
            behavior_for(&policy(""), &browser, &seed, 1.0, Some(&deep))
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

        let (behavior, applied) = behavior_for(&locked, &browser, &seed, 1.0, Some(&wish));

        assert_eq!(
            behavior,
            behavior_for(&locked, &browser, &seed, 1.0, None).0
        );
        assert_eq!(behavior.dwell, Some(DwellMode::Human));
        assert!((0.6..0.9).contains(&behavior.scroll_depth.unwrap()));
        assert!(applied.request_ignored && !applied.from_request);
        assert_eq!(applied.referer_strategy, RefererStrategy::Direct);

        // Without the lock the policy's own floor holds.
        let floored = policy("defaults:\n  min_dwell: 5s\n");
        let (_, applied) = behavior_for(&floored, &browser, &seed, 1.0, Some(&wish));
        assert!(applied.dwell.unwrap() >= Duration::from_secs(5));
        assert!(!applied.request_ignored);
    }

    #[test]
    fn every_page_is_given_a_patience_of_its_own_at_the_personas_pace() {
        let browser = BrowserConfig::default();
        let seed = Seed::from_u64(5);

        let (behavior, applied) = behavior_for(&policy(""), &browser, &seed, 1.0, None);

        // What the sidecar is asked for is what is recorded with the
        // attempt, and it is the draw of the attempt's seed: the visit
        // replays.
        let patience = applied.patience.unwrap();
        assert_eq!(
            behavior.patience_ms,
            u64::try_from(patience.as_millis()).ok()
        );
        assert_eq!(patience, browser.patience.draw(1.0, &seed));
        assert_eq!(
            applied,
            behavior_for(&policy(""), &browser, &seed, 1.0, None).1
        );
        // A slower persona has more patience with the same page.
        let slower = behavior_for(&policy(""), &browser, &seed, 1.25, None).1;
        assert!(slower.patience.unwrap() > patience);
        // It is nobody's wish: a request that asks for the whole page and
        // a short dwell has the same patience, and so has a locked policy.
        let wish = BehaviorWish {
            scroll_depth: Some(1.0),
            dwell: Some(Duration::from_secs(1)),
            referer_strategy: None,
        };
        let wished = behavior_for(&policy(""), &browser, &seed, 1.0, Some(&wish)).1;
        assert_eq!(wished.patience, Some(patience));
        let locked = policy("defaults:\n  behavior_locked: true\n");
        let kept = behavior_for(&locked, &browser, &seed, 1.0, Some(&wish)).1;
        assert_eq!(kept.patience, Some(patience));
        // Even a page that is not scrolled has one: with it, the time to
        // reach the page bounds nothing else.
        let still = BehaviorWish {
            scroll_depth: Some(0.0),
            ..BehaviorWish::default()
        };
        let (unscrolled, _) = behavior_for(&policy(""), &browser, &seed, 1.0, Some(&still));
        assert_eq!(unscrolled.scroll_depth, None);
        assert_eq!(unscrolled.patience_ms, behavior.patience_ms);

        // Pages are not given up after the same time (two of three hundred
        // may share a millisecond by chance; there is no pile anywhere), and
        // none outside what the configuration allows.
        let mut drawn: Vec<Duration> = (0..300)
            .map(|n| {
                behavior_for(&policy(""), &browser, &Seed::from_u64(n), 1.0, None)
                    .1
                    .patience
                    .unwrap()
            })
            .collect();
        drawn.sort_unstable();
        assert!(drawn[0] > Duration::from_secs(6), "{:?}", drawn[0]);
        assert!(drawn[299] <= Duration::from_mins(4), "{:?}", drawn[299]);
        drawn.dedup();
        assert!(drawn.len() >= 297, "{}", drawn.len());
    }

    #[test]
    fn a_page_the_persona_has_been_reading_is_left_by_a_click_on_its_link() {
        let entry = entry_from(&Url::parse("https://example.org/list?page=2").unwrap(), 3);

        assert_eq!(entry.strategy, EntryStrategy::Link);
        assert_eq!(
            entry.from_url.as_deref(),
            Some("https://example.org/list?page=2")
        );
        // Nothing is loaded to start from: the tab holds the page or it
        // does not, and then the address is typed. A crawl never stops at
        // a link that is gone.
        assert_eq!(
            (entry.from_open_page, entry.max_backs),
            (Some(true), Some(3))
        );
        assert_eq!(entry.fallback, Some(EntryFallback::None));
    }

    #[test]
    fn how_a_page_was_reached_is_what_the_browser_says_it_did() {
        let examples: serde_json::Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
        let reached = |entry: serde_json::Value| {
            let mut page = examples["PageResult"][0].clone();
            page["behavior"]["entry"] = entry;
            reached_of(&serde_json::from_value::<PageResult>(page).unwrap())
        };
        let entry = |strategy: &str, outcome: &str, backs: Option<u64>| {
            serde_json::json!({
                "strategy": strategy,
                "outcome": outcome,
                "from_url": "https://example.org/list",
                "referer": "https://example.org/list",
                "backs": backs,
            })
        };

        // A click on the open page, and one after going back twice.
        assert_eq!(
            reached(entry("link", "arrived", Some(0))),
            Reached::Link { backs: Some(0) }
        );
        assert_eq!(
            reached(entry("link", "arrived", Some(2))),
            Reached::Link { backs: Some(2) }
        );
        // A link on a page that was loaded to start from: no going back.
        assert_eq!(
            reached(entry("link", "arrived", None)),
            Reached::Link { backs: Some(0) }
        );
        // The link was not there and the address was typed; so it was for
        // an entry that never asked for a click, and for no entry at all.
        assert_eq!(reached(entry("link", "fell_back", None)), Reached::Typed);
        assert_eq!(reached(entry("direct", "arrived", None)), Reached::Typed);
        assert_eq!(reached(entry("none", "arrived", None)), Reached::Typed);
        assert_eq!(reached(serde_json::Value::Null), Reached::Typed);
        let mut bare = examples["PageResult"][0].clone();
        bare["behavior"] = serde_json::Value::Null;
        assert_eq!(
            reached_of(&serde_json::from_value::<PageResult>(bare).unwrap()),
            Reached::Typed
        );
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
        // A hop to another site whose own exit cannot be had now (F166):
        // the site is not to blame, and a later attempt may find the exit.
        assert_eq!(
            told(HttpError::HopExitUnavailable {
                url: "http://other.example/x".to_owned(),
                reason: "its plain exit cannot carry traffic now".to_owned(),
            }),
            (
                Outcome::ProviderError,
                "hop_exit_unavailable".to_owned(),
                Courtesy::NotTheSite,
                true
            )
        );
        assert_eq!(
            told(HttpError::Tls("the site's certificate was refused")),
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
        let output =
            PathExecutor::http_failure(&HttpError::Tls("the site's certificate was refused"));
        assert_eq!(
            output.classification.evidence,
            [
                "failure:connection_failed",
                "error:the TLS handshake with the target failed: the site's certificate was refused"
            ]
        );
    }

    #[test]
    fn a_refusal_of_the_forwarder_is_the_guards_outcome_for_the_hosts_the_attempt_went_to() {
        let examples: serde_json::Value = serde_json::from_str(EXAMPLES_JSON).unwrap();
        let mut page: PageResult =
            serde_json::from_value(examples["PageResult"][0].clone()).unwrap();
        page.url = "https://shop.example/start".to_owned();
        page.redirects = vec![kage_browser::contract::RedirectHop {
            url: "https://shop.example/start".to_owned(),
            status: 302,
            location: Some("//Hidden.Example/x".to_owned()),
        }];
        page.final_url = "http://10.0.0.1:8091/x".to_owned();
        let hosts = hosts_of(&page);
        for host in ["shop.example", "hidden.example", "10.0.0.1"] {
            assert!(hosts.iter().any(|seen| seen == host), "{host} in {hosts:?}");
        }

        let denials = Denials::default();
        let persona = Caller::Persona(kage_core::ids::PersonaId::new());
        let plain = Caller::Plain(PlainRoute::Host);
        let mark = denials.mark();
        denials.record(
            plain,
            "other.qa.test",
            "other.qa.test resolves to 10.0.0.9, in Private",
        );
        denials.record(
            persona,
            "Hidden.Example.",
            "hidden.example resolves to 10.0.0.5",
        );
        // Only the caller's own, of a host the attempt went to, after its mark.
        let found = denials.since(persona, mark, &hosts).unwrap();
        assert_eq!(found.host, "hidden.example");
        assert_eq!(denials.since(plain, mark, &hosts), None);
        assert_eq!(denials.since(persona, denials.mark(), &hosts), None);
        for _ in 0..40 {
            denials.record(persona, "filler.example", "refused");
        }
        // Only the latest are kept.
        assert_eq!(denials.since(persona, mark, &hosts), None);

        let output = denied_by_forwarder(&found);
        assert_eq!(output.outcome(), Outcome::SsrfDenied);
        assert!(!output.classification.retryable);
        assert_eq!(output.courtesy, Courtesy::NotTheSite);
        assert!(
            output
                .classification
                .evidence
                .iter()
                .any(|evidence| evidence.starts_with("refused:hidden.example (")),
            "{:?}",
            output.classification.evidence
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

    #[test]
    fn a_page_that_failed_while_its_proxy_was_not_reached_is_the_proxys_failure() {
        // F108 (sched-05): a browser told a plain http:// request that the
        // forwarder closed for want of its proxy reports navigation_failed,
        // as for a site that does not answer.
        let site_silent = || {
            AttemptOutput::failed(
                Outcome::ProviderError,
                "navigation_failed",
                Courtesy::ServerError,
            )
        };
        let blamed = blame_the_proxy(site_silent(), true);
        assert_eq!(blamed.outcome(), Outcome::ProviderError);
        assert_eq!(blamed.reason.as_deref(), Some("proxy_failed"));
        assert_eq!(blamed.courtesy, Courtesy::NotTheSite);
        assert!(blamed.classification.retryable);
        assert!(
            blamed
                .classification
                .evidence
                .contains(&"site_saw_nothing:navigation_failed".to_owned()),
            "{:?}",
            blamed.classification.evidence
        );
        // A timeout with no answer, likewise.
        let timed_out = AttemptOutput::failed(Outcome::Timeout, "timeout", Courtesy::ServerError);
        assert_eq!(
            blame_the_proxy(timed_out, true).courtesy,
            Courtesy::NotTheSite
        );

        // With the proxy relaying, the site's silence is the site's.
        let kept = blame_the_proxy(site_silent(), false);
        assert_eq!(kept.reason.as_deref(), Some("navigation_failed"));
        assert_eq!(kept.courtesy, Courtesy::ServerError);
        // An answer of the site is its own, whatever the proxy did besides.
        let mut answered = site_silent();
        answered.status = Some(503);
        assert_eq!(
            blame_the_proxy(answered, true).courtesy,
            Courtesy::ServerError
        );
        // So is anything that is no server error.
        let blocked =
            AttemptOutput::failed(Outcome::ProviderError, "aborted", Courtesy::NotTheSite);
        assert_eq!(
            blame_the_proxy(blocked, true).reason.as_deref(),
            Some("aborted")
        );
    }
}
