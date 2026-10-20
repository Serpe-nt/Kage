//! One fetch from the request to its result (spec §3.2, §8).
//!
//! 1. The SSRF guard looks at the URL; the domain's policy is resolved.
//!    The request's level (spec §25) decides its path once: the http path
//!    at 0 and 1, the browser at 2 and 3. Nothing moves it later.
//! 2. The scheduler grants a persona and its slot; the request waits for
//!    it, or is answered at once when the domain is paused, egress is
//!    unsafe, the bandwidth budget is used up or no persona can serve.
//! 3. The executor makes the attempt and the classifier labels it.
//! 4. What the attempt means is settled (cooldown, breaker), and the
//!    rules of [`kage_core::schedule::retry`] say what follows: done, or
//!    another persona after a pause.
//! 5. What the request asked to keep of its page is stored
//!    ([`crate::archive`]), then the request, its attempts and its result.
//!    The result's headers show the names of the cookies a site set and
//!    not their values, unless the configuration says otherwise.
//!
//! Three things stand around those steps since M11. Before the scheduler
//! is asked, a fetch that named a cache lifetime is answered from the
//! response cache when the cache has its page (`cache`, spec §15), and
//! the site's `robots.txt` is looked at where its policy says so
//! (`gate`, spec §14): a scout asks for the file, never the persona that
//! reads. After the last attempt, the document the visit captured is read
//! once for what the request asked to have extracted and for what a crawl
//! takes from it (`read`, spec §13, §14).
//!
//! # Level 0
//!
//! A plain client has no persona: the scheduler only holds it to the
//! site's own gates ([`Scheduler::acquire_plain`]), and it leaves through
//! its exit: the host's connection under `egress.plain: host`, or under
//! `pool` the site's one plain exit, the proxy of the site's scout (chosen
//! as a scout is, and the site's scout from then on), never another
//! address. A block is its answer at once; a failure that was not the
//! site's is tried again from the same exit after a pause. Nothing is
//! settled on a persona or on the site's breaker.
//!
//! A synchronous fetch that takes longer than its wall time is not
//! cancelled: [`Pipeline::fetch_sync`] hands back a [`Running`] fetch,
//! which the API carries on as a job (spec §8.4).
//!
//! # Work for a job
//!
//! [`Pipeline::fetch_for_job`] is the same fetch with three differences
//! (spec §16.6). A request that cannot be attempted now (its domain is
//! paused, the bandwidth budget is used up, egress is unsafe, its persona
//! is away for longer than the job cares to hold the page in the
//! scheduler) is not answered with an error: it is [`JobFetch::Deferred`],
//! nothing is concluded, and the job asks again later under the same
//! request. A stop signal takes a request that waits for a persona out of
//! the queue ([`JobFetch::Stopped`]); an attempt in flight is never cut
//! by it. And a request can be taken up again after the process that began
//! it stopped: its earlier attempts stay in the record and count toward
//! the retry budget.
//!
//! # A server that stops
//!
//! [`Pipeline::wind_down`] waits for the attempts that are under way,
//! whoever they are for, and cuts what outlasts the wait: such an attempt
//! is written down with the reason [`INTERRUPTED`], so that the record
//! says the site was asked (follow-up F39).

mod cache;
mod flights;
mod gate;
mod read;
mod visit;
mod wind_down;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use kage_browser::contract::{CaptureOptions, PageResult, Wait};
use kage_core::config::{Config, MAX_ATTEMPT_TIMEOUT};
use kage_core::domain::{
    ArtifactRef, Attempt, CountryCode, Engine, Exit, FetchRecord, Header, Level, Persona,
    PersonaSummary, Render, RenderMode, RequestRecord, RobotsVerdict, SessionId, Visit,
    redact_set_cookie,
};
use kage_core::ids::{ApiKeyId, AttemptId, JobId, PersonaId, RequestId, ResultId};
use kage_core::outcome::{Block, Outcome};
use kage_core::pacing::Lognormal;
use kage_core::policy::{EffectivePolicy, PacingDecision, PacingRequest};
use kage_core::schedule::planner::{
    Completion, Dispatch, PersonaView, Refusal, RetryPause, Visitors, Work,
};
use kage_core::schedule::politeness::Courtesy;
use kage_core::schedule::queue::Priority;
use kage_core::schedule::retry::retry_pause;
use kage_core::schedule::retry::{AttemptVerdict, RequestProgress, Ruling, Step};
use kage_core::schedule::selection::Network;
use kage_core::seed::Seed;
use kage_core::ssrf::{SsrfPolicy, check_url};
use kage_crawl::html::Page;
use kage_extract::Options as ExtractOptions;
use kage_http::RequestKind;
use tracing::Instrument;
use url::Url;

pub use self::cache::CachePlan;
use self::flights::{Boarded, Flight, Flights};
use self::gate::Gate;
pub use self::gate::{NewScout, Peek, ROBOTS_NO_SCOUT, ROBOTS_UNAVAILABLE, RobotsRead, ScoutNeed};
pub use self::read::{CrawlReading, ReadLink};
use self::wind_down::UnderWay;
pub use self::wind_down::{INTERRUPTED, WoundDown};

use crate::SchedulerError;
use crate::archive::{Archive, Archived, Keep};
use crate::backend::Backend;
use crate::executor::{
    Arrival, AttemptExecutor, AttemptJob, AttemptOutput, BehaviorWish, FlowPlan, PlainJob,
    default_capture, default_wait,
};
use crate::forwarder::PlainRoute;
use crate::metrics;
use crate::robots::{Desk, RobotsSettings, RobotsWish};
use crate::scheduler::{Grant, PlainGrant, Scheduler};
use crate::verdicts::Verdicts;

/// One fetch as a caller asks for it.
#[derive(Debug, Clone)]
pub struct FetchOrder {
    /// The page.
    pub url: Url,
    /// `GET`, `HEAD` or `POST` (http path).
    pub method: http::Method,
    /// The caller's headers (http path).
    pub headers: Vec<(String, String)>,
    /// The request body (http path).
    pub body: Option<Bytes>,
    /// What the request is on the http path: a navigation, a `fetch()` or
    /// an `XMLHttpRequest`.
    pub kind: RequestKind,
    /// The camouflage level (spec §25): the path of every attempt, and how
    /// much of a person each is.
    pub level: Level,
    /// Whose work it is.
    pub priority: Priority,
    /// The persona of the caller's session, when there is one.
    pub pin: Option<PersonaId>,
    /// Engines the caller accepts; empty for any the policy allows.
    pub engines: Vec<Engine>,
    /// Countries the caller would rather have.
    pub prefer_countries: Vec<CountryCode>,
    /// Pacing the caller asks for.
    pub pacing: Option<PacingRequest>,
    /// Longest one attempt may take; the path's default when `None`.
    pub timeout: Option<Duration>,
    /// Retries after the first attempt; the policy's when `None`.
    pub max_retries: Option<u32>,
    /// What a browser attempt captures.
    pub capture: CaptureOptions,
    /// What a browser attempt waits for.
    pub wait: Wait,
    /// What the request asks of the behavior on a browser page; `None`
    /// leaves all of it to the domain policy.
    pub behavior: Option<BehaviorWish>,
    /// Which parts of the page are stored with the result.
    pub keep: Keep,
    /// The largest body kept.
    pub max_body_bytes: Option<u64>,
    /// The key that asked; `None` for Kage's own.
    pub api_key: Option<ApiKeyId>,
    /// The job the fetch belongs to.
    pub job: Option<JobId>,
    /// The caller's session.
    pub session: Option<SessionId>,
    /// The personas that read the domain for the job the fetch belongs to,
    /// and how many it may have there: the fetch goes to one of them while
    /// they can go on.
    pub visitors: Option<Visitors>,
    /// How long the fetch waits for a persona before it would rather be
    /// told when to come back; `None` waits as long as it takes.
    pub patience: Option<Duration>,
    /// A flow to run in a browser in place of loading `url`, which is
    /// then the page the flow starts on.
    pub flow: Option<FlowPlan>,
    /// How the persona arrives, when the caller knows what it has been
    /// reading (a crawl's next page): by a link of a page it holds, or by
    /// a typed address. `None` leaves it to the path.
    pub arrival: Option<Arrival>,
    /// Personas that may not take the fetch, whoever else may: the one
    /// that read a crawl's sitemaps is not the one that reads its pages.
    /// A pinned fetch has its persona whatever this says.
    pub exclude: Vec<PersonaId>,
    /// What the request says about the site's `robots.txt` (spec §14).
    pub robots: RobotsWish,
    /// The fetch is the site's scout's: it asks for what no reader asks
    /// for (a `robots.txt`, a sitemap). `pin` names the scout, the
    /// policy's engines and countries do not bind it (a scout may be a
    /// persona the policy keeps from reading the site), and `robots.txt`
    /// is not looked at for it.
    pub scout: bool,
    /// What is extracted from the page's document (spec §14); `None` for
    /// nothing.
    pub extract: Option<ExtractOptions>,
    /// Whether the report carries what a crawl reads from the page (its
    /// links, its text), from the same tree as the extraction.
    pub read_page: bool,
    /// What the fetch may do with the response cache (spec §15); `None`
    /// for one that neither reads nor fills it.
    pub cache: Option<CachePlan>,
    /// The request as it was accepted, for the record.
    pub spec: serde_json::Value,
}

impl FetchOrder {
    /// A plain `GET` of a page with every default: level 3, the policy's
    /// pacing and retries, the document captured.
    pub fn get(url: Url, priority: Priority) -> Self {
        Self {
            spec: serde_json::json!({ "url": url.as_str() }),
            url,
            method: http::Method::GET,
            headers: Vec::new(),
            body: None,
            kind: RequestKind::Document,
            level: Level::Human,
            priority,
            pin: None,
            engines: Vec::new(),
            prefer_countries: Vec::new(),
            pacing: None,
            timeout: None,
            max_retries: None,
            capture: default_capture(),
            wait: default_wait(),
            behavior: None,
            keep: Keep::default(),
            max_body_bytes: None,
            api_key: None,
            job: None,
            session: None,
            visitors: None,
            patience: None,
            flow: None,
            arrival: None,
            exclude: Vec::new(),
            robots: RobotsWish::Policy,
            scout: false,
            extract: None,
            read_page: false,
            cache: None,
        }
    }
}

/// The pacing that applied to a fetch (spec §16.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacingReport {
    /// The profile's name; `custom` for one sent with the request.
    pub applied: String,
    /// Whether the request asked for pacing the policy's lock overrode.
    pub request_ignored: bool,
    /// How long the fetch waited for personas and their slots, in all.
    pub waited: Duration,
}

/// What a fetch came to.
#[derive(Debug, Clone)]
pub struct FetchReport {
    /// The request.
    pub request: RequestId,
    /// The stored result.
    pub result: ResultId,
    /// The final outcome.
    pub outcome: Outcome,
    /// Why the fetch ended without the site's verdict, when it did.
    pub reason: Option<String>,
    /// When it would be worth asking again, when the scheduler knows.
    pub retry_at: Option<DateTime<Utc>>,
    /// Every attempt, in order.
    pub attempts: Vec<Attempt>,
    /// What the last attempt came to; `None` when there was none.
    pub last: Option<AttemptOutput>,
    /// The headers of the last response as the stored result has them:
    /// with the values of the cookies the site set left out, unless the
    /// configuration exposes them.
    pub headers: Vec<Header>,
    /// The bodies and files that were stored with the result.
    pub artifacts: Vec<ArtifactRef>,
    /// The captured XHR and `fetch()` responses that were stored with it.
    pub xhr: Option<serde_json::Value>,
    /// When the result was stored.
    pub finished_at: DateTime<Utc>,
    /// The persona of the last attempt.
    pub persona: Option<PersonaSummary>,
    /// The block report, when the outcome is a block.
    pub block: Option<Block>,
    /// The pacing that applied.
    pub pacing: PacingReport,
    /// What was extracted from the page's document, when the request
    /// asked for it and the fetch got an HTML document.
    pub extract: Option<serde_json::Value>,
    /// What the site's `robots.txt` says of the address; `None` for a
    /// scout's own fetch and for a request that ended before it was
    /// looked at.
    pub robots: Option<RobotsVerdict>,
    /// What a crawl reads from the page, when the order asked for it.
    pub page: Option<Page>,
    /// What a crawl takes from the page, when the order asked for what a
    /// crawl reads and the fetch got its page: stored with the result.
    pub reading: Option<CrawlReading>,
    /// The stored result, when the fetch was answered from the response
    /// cache: nothing was asked of the site, and there are no attempts.
    pub cached: Option<Box<FetchRecord>>,
    /// What the result keeps of the visit besides the page, as it was
    /// stored: the pacing, what the persona did on the page, the
    /// browser's log, what a capture fired.
    pub visit: Visit,
}

/// A synchronous fetch that outlasted its wall time. It keeps running;
/// its request is stored and its result will be. From here on it is a
/// job's fetch: what only says "not now" (a paused domain, a server that
/// is stopping) defers it instead of ending it, so that whoever carries it
/// on as a job can take it up again under its request.
#[derive(Debug)]
pub struct Running {
    /// The request, by which the result will be found.
    pub request: RequestId,
    task: tokio::task::JoinHandle<Result<Ran, SchedulerError>>,
}

impl Running {
    /// Waits for the fetch to end, or to be deferred.
    ///
    /// # Errors
    ///
    /// What the fetch failed with; [`SchedulerError::ShuttingDown`] when
    /// its task was stopped.
    pub async fn wait(self) -> Result<JobFetch, SchedulerError> {
        Ok(
            match self
                .task
                .await
                .unwrap_or(Err(SchedulerError::ShuttingDown))?
            {
                Ran::Finished(report) => JobFetch::Finished(report),
                Ran::Deferred(deferred) => JobFetch::Deferred(deferred),
                Ran::Stopped => JobFetch::Stopped,
            },
        )
    }
}

/// How a synchronous fetch ended within its wall time.
#[derive(Debug)]
pub enum Synchronous {
    /// It finished.
    Finished(Box<FetchReport>),
    /// It did not: it runs on, to be picked up as a job.
    Overflowed(Running),
}

/// Why a fetch made for a job was not attempted now, and when to ask
/// again. Nothing was concluded: the request is still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deferred {
    /// What stands in the way.
    pub reason: Refusal,
    /// When it would be worth asking again, when the scheduler knows.
    pub retry_at: Option<DateTime<Utc>>,
}

/// How a fetch made for a job ended.
#[derive(Debug)]
pub enum JobFetch {
    /// It finished; its result is stored.
    Finished(Box<FetchReport>),
    /// It could not be attempted now. Its request stays open and is taken
    /// up again with [`Pipeline::fetch_for_job`].
    Deferred(Deferred),
    /// The stop signal came while it waited for a persona. Its request
    /// stays open: take it up again, or end it with [`Pipeline::abandon`].
    Stopped,
}

/// One capture point of a flow (an `extract`, `screenshot` or `pdf`
/// step), to be stored as a result of its own with what it kept.
#[derive(Debug, Clone)]
pub struct Capture<'a> {
    /// The page as the step captured it.
    pub page: &'a PageResult,
    /// Which parts of it are kept.
    pub keep: Keep,
    /// The key the flow belongs to.
    pub api_key: Option<ApiKeyId>,
    /// The job the flow belongs to.
    pub job: Option<JobId>,
    /// The caller's session.
    pub session: Option<SessionId>,
    /// The persona that ran the flow.
    pub persona: Option<PersonaSummary>,
    /// What is extracted from the captured document (spec §14).
    pub extract: Option<ExtractOptions>,
    /// What is recorded as the request: which step of which flow.
    pub spec: serde_json::Value,
    /// The level of the flow: 2 or 3.
    pub level: Level,
}

/// A stored capture point.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    /// The request the capture is stored under.
    pub request: RequestId,
    /// What the classifier makes of the captured page.
    pub outcome: Outcome,
    /// The status of the page's main response.
    pub status: Option<u16>,
    /// The bodies and files that were stored.
    pub artifacts: Vec<ArtifactRef>,
    /// What was extracted from the captured document.
    pub extract: Option<serde_json::Value>,
}

/// How long a plain client's request waits when the site has no plain
/// exit that can carry traffic: a job comes back after it, a caller is
/// told when.
const PLAIN_EXIT_WAIT: Duration = Duration::from_mins(5);

/// Tells work for a job to stop waiting: `true` once it should. An attempt
/// that is under way is not cut by it.
pub type Stop = tokio::sync::watch::Receiver<bool>;

/// The span of one request through the pipeline (spec §20): its id, its
/// domain and its level, never its address.
fn fetch_span(id: RequestId, order: &FetchOrder) -> tracing::Span {
    tracing::info_span!(
        "pipeline.fetch",
        request = %id,
        domain = order.url.host_str().unwrap_or_default(),
        level = order.level.label(),
        render = order.level.render().as_str(),
        outcome = tracing::field::Empty,
    )
}

/// Writes down on the current span how a run of the pipeline ended.
fn note_ending(ran: &Result<Ran, SchedulerError>) {
    let ending = match ran {
        Ok(Ran::Finished(report)) => report.outcome.as_str(),
        Ok(Ran::Deferred(_)) => "deferred",
        Ok(Ran::Stopped) => "stopped",
        Err(_) => "error",
    };
    tracing::Span::current().record("outcome", ending);
}

/// Resolves when the signal says stop; never when its sender is gone.
async fn stopped(stop: &mut Stop) {
    if stop.wait_for(|stop| *stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Whether a refusal is one a job waits out instead of failing its item:
/// it says nothing about the request, only about now.
const fn waits(refused: &crate::scheduler::Refused) -> bool {
    match refused.reason {
        Refusal::DomainPaused
        | Refusal::EgressUnsafe
        | Refusal::BandwidthExhausted
        | Refusal::RobotsUnavailable
        | Refusal::PlainExitUnavailable
        | Refusal::ShuttingDown => true,
        // Nobody can serve it at all, unless the scheduler named the
        // moment somebody can.
        Refusal::NoPersona => refused.retry_at.is_some(),
    }
}

/// What the pipeline works with besides the scheduler.
#[derive(Debug, Clone, Copy)]
pub struct PipelineSettings {
    /// Longest one http attempt takes by default.
    pub http_timeout: Duration,
    /// Longest one browser attempt takes by default.
    pub browser_timeout: Duration,
    /// Total time a synchronous fetch may take before it is handed over.
    pub sync_wall_time: Duration,
    /// The pause before a retry on another persona.
    pub retry_pause: Lognormal,
    /// The SSRF guard's settings.
    pub security: SsrfPolicy,
    /// Whether a result shows the values of the cookies a site set.
    pub expose_set_cookie: bool,
    /// Where level 0 leaves from (`egress.plain`).
    pub plain_exit: Exit,
}

impl PipelineSettings {
    /// The settings of the configuration.
    pub fn from_config(config: &Config) -> Self {
        Self {
            http_timeout: config.http.timeout.get(),
            browser_timeout: config.browser.timeout.get(),
            sync_wall_time: config.scheduler.sync_wall_time.get(),
            retry_pause: config.scheduler.retry_pause.distribution(),
            security: config.security.ssrf(),
            expose_set_cookie: config.security.expose_set_cookie,
            plain_exit: config.egress.plain_exit(),
        }
    }
}

/// See the module documentation.
#[derive(Debug, Clone)]
pub struct Pipeline {
    scheduler: Arc<Scheduler>,
    executor: Arc<dyn AttemptExecutor>,
    verdicts: Arc<Verdicts>,
    settings: PipelineSettings,
    archive: Option<Arc<dyn Archive>>,
    robots: Arc<RobotsSettings>,
    desk: Arc<Desk>,
    /// The attempts that are under way, for a server that stops.
    under_way: Arc<UnderWay>,
    /// The fetches under way that put their page into the cache, which an
    /// identical one waits for.
    flights: Arc<Flights>,
}

/// Counts an attempt and writes its line in the log. `page_of_visit` is
/// `None` for a plain client's attempt, which belongs to no visit.
fn note(
    attempt: &Attempt,
    level: Level,
    domain: &str,
    page_of_visit: Option<u32>,
    output: &AttemptOutput,
) {
    let vendor = output
        .classification
        .vendors
        .first()
        .map_or("", |vendor| vendor.as_str());
    metrics::attempt(
        domain,
        level,
        attempt.outcome,
        vendor,
        (attempt.finished_at - attempt.started_at)
            .to_std()
            .unwrap_or(Duration::ZERO),
    );
    tracing::info!(
        request = %attempt.request_id,
        persona = attempt.persona_id.as_ref().map(tracing::field::display),
        exit = attempt.exit.map(Exit::as_str),
        domain,
        level = level.label(),
        render = attempt.render.as_str(),
        outcome = attempt.outcome.as_str(),
        vendor,
        n = attempt.n,
        status = attempt.status,
        total_ms = attempt.timing.total_ms,
        page_of_visit,
        seed = attempt.seed.as_ref().map(tracing::field::display),
        "attempt"
    );
}

/// The seed of one attempt: the persona, the request and the attempt's
/// number, so that the attempt can be replayed from what is stored.
pub fn attempt_seed(persona: &Seed, request: RequestId, n: u32) -> Seed {
    Seed::derive(
        "attempt",
        &[
            persona.as_bytes(),
            request.as_uuid().as_bytes(),
            &n.to_le_bytes(),
        ],
    )
}

/// How a request that is still under way stands.
struct Course {
    order: FetchOrder,
    request: RequestRecord,
    domain: String,
    policy: EffectivePolicy,
    pacing: PacingDecision,
    progress: RequestProgress,
    attempts: Vec<Attempt>,
    last: Option<AttemptOutput>,
    last_persona: Option<Persona>,
    waited: Duration,
    /// Networks of the personas that were blocked.
    avoid: Vec<Network>,
    /// The pause before the next attempt, when it is another persona's.
    retry: Option<RetryPause>,
    give_up_at: Option<DateTime<Utc>>,
    /// Set once a synchronous fetch outlasted its wall time: it is carried
    /// on as a job and waits where a job waits.
    overflowed: Option<Arc<AtomicBool>>,
    /// The site the page belongs to: its registrable domain.
    site: String,
    /// The site's scout, which reads none of its pages.
    site_scout: Option<PersonaId>,
    /// The request for the site's `robots.txt` that follows this fetch,
    /// where the file is reported and nothing of it is known.
    robots_due: Option<gate::Call>,
    /// What the site's `robots.txt` says of the address, once it was
    /// looked at.
    robots: Option<RobotsVerdict>,
    /// The gap an enforced `Crawl-delay` asks for.
    gap_floor: Option<Duration>,
    /// The request's hold on its cache key while it is fetched: an
    /// identical one waits until the course ends.
    flight: Option<Flight>,
}

impl Course {
    /// What the request has come to when nothing more is attempted: its
    /// last attempt's outcome, whether this process made it or an earlier
    /// one did.
    /// Whether the fetch is a job's by now: made for one, or a
    /// synchronous fetch that outlasted its wall time.
    fn waits_like_a_job(&self, has_stop: bool) -> bool {
        has_stop
            || self
                .overflowed
                .as_ref()
                .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    /// Whether the last attempt was cut because the server stopped.
    fn was_cut(&self) -> bool {
        self.last
            .as_ref()
            .is_some_and(|last| last.reason.as_deref() == Some(INTERRUPTED))
    }

    fn standing(&self) -> (Outcome, Option<String>) {
        match (&self.last, self.attempts.last()) {
            (Some(last), _) => (last.outcome(), last.reason.clone()),
            (None, Some(attempt)) => (attempt.outcome, attempt.reason.clone()),
            (None, None) => (Outcome::ProviderError, None),
        }
    }
}

/// How a run of the pipeline ended.
#[derive(Debug)]
enum Ran {
    Finished(Box<FetchReport>),
    Deferred(Deferred),
    Stopped,
}

impl Pipeline {
    /// A pipeline over a scheduler and an executor.
    pub fn new(
        scheduler: Arc<Scheduler>,
        executor: Arc<dyn AttemptExecutor>,
        verdicts: Arc<Verdicts>,
        settings: PipelineSettings,
    ) -> Self {
        Self {
            scheduler,
            executor,
            verdicts,
            settings,
            archive: None,
            robots: Arc::new(RobotsSettings::default()),
            desk: Arc::new(Desk::default()),
            under_way: Arc::new(UnderWay::default()),
            flights: Arc::new(Flights::default()),
        }
    }

    /// The same pipeline, keeping `robots.txt` as `settings` say (the
    /// configuration's `robots` section) in place of the defaults.
    #[must_use]
    pub fn with_robots(mut self, settings: RobotsSettings) -> Self {
        self.robots = Arc::new(settings);
        self
    }

    /// The same pipeline, keeping what a request asks to keep of its page
    /// in `archive`. Without one nothing is kept and a stored result has
    /// no bodies.
    #[must_use]
    pub fn with_archive(mut self, archive: Arc<dyn Archive>) -> Self {
        self.archive = Some(archive);
        self
    }

    /// The settings the pipeline works with.
    pub const fn settings(&self) -> &PipelineSettings {
        &self.settings
    }

    /// Whether a site's reader asks for its files itself
    /// ([`RobotsSettings::own_scout`]): then nobody is kept from reading a
    /// site for being its scout.
    pub fn own_scout(&self) -> bool {
        self.robots.own_scout
    }

    fn backend(&self) -> &Arc<dyn Backend> {
        self.scheduler.backend()
    }

    /// The personas as the scheduler sees them now: for a caller that has
    /// to know whether there is somebody else to send (a crawl keeps the
    /// persona that reads a site's sitemaps apart from its visitor).
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn personas(&self) -> Result<Vec<PersonaView>, SchedulerError> {
        self.backend().personas().await
    }

    fn now(&self) -> DateTime<Utc> {
        self.scheduler.clock().now()
    }

    /// Runs a fetch to its end, however long that takes.
    ///
    /// # Errors
    ///
    /// Fails when the request cannot be accepted (a URL without a host,
    /// pacing that does not exist) or the backend fails. A fetch that
    /// could not get its page is not an error: its report says why.
    pub async fn fetch(&self, order: FetchOrder) -> Result<FetchReport, SchedulerError> {
        self.fetch_as(RequestId::new(), order, None).await
    }

    /// Runs a synchronous fetch for at most its wall time. One that takes
    /// longer runs on and is handed back to be picked up as a job (spec
    /// §8.4).
    ///
    /// # Errors
    ///
    /// As [`Self::fetch`].
    pub async fn fetch_sync(&self, order: FetchOrder) -> Result<Synchronous, SchedulerError> {
        self.fetch_sync_as(RequestId::new(), order).await
    }

    /// [`Self::fetch_sync`] under an id the caller minted: the API answers
    /// with it before the fetch has one of its own (`X-Request-Id`).
    ///
    /// # Errors
    ///
    /// As [`Self::fetch`].
    pub async fn fetch_sync_as(
        &self,
        request: RequestId,
        order: FetchOrder,
    ) -> Result<Synchronous, SchedulerError> {
        let wall = self.settings.sync_wall_time;
        let pipeline = self.clone();
        let overflowed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&overflowed);
        let span = fetch_span(request, &order);
        let mut task = tokio::spawn(
            async move {
                let mut course = pipeline.begin(request, order, None, None).await?;
                course.overflowed = Some(flag);
                let ran = Box::pin(pipeline.run(course, None)).await;
                note_ending(&ran);
                ran
            }
            .instrument(span),
        );
        let Ok(finished) = tokio::time::timeout(wall, &mut task).await else {
            // From here on the fetch is a job's.
            overflowed.store(true, Ordering::Relaxed);
            return Ok(Synchronous::Overflowed(Running { request, task }));
        };
        match finished.unwrap_or(Err(SchedulerError::ShuttingDown))? {
            Ran::Finished(report) => Ok(Synchronous::Finished(report)),
            // Nothing is deferred or stopped inside the wall time.
            Ran::Deferred(_) | Ran::Stopped => Err(SchedulerError::ShuttingDown),
        }
    }

    /// Runs a fetch that is refused instead of waiting for a persona past
    /// `patience`: for a caller that would rather hear "not now".
    ///
    /// # Errors
    ///
    /// As [`Self::fetch`].
    pub async fn fetch_within(
        &self,
        order: FetchOrder,
        patience: Duration,
    ) -> Result<FetchReport, SchedulerError> {
        let give_up_at = chrono::Duration::from_std(patience)
            .ok()
            .and_then(|patience| self.now().checked_add_signed(patience));
        self.fetch_as(RequestId::new(), order, give_up_at).await
    }

    /// Resolves what a request is under the policies in force, and stores
    /// it.
    async fn begin(
        &self,
        id: RequestId,
        order: FetchOrder,
        give_up_at: Option<DateTime<Utc>>,
        earlier: Option<Vec<Attempt>>,
    ) -> Result<Course, SchedulerError> {
        let domain = order
            .url
            .host_str()
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| SchedulerError::NoHost(order.url.to_string()))?;
        let policies = self.verdicts.policies().current();
        let policy = policies.domains.resolve(&domain);
        let pacing = policy.pacing_for(order.pacing.as_ref(), &policies.pacing)?;
        // The site's scout reads none of its pages, unless the reader is
        // its own scout. A session's persona is the session's, whatever
        // else it is.
        let site = kage_crawl::site::of_host(&domain);
        let site_scout = if order.scout || order.pin.is_some() || self.robots.own_scout {
            None
        } else {
            self.site_scout(&site).await?
        };
        let request = RequestRecord {
            id,
            api_key_id: order.api_key,
            job_id: order.job,
            session_id: order.session.clone(),
            url: order.url.to_string(),
            method: order.method.to_string(),
            domain: domain.clone(),
            render: RenderMode::from(order.level.render()),
            level: Some(order.level),
            spec: order.spec.clone(),
            created_at: self.now(),
            finished_at: None,
            outcome: None,
        };
        // A request that is taken up again was stored when it was accepted.
        if earlier.is_none() {
            self.backend().save_request(&request).await?;
        }
        let earlier = earlier.unwrap_or_default();

        let retries = order.max_retries.unwrap_or(policy.retries);
        let progress = RequestProgress::new(order.level, retries).resumed(
            earlier
                .iter()
                .map(|attempt| (attempt.persona_id, attempt.outcome)),
        );
        // The networks of the personas that were blocked before the
        // request changed hands are avoided as if it had not.
        let avoid = if progress.blocked().is_empty() {
            Vec::new()
        } else {
            self.backend()
                .personas()
                .await?
                .into_iter()
                .filter(|view| progress.blocked().contains(&view.id))
                .map(|view| view.network)
                .collect()
        };
        Ok(Course {
            progress,
            order,
            request,
            domain,
            policy,
            pacing,
            attempts: earlier,
            last: None,
            last_persona: None,
            waited: Duration::ZERO,
            avoid,
            retry: None,
            give_up_at,
            overflowed: None,
            site,
            site_scout,
            robots_due: None,
            robots: None,
            gap_floor: None,
            flight: None,
        })
    }

    async fn fetch_as(
        &self,
        id: RequestId,
        order: FetchOrder,
        give_up_at: Option<DateTime<Utc>>,
    ) -> Result<FetchReport, SchedulerError> {
        let span = fetch_span(id, &order);
        let ran = async {
            let course = self.begin(id, order, give_up_at, None).await?;
            let ran = Box::pin(self.run(course, None)).await;
            note_ending(&ran);
            ran
        }
        .instrument(span)
        .await;
        match ran? {
            Ran::Finished(report) => Ok(*report),
            // Without a stop signal nothing is deferred and nothing stops.
            Ran::Deferred(_) | Ran::Stopped => Err(SchedulerError::ShuttingDown),
        }
    }

    /// Runs a fetch for a job (see the module documentation). `earlier` is
    /// `None` for a request that is new, and the attempts made so far for
    /// one that is taken up again under its id: by the process that began
    /// it or by another after it stopped.
    ///
    /// # Errors
    ///
    /// As [`Self::fetch`].
    pub async fn fetch_for_job(
        &self,
        id: RequestId,
        order: FetchOrder,
        earlier: Option<Vec<Attempt>>,
        stop: Stop,
    ) -> Result<JobFetch, SchedulerError> {
        let span = fetch_span(id, &order);
        let ran = async {
            let course = self.begin(id, order, None, earlier).await?;
            let ran = Box::pin(self.run(course, Some(stop))).await;
            note_ending(&ran);
            ran
        }
        .instrument(span)
        .await;
        Ok(match ran? {
            Ran::Finished(report) => JobFetch::Finished(report),
            Ran::Deferred(deferred) => JobFetch::Deferred(deferred),
            Ran::Stopped => JobFetch::Stopped,
        })
    }

    /// Ends a request that will not be attempted any more (its job was
    /// cancelled, or waited as long as it may) with `provider_error` and
    /// the reason, so that whoever asks for its result is told why there is
    /// none. `earlier` are the attempts it had.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn abandon(
        &self,
        id: RequestId,
        order: FetchOrder,
        earlier: Vec<Attempt>,
        reason: &str,
    ) -> Result<FetchReport, SchedulerError> {
        let course = self.begin(id, order, None, Some(earlier)).await?;
        self.conclude(
            course,
            Outcome::ProviderError,
            Some(reason.to_owned()),
            None,
        )
        .await
    }

    /// Stores one capture point of a flow as a request and a result of its
    /// own, so that it is read, kept and deleted like any other result.
    /// The flow's attempts stay with the flow's own request.
    ///
    /// # Errors
    ///
    /// Fails when the backend fails.
    pub async fn keep_capture(&self, capture: Capture<'_>) -> Result<Kept, SchedulerError> {
        let page = capture.page;
        let id = RequestId::new();
        let now = self.now();
        let domain = Url::parse(&page.final_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .unwrap_or_else(|| "(none)".to_owned());
        let request = RequestRecord {
            id,
            api_key_id: capture.api_key,
            job_id: capture.job,
            session_id: capture.session,
            url: page.final_url.clone(),
            method: http::Method::GET.to_string(),
            domain,
            render: RenderMode::Browser,
            level: Some(capture.level),
            spec: capture.spec,
            created_at: now,
            finished_at: None,
            outcome: None,
        };
        self.backend().save_request(&request).await?;

        let classification = self.verdicts.judge_page(page);
        let outcome = classification.outcome;
        let block = classification.block.clone().filter(|_| outcome.is_block());
        let output = AttemptOutput::of_page(Box::new(page.clone()), classification);
        let headers = if self.settings.expose_set_cookie {
            output.headers.clone()
        } else {
            redact_set_cookie(&output.headers)
        };
        let archived = match &self.archive {
            Some(archive) => archive.keep(id, capture.keep, &output, now).await,
            None => Archived::default(),
        };
        let extract = match (&page.dom_html, capture.extract, Url::parse(&page.final_url)) {
            (Some(document), Some(options), Ok(loaded_from))
                if outcome.is_success() && read::is_html(&output.headers) =>
            {
                read::read_document(document.clone(), loaded_from, false, Some(options))
                    .await
                    .extract
            }
            _ => None,
        };
        let record = FetchRecord {
            id: ResultId::new(),
            request_id: id,
            outcome,
            status: output.status,
            headers,
            final_url: output.final_url.clone(),
            redirects: output.redirects.clone(),
            extract: extract.clone(),
            xhr: archived.xhr,
            artifacts: archived.artifacts.clone(),
            persona: capture.persona,
            block,
            robots: None,
            cached: false,
            cached_at: None,
            reason: output.reason.clone(),
            // A capture point of a flow is no visit of its own: the flow's
            // last result says what applied.
            visit: Visit::default(),
            reading: None,
            created_at: now,
        };
        self.backend().finish(&record).await?;
        Ok(Kept {
            request: id,
            outcome,
            status: record.status,
            artifacts: archived.artifacts,
            extract,
        })
    }

    /// The attempts of a request until it is done, deferred or stopped.
    async fn run(&self, course: Course, mut stop: Option<Stop>) -> Result<Ran, SchedulerError> {
        let id = course.request.id;
        let finished = |report| Ok(Ran::Finished(Box::new(report)));
        // The guard looks at the URL before anything is sent anywhere.
        if let Err(denial) = check_url(&course.order.url, self.settings.security) {
            tracing::info!(request = %id, %denial, "the URL is refused");
            return finished(
                self.conclude(
                    course,
                    Outcome::SsrfDenied,
                    Some("ssrf_denied".to_owned()),
                    None,
                )
                .await?,
            );
        }
        // A request that was taken up again with nothing left of its
        // budget ends with what its last attempt reached.
        if !course.progress.attempts_left() {
            let (outcome, reason) = course.standing();
            return finished(self.conclude(course, outcome, reason, None).await?);
        }
        let mut course = match self.before(course, stop.as_ref()).await? {
            Ok(course) => course,
            Err(ran) => return Ok(ran),
        };
        if course.order.level == Level::Plain {
            return Box::pin(self.run_plain(course, stop)).await;
        }

        loop {
            // Waiting for a persona and its turn (spec §20).
            let acquiring =
                self.scheduler
                    .acquire(self.work(&course))
                    .instrument(tracing::info_span!(
                        "scheduler.wait",
                        kind = ?course.order.priority,
                        n = course.progress.attempts() + 1,
                    ));
            let granted = match &mut stop {
                Some(stop) => tokio::select! {
                    granted = acquiring => granted,
                    () = stopped(stop) => return Ok(Ran::Stopped),
                },
                None => acquiring.await,
            };
            let grant = match granted {
                Ok(grant) => grant,
                Err(refused) => return self.refused(course, &refused, stop.is_some()).await,
            };
            let ruling = self.attempt(&mut course, grant).await?;
            // The server stopped waiting for the page: this process asks
            // for nothing more. A job's fetch is given back with the cut
            // attempt in its record; any other ends with it.
            if course.was_cut() {
                return self.after_cut(course, stop.is_some()).await;
            }
            // A session's persona that was turned away cannot be replaced:
            // the request ends with what it got.
            let pinned_and_blocked = course.order.pin.is_some() && ruling.penalty.is_some();
            match ruling.step {
                Step::Retry if !pinned_and_blocked => {
                    course.retry = Some(self.retry_pause(&course));
                }
                Step::Finish | Step::Retry => {
                    let (outcome, reason) = course.standing();
                    return finished(self.conclude(course, outcome, reason, None).await?);
                }
            }
        }
    }

    /// How a run ends whose next attempt the scheduler refused: a job waits
    /// out what only says "not now"; otherwise what was reached before
    /// stands, and a request that never got an attempt is answered with
    /// the reason.
    async fn refused(
        &self,
        course: Course,
        refused: &crate::scheduler::Refused,
        for_job: bool,
    ) -> Result<Ran, SchedulerError> {
        if course.waits_like_a_job(for_job) && waits(refused) {
            return Ok(Ran::Deferred(Deferred {
                reason: refused.reason,
                retry_at: refused.retry_at,
            }));
        }
        let (outcome, reason) = if course.attempts.is_empty() {
            (
                Outcome::ProviderError,
                Some(refused.reason.as_str().to_owned()),
            )
        } else {
            course.standing()
        };
        let report = self
            .conclude(course, outcome, reason, refused.retry_at)
            .await?;
        Ok(Ran::Finished(Box::new(report)))
    }

    /// The attempts of a plain client (level 0) until it is done, deferred
    /// or stopped: each through the site's gates and from its exit.
    async fn run_plain(
        &self,
        mut course: Course,
        mut stop: Option<Stop>,
    ) -> Result<Ran, SchedulerError> {
        loop {
            let Some(route) = self.plain_route(&course).await? else {
                let refused = crate::scheduler::Refused {
                    reason: Refusal::PlainExitUnavailable,
                    retry_at: self.later_by(PLAIN_EXIT_WAIT),
                };
                return self.refused(course, &refused, stop.is_some()).await;
            };
            let mut work = self.work(&course);
            // A retry waits its pause, drawn as for a persona at the
            // persona's middle pace.
            if let Some(retry) = course.retry {
                let pause = retry_pause(&retry.pause, 1.0, &retry.seed);
                work.not_before = chrono::Duration::from_std(pause)
                    .ok()
                    .and_then(|pause| self.now().checked_add_signed(pause));
            }
            let acquiring = self
                .scheduler
                .acquire_plain(work)
                .instrument(tracing::info_span!(
                    "scheduler.wait",
                    kind = ?course.order.priority,
                    n = course.progress.attempts() + 1,
                ));
            let granted = match &mut stop {
                Some(stop) => tokio::select! {
                    granted = acquiring => granted,
                    () = stopped(stop) => return Ok(Ran::Stopped),
                },
                None => acquiring.await,
            };
            let grant = match granted {
                Ok(grant) => grant,
                Err(refused) => return self.refused(course, &refused, stop.is_some()).await,
            };
            let ruling = self.attempt_plain(&mut course, grant, route).await?;
            if course.was_cut() {
                return self.after_cut(course, stop.is_some()).await;
            }
            match ruling.step {
                Step::Retry => course.retry = Some(self.retry_pause(&course)),
                Step::Finish => {
                    let (outcome, reason) = course.standing();
                    let report = self.conclude(course, outcome, reason, None).await?;
                    return Ok(Ran::Finished(Box::new(report)));
                }
            }
        }
    }

    /// The moment `wait` from now.
    fn later_by(&self, wait: Duration) -> Option<DateTime<Utc>> {
        chrono::Duration::from_std(wait)
            .ok()
            .and_then(|wait| self.now().checked_add_signed(wait))
    }

    /// Where a plain client's attempt leaves from: the host's connection,
    /// or the proxy of the site's scout (chosen now when the site has none,
    /// and its scout from then on). `None` when the pool has no plain exit
    /// for the site that can carry traffic: nobody may scout it, or its
    /// scout's proxy is down. Never another address.
    async fn plain_route(&self, course: &Course) -> Result<Option<PlainRoute>, SchedulerError> {
        if self.settings.plain_exit == Exit::Host {
            return Ok(Some(PlainRoute::Host));
        }
        let scout = match self.site_scout(&course.site).await? {
            Some(scout) => Some(scout),
            None => self.scout_for(&course.order.url, &[], 1).await?,
        };
        let Some(scout) = scout else {
            return Ok(None);
        };
        Ok(self
            .backend()
            .plain_exit(scout)
            .await?
            .map(PlainRoute::Pool))
    }

    /// How a run ends whose last attempt was cut: a fetch for a job
    /// (`for_job`) is stopped and one that outlasted its wall time is given
    /// back, both with the request open; any other ends with what was cut.
    async fn after_cut(&self, course: Course, for_job: bool) -> Result<Ran, SchedulerError> {
        if for_job {
            return Ok(Ran::Stopped);
        }
        if course.waits_like_a_job(false) {
            return Ok(Ran::Deferred(Deferred {
                reason: Refusal::ShuttingDown,
                retry_at: None,
            }));
        }
        let (outcome, reason) = course.standing();
        let report = self.conclude(course, outcome, reason, None).await?;
        Ok(Ran::Finished(Box::new(report)))
    }

    /// What stands before the attempts of a request: the response cache,
    /// which may hold its page, and the site's `robots.txt`, where its
    /// policy asks. `Err` with how the run ended when it ends here.
    async fn before(
        &self,
        mut course: Course,
        stop: Option<&Stop>,
    ) -> Result<Result<Course, Ran>, SchedulerError> {
        let finished = |report| Ok(Err(Ran::Finished(Box::new(report))));
        // A page the cache holds is no visit: nothing is asked of anybody;
        // nor is a page an identical fetch under way is about to keep.
        if course.attempts.is_empty() {
            match self.board(&mut course, stop).await? {
                Boarded::Served(report) => return Ok(Err(Ran::Finished(report))),
                Boarded::Stopped => return Ok(Err(Ran::Stopped)),
                Boarded::Go => {}
            }
        }
        let (outcome, reason, retry_at) = match self.robots_gate(&mut course, stop).await? {
            Gate::Open => return Ok(Ok(course)),
            Gate::Stopped => return Ok(Err(Ran::Stopped)),
            Gate::Denied(reason) => {
                metrics::robots_denied(reason.unwrap_or("rule"));
                (Outcome::RobotsDenied, reason.map(str::to_owned), None)
            }
            Gate::Unavailable { reason, retry_at } => {
                // A job waits until the file can be had; a caller that
                // waits for its page is told that it is not fetched.
                if course.waits_like_a_job(stop.is_some()) {
                    return Ok(Err(Ran::Deferred(Deferred {
                        reason: Refusal::RobotsUnavailable,
                        retry_at,
                    })));
                }
                metrics::robots_denied(reason);
                (Outcome::RobotsDenied, Some(reason.to_owned()), retry_at)
            }
            // The scout was told what the page would have been told.
            Gate::Refused(refused) => {
                if course.waits_like_a_job(stop.is_some()) && waits(&refused) {
                    return Ok(Err(Ran::Deferred(Deferred {
                        reason: refused.reason,
                        retry_at: refused.retry_at,
                    })));
                }
                (
                    Outcome::ProviderError,
                    Some(refused.reason.as_str().to_owned()),
                    refused.retry_at,
                )
            }
        };
        finished(self.conclude(course, outcome, reason, retry_at).await?)
    }

    /// The pause before the next attempt of a request, when another
    /// persona makes it.
    fn retry_pause(&self, course: &Course) -> RetryPause {
        RetryPause {
            pause: self.settings.retry_pause,
            seed: Seed::derive(
                "retry-pause",
                &[
                    course.request.id.as_uuid().as_bytes(),
                    &course.progress.attempts().to_le_bytes(),
                ],
            ),
        }
    }

    /// The next attempt of a request, as work for the scheduler.
    fn work(&self, course: &Course) -> Work {
        let order = &course.order;
        // A scout may be a persona the policy keeps from reading the site.
        let allowed: &[Engine] = if order.scout {
            Engine::ALL
        } else {
            &course.policy.engines
        };
        let engines: Vec<Engine> = allowed
            .iter()
            .copied()
            .filter(|engine| order.engines.is_empty() || order.engines.contains(engine))
            .collect();
        let rules = self.verdicts.breaker_rules(&course.domain);
        Work {
            domain: course.domain.clone(),
            priority: order.priority,
            render: course.progress.render(),
            // A session's persona.
            pin: order.pin,
            exclude: if order.pin.is_some() {
                Vec::new()
            } else {
                // Those that tried already, those the caller keeps out,
                // and the site's scout, which reads none of its pages: the
                // one known when the request began, or one chosen since.
                let scout = if order.scout || self.robots.own_scout {
                    None
                } else {
                    self.desk
                        .scout(&course.site)
                        .and_then(|scouted| scouted.0)
                        .or(course.site_scout)
                };
                let mut exclude = course.progress.used().to_vec();
                for persona in order.exclude.iter().chain(&scout) {
                    if !exclude.contains(persona) {
                        exclude.push(*persona);
                    }
                }
                exclude
            },
            avoid: course.avoid.clone(),
            engines,
            countries: if order.scout {
                None
            } else {
                course.policy.countries.clone()
            },
            prefer_countries: order.prefer_countries.clone(),
            // A session's persona is not a matter of visitors.
            visitors: if order.pin.is_some() {
                None
            } else {
                order.visitors.clone()
            },
            pacing: course.pacing.profile,
            active_hours: course.policy.active_hours,
            breaker_threshold: rules.threshold,
            breaker_pause: rules.pause,
            retry_pause: course.retry,
            not_before: None,
            gap_floor: course.gap_floor,
            give_up_at: course.give_up_at.or_else(|| {
                let patience = chrono::Duration::from_std(order.patience?).ok()?;
                self.now().checked_add_signed(patience)
            }),
            plain: order.level == Level::Plain,
        }
    }

    /// Makes the attempt a grant allows, settles what it means, stores it,
    /// and says what follows.
    async fn attempt(&self, course: &mut Course, grant: Grant) -> Result<Ruling, SchedulerError> {
        let dispatch = grant.dispatch().clone();
        course.waited += dispatch.waited.to_std().unwrap_or(Duration::ZERO);
        let n = course.progress.attempts() + 1;
        let Some(persona) = self.backend().persona(dispatch.persona).await? else {
            // Retired between the plan and now: another persona's turn.
            grant.complete(Completion {
                courtesy: Courtesy::NotTheSite,
                blocked: false,
            });
            let verdict = AttemptVerdict::plain(Outcome::ProviderError);
            return Ok(course.progress.record(Some(dispatch.persona), verdict));
        };
        let seed = attempt_seed(&persona.seed, course.request.id, n);
        let job = self.job(course, &dispatch, &persona, n, seed);

        // Under way from here until it is written down: a server that
        // stops waits for it, and cuts it only when the wait is over.
        let _under_way = self.under_way.begin();
        // The attempt (spec §20): the persona by its id, which is no
        // secret; never its seed, its address or its fingerprint.
        let span = tracing::info_span!(
            "attempt",
            n,
            level = course.order.level.label(),
            render = course.progress.render().as_str(),
            persona = %persona.id,
            outcome = tracing::field::Empty,
            vendor = tracing::field::Empty,
        );
        let started_at = self.now();
        let output = self
            .under_way
            .until_cut(self.executor.run(&job).instrument(span.clone()))
            .await;
        let finished_at = self.now();
        let outcome = output.outcome();
        span.record("outcome", outcome.as_str());
        if let Some(vendor) = output.classification.vendors.first() {
            span.record("vendor", vendor.as_str());
        }
        metrics::breaker_threshold(&course.domain, dispatch.breaker.threshold);
        let verdict = AttemptVerdict::of(&output.classification);
        let ruling = course.progress.record(Some(persona.id), verdict);

        // What the persona answers for.
        self.verdicts
            .settle(
                (persona.id, &persona.seed),
                &course.domain,
                outcome,
                ruling.penalty.as_ref(),
                dispatch.trial,
                &dispatch.breaker,
            )
            .await?;
        if ruling.penalty.is_some() {
            let network = self
                .backend()
                .personas()
                .await?
                .into_iter()
                .find(|view| view.id == persona.id)
                .map(|view| view.network);
            course.avoid.extend(network);
        }
        grant.complete(Completion {
            courtesy: output.courtesy,
            // Its visit is over when it was turned away for good.
            blocked: ruling.penalty.is_some(),
        });

        let attempt = Attempt {
            id: AttemptId::new(),
            request_id: course.request.id,
            n,
            persona_id: Some(persona.id),
            engine: Some(persona.engine),
            exit: None,
            render: dispatch.render,
            started_at,
            finished_at,
            status: output.status,
            final_url: output.final_url.clone(),
            outcome,
            block: output.classification.block.clone(),
            timing: output.timing,
            seed: Some(seed),
            reason: output.reason.clone(),
            patience_ms: output
                .behavior
                .as_ref()
                .and_then(|behavior| behavior.patience)
                .and_then(|patience| u64::try_from(patience.as_millis()).ok()),
        };
        self.backend().save_attempt(&attempt).await?;
        note(
            &attempt,
            course.order.level,
            &course.domain,
            Some(dispatch.page_of_visit),
            &output,
        );
        course.attempts.push(attempt);
        course.last = Some(output);
        course.last_persona = Some(persona);
        Ok(ruling)
    }

    /// Makes a plain client's attempt (level 0), stores it, and says what
    /// follows. Nobody answers for it: no persona cools and the site's
    /// breaker counts nothing; only what it says about the site's health
    /// (server errors) goes back to the scheduler.
    async fn attempt_plain(
        &self,
        course: &mut Course,
        grant: PlainGrant,
        route: PlainRoute,
    ) -> Result<Ruling, SchedulerError> {
        course.waited += grant.dispatch().waited.to_std().unwrap_or(Duration::ZERO);
        let n = course.progress.attempts() + 1;
        let job = PlainJob {
            request: course.request.id,
            n,
            route,
            url: course.order.url.clone(),
            method: course.order.method.clone(),
            headers: course.order.headers.clone(),
            body: course.order.body.clone(),
            domain: course.domain.clone(),
            timeout: course
                .order
                .timeout
                .unwrap_or(self.settings.http_timeout)
                .min(MAX_ATTEMPT_TIMEOUT),
            max_body_bytes: course.order.max_body_bytes,
        };
        let _under_way = self.under_way.begin();
        let span = tracing::info_span!(
            "attempt",
            n,
            level = Level::Plain.label(),
            render = Render::Http.as_str(),
            exit = route.exit().as_str(),
            outcome = tracing::field::Empty,
            vendor = tracing::field::Empty,
        );
        let started_at = self.now();
        let output = self
            .under_way
            .until_cut(self.executor.plain(&job).instrument(span.clone()))
            .await;
        let finished_at = self.now();
        let outcome = output.outcome();
        span.record("outcome", outcome.as_str());
        if let Some(vendor) = output.classification.vendors.first() {
            span.record("vendor", vendor.as_str());
        }
        let ruling = course
            .progress
            .record(None, AttemptVerdict::of(&output.classification));
        grant.complete(output.courtesy);
        let attempt = Attempt {
            id: AttemptId::new(),
            request_id: course.request.id,
            n,
            persona_id: None,
            engine: None,
            exit: Some(route.exit()),
            render: Render::Http,
            started_at,
            finished_at,
            status: output.status,
            final_url: output.final_url.clone(),
            outcome,
            block: output.classification.block.clone(),
            timing: output.timing,
            seed: None,
            reason: output.reason.clone(),
            patience_ms: None,
        };
        self.backend().save_attempt(&attempt).await?;
        note(&attempt, Level::Plain, &course.domain, None, &output);
        course.attempts.push(attempt);
        course.last = Some(output);
        course.last_persona = None;
        Ok(ruling)
    }

    /// The attempt a grant allows, as the executor is asked for it.
    fn job(
        &self,
        course: &Course,
        dispatch: &Dispatch,
        persona: &Persona,
        n: u32,
        seed: Seed,
    ) -> AttemptJob {
        let timeout = course
            .order
            .timeout
            .unwrap_or(match dispatch.render {
                Render::Http => self.settings.http_timeout,
                Render::Browser => self.settings.browser_timeout,
            })
            .min(MAX_ATTEMPT_TIMEOUT);
        AttemptJob {
            request: course.request.id,
            n,
            persona: persona.clone(),
            level: course.order.level,
            render: dispatch.render,
            url: course.order.url.clone(),
            method: course.order.method.clone(),
            kind: course.order.kind,
            behavior: course.order.behavior,
            headers: course.order.headers.clone(),
            body: course.order.body.clone(),
            domain: course.domain.clone(),
            seed,
            timeout,
            policy: course.policy.clone(),
            starts_visit: dispatch.starts_visit,
            keeps_visiting: !dispatch.ends_visit,
            capture: course.order.capture.clone(),
            wait: course.order.wait.clone(),
            max_body_bytes: course.order.max_body_bytes,
            flow: course.order.flow.clone(),
            arrival: course.order.arrival.clone(),
        }
    }

    /// The headers of the last response as a result shows them: the
    /// values of the cookies a site set left out unless the configuration
    /// exposes them.
    fn shown_headers(&self, last: Option<&AttemptOutput>) -> Vec<Header> {
        last.map_or_else(Vec::new, |last| {
            if self.settings.expose_set_cookie {
                last.headers.clone()
            } else {
                redact_set_cookie(&last.headers)
            }
        })
    }

    /// Keeps what a request asked to keep of its last attempt.
    async fn keep(&self, request: RequestId, keep: Keep, last: Option<&AttemptOutput>) -> Archived {
        match (&self.archive, last) {
            (Some(archive), Some(last)) => archive.keep(request, keep, last, self.now()).await,
            _ => Archived::default(),
        }
    }

    /// Stores the result and reports.
    async fn conclude(
        &self,
        course: Course,
        outcome: Outcome,
        reason: Option<String>,
        retry_at: Option<DateTime<Utc>>,
    ) -> Result<FetchReport, SchedulerError> {
        let last = course.last;
        let persona = course.last_persona.as_ref().map(|persona| PersonaSummary {
            id: persona.id,
            engine: persona.engine,
            claimed_os: persona.claimed_os,
            country: persona.country,
        });
        // The last attempt as the record has it, when this process made
        // none: the request was taken up again and went no further.
        let recorded = course.attempts.last().filter(|_| last.is_none());
        let block = last
            .as_ref()
            .and_then(|last| last.classification.block.clone())
            .or_else(|| recorded.and_then(|attempt| attempt.block.clone()))
            .filter(|_| outcome.is_block());
        let headers = self.shown_headers(last.as_ref());
        let archived = self
            .keep(course.request.id, course.order.keep, last.as_ref())
            .await;
        let reading = self.read(&course.order, outcome, last.as_ref()).await;
        // What the answer says of the visit besides the page is stored
        // with the result, so that the result read later says the same.
        let pacing = PacingReport {
            applied: course.pacing.applied,
            request_ignored: course.pacing.request_ignored,
            waited: course.waited,
        };
        let asked = course.order.capture.console;
        let kept = visit::of(visit::pacing(&pacing), outcome, last.as_ref(), asked);
        let record = FetchRecord {
            id: ResultId::new(),
            request_id: course.request.id,
            outcome,
            status: last
                .as_ref()
                .and_then(|last| last.status)
                .or_else(|| recorded.and_then(|attempt| attempt.status)),
            headers: headers.clone(),
            final_url: last
                .as_ref()
                .and_then(|last| last.final_url.clone())
                .or_else(|| recorded.and_then(|attempt| attempt.final_url.clone())),
            redirects: last
                .as_ref()
                .map_or_else(Vec::new, |last| last.redirects.clone()),
            extract: reading.extract.clone(),
            reading: reading.stored_crawl(),
            xhr: archived.xhr.clone(),
            artifacts: archived.artifacts.clone(),
            persona: persona.clone(),
            block: block.clone(),
            robots: course.robots.clone(),
            cached: false,
            cached_at: None,
            reason: reason.clone(),
            visit: kept.clone(),
            created_at: self.now(),
        };
        self.backend().finish(&record).await?;
        self.fill_cache(&course.order, &record, reading.thin).await;
        if let Some(call) = course.robots_due {
            let readers = course
                .attempts
                .iter()
                .filter_map(|attempt| attempt.persona_id);
            self.follow_up(call, readers);
        }
        let first_vendor = last
            .as_ref()
            .and_then(|last| last.classification.vendors.first())
            .map_or("", |vendor| vendor.as_str());
        metrics::request(&course.domain, course.order.level, outcome, first_vendor);
        Ok(FetchReport {
            request: course.request.id,
            result: record.id,
            outcome,
            reason,
            retry_at,
            attempts: course.attempts,
            last,
            headers,
            artifacts: archived.artifacts,
            xhr: archived.xhr,
            finished_at: record.created_at,
            persona,
            block,
            pacing,
            extract: reading.extract,
            robots: course.robots,
            page: reading.page,
            reading: reading.crawl,
            cached: None,
            visit: kept,
        })
    }
}
