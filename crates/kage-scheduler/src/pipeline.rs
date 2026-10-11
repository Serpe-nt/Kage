//! One fetch from the request to its result (spec §3.2, §8).
//!
//! 1. The SSRF guard looks at the URL; the domain's policy and the render
//!    decision are resolved (spec §8.1): what the request asks for, else
//!    what the policy says, else the browser.
//! 2. The scheduler grants a persona and its slot; the request waits for
//!    it, or is answered at once when the domain is paused, egress is
//!    unsafe, the bandwidth budget is used up or no persona can serve.
//! 3. The executor makes the attempt and the classifier labels it.
//! 4. What the attempt means is settled (cooldown, breaker), and the
//!    rules of [`kage_core::schedule::retry`] say what follows: done, the
//!    same persona in its browser, or another persona after a pause.
//! 5. What the request asked to keep of its page is stored
//!    ([`crate::archive`]), then the request, its attempts and its result.
//!    The result's headers show the names of the cookies a site set and
//!    not their values, unless the configuration says otherwise.
//!
//! A synchronous fetch that takes longer than its wall time is not
//! cancelled: [`Pipeline::fetch_sync`] hands back a [`Running`] fetch,
//! which is the hook for turning it into a job (spec §8.4, milestone M9).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use kage_browser::contract::{CaptureOptions, Wait};
use kage_core::config::{Config, MAX_ATTEMPT_TIMEOUT};
use kage_core::domain::{
    ArtifactRef, Attempt, CountryCode, Engine, FetchRecord, Header, Persona, PersonaSummary,
    Render, RenderMode, RequestRecord, SessionId, redact_set_cookie,
};
use kage_core::ids::{ApiKeyId, AttemptId, JobId, PersonaId, RequestId, ResultId};
use kage_core::outcome::{Block, Outcome};
use kage_core::pacing::Lognormal;
use kage_core::policy::{EffectivePolicy, PacingDecision, PacingRequest};
use kage_core::schedule::planner::{Completion, Dispatch, Refusal, RetryPause, Work};
use kage_core::schedule::politeness::Courtesy;
use kage_core::schedule::queue::Priority;
use kage_core::schedule::retry::{AttemptVerdict, RequestProgress, Ruling, Step};
use kage_core::schedule::selection::Network;
use kage_core::seed::Seed;
use kage_core::ssrf::{SsrfPolicy, check_url};
use kage_http::RequestKind;
use url::Url;

use crate::SchedulerError;
use crate::archive::{Archive, Archived, Keep};
use crate::backend::Backend;
use crate::executor::{
    AttemptExecutor, AttemptJob, AttemptOutput, BehaviorWish, default_capture, default_wait,
};
use crate::metrics;
use crate::scheduler::{Grant, Scheduler};
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
    /// The render mode asked for.
    pub render: RenderMode,
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
    /// The request as it was accepted, for the record.
    pub spec: serde_json::Value,
}

impl FetchOrder {
    /// A plain `GET` of a page with every default: render `auto`, the
    /// policy's pacing and retries, the document captured.
    pub fn get(url: Url, priority: Priority) -> Self {
        Self {
            spec: serde_json::json!({ "url": url.as_str() }),
            url,
            method: http::Method::GET,
            headers: Vec::new(),
            body: None,
            kind: RequestKind::Document,
            render: RenderMode::Auto,
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
    /// Whether the fetch moved from the http path to the browser.
    pub escalated: bool,
}

/// A synchronous fetch that outlasted its wall time. It keeps running;
/// its request is stored and its result will be.
#[derive(Debug)]
pub struct Running {
    /// The request, by which the result will be found.
    pub request: RequestId,
    task: tokio::task::JoinHandle<Result<FetchReport, SchedulerError>>,
}

impl Running {
    /// Waits for the fetch to end.
    ///
    /// # Errors
    ///
    /// What the fetch failed with; [`SchedulerError::ShuttingDown`] when
    /// its task was stopped.
    pub async fn wait(self) -> Result<FetchReport, SchedulerError> {
        self.task.await.unwrap_or(Err(SchedulerError::ShuttingDown))
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
}

/// Counts an attempt and writes its line in the log.
fn note(attempt: &Attempt, domain: &str, dispatch: &Dispatch, output: &AttemptOutput) {
    let vendor = output
        .classification
        .vendors
        .first()
        .map_or("", |vendor| vendor.as_str());
    metrics::attempt(
        domain,
        attempt.render,
        attempt.outcome,
        vendor,
        (attempt.finished_at - attempt.started_at)
            .to_std()
            .unwrap_or(Duration::ZERO),
    );
    tracing::info!(
        request = %attempt.request_id,
        persona = %attempt.persona_id,
        domain,
        render = attempt.render.as_str(),
        outcome = attempt.outcome.as_str(),
        vendor,
        n = attempt.n,
        status = attempt.status,
        total_ms = attempt.timing.total_ms,
        page_of_visit = dispatch.page_of_visit,
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

/// The path of the first attempt (spec §8.1): what the request asks for,
/// else what the policy says, else the browser.
pub fn first_render(requested: RenderMode, policy: &EffectivePolicy) -> Render {
    policy.render_for(requested)
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
    /// The persona that makes the next attempt, when it is its own second
    /// try.
    follow_up: Option<PersonaId>,
    /// Networks of the personas that were blocked.
    avoid: Vec<Network>,
    /// The pause before the next attempt, when it is another persona's.
    retry: Option<RetryPause>,
    give_up_at: Option<DateTime<Utc>>,
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
        }
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

    fn backend(&self) -> &Arc<dyn Backend> {
        self.scheduler.backend()
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
        let mut task = tokio::spawn(async move { pipeline.fetch_as(request, order, None).await });
        match tokio::time::timeout(wall, &mut task).await {
            Ok(finished) => finished
                .unwrap_or(Err(SchedulerError::ShuttingDown))
                .map(|report| Synchronous::Finished(Box::new(report))),
            Err(_) => Ok(Synchronous::Overflowed(Running { request, task })),
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
    ) -> Result<Course, SchedulerError> {
        let domain = order
            .url
            .host_str()
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| SchedulerError::NoHost(order.url.to_string()))?;
        let policies = self.verdicts.policies().current();
        let policy = policies.domains.resolve(&domain);
        let pacing = policy.pacing_for(order.pacing.as_ref(), &policies.pacing)?;
        let request = RequestRecord {
            id,
            api_key_id: order.api_key,
            job_id: order.job,
            session_id: order.session.clone(),
            url: order.url.to_string(),
            method: order.method.to_string(),
            domain: domain.clone(),
            render: order.render,
            spec: order.spec.clone(),
            created_at: self.now(),
            finished_at: None,
            outcome: None,
        };
        self.backend().save_request(&request).await?;

        let retries = order.max_retries.unwrap_or(policy.retries);
        Ok(Course {
            progress: RequestProgress::new(
                order.render,
                first_render(order.render, &policy),
                retries,
            ),
            order,
            request,
            domain,
            policy,
            pacing,
            attempts: Vec::new(),
            last: None,
            last_persona: None,
            waited: Duration::ZERO,
            follow_up: None,
            avoid: Vec::new(),
            retry: None,
            give_up_at,
        })
    }

    async fn fetch_as(
        &self,
        id: RequestId,
        order: FetchOrder,
        give_up_at: Option<DateTime<Utc>>,
    ) -> Result<FetchReport, SchedulerError> {
        let mut course = self.begin(id, order, give_up_at).await?;

        // The guard looks at the URL before anything is sent anywhere.
        if let Err(denial) = check_url(&course.order.url, self.settings.security) {
            tracing::info!(request = %id, %denial, "the URL is refused");
            return self
                .conclude(
                    course,
                    Outcome::SsrfDenied,
                    Some("ssrf_denied".to_owned()),
                    None,
                )
                .await;
        }

        loop {
            let grant = match self.scheduler.acquire(self.work(&course)).await {
                Ok(grant) => grant,
                Err(refused) => {
                    // Under `auto` an http attempt nobody can make is a
                    // browser attempt: no persona of this pool has an
                    // http path until an engine major has a profile.
                    if refused.reason == Refusal::NoPersona
                        && course.progress.render() == Render::Http
                        && course.order.render == RenderMode::Auto
                        && course.attempts.is_empty()
                    {
                        course.progress = RequestProgress::new(
                            course.order.render,
                            Render::Browser,
                            course.order.max_retries.unwrap_or(course.policy.retries),
                        );
                        continue;
                    }
                    if course.follow_up.is_some() {
                        // The persona's own browser cannot be had: its
                        // http block stands, and another persona tries.
                        self.forfeit(&mut course).await?;
                        continue;
                    }
                    // What was reached before stands; a request that never
                    // got an attempt is answered with the reason.
                    let (outcome, reason) = match &course.last {
                        Some(last) => (last.outcome(), last.reason.clone()),
                        None => (
                            Outcome::ProviderError,
                            Some(refused.reason.as_str().to_owned()),
                        ),
                    };
                    return self
                        .conclude(course, outcome, reason, refused.retry_at)
                        .await;
                }
            };
            let ruling = self.attempt(&mut course, grant).await?;
            // A session's persona that was turned away cannot be replaced:
            // the request ends with what it got.
            let pinned_and_blocked = course.order.pin.is_some() && ruling.penalty.is_some();
            match ruling.step {
                Step::RetryOtherPersona { .. } if !pinned_and_blocked => {
                    course.follow_up = None;
                    course.retry = Some(self.retry_pause(&course));
                }
                Step::EscalateSamePersona => {
                    course.follow_up = course.last_persona.as_ref().map(|persona| persona.id);
                    course.retry = None;
                }
                Step::Finish | Step::RetryOtherPersona { .. } => {
                    let (outcome, reason) = course
                        .last
                        .as_ref()
                        .map_or((Outcome::ProviderError, None), |last| {
                            (last.outcome(), last.reason.clone())
                        });
                    return self.conclude(course, outcome, reason, None).await;
                }
            }
        }
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

    /// Gives up a persona's own second try: the http block it was to cure
    /// is settled as it stands, and the next attempt is another persona's.
    async fn forfeit(&self, course: &mut Course) -> Result<(), SchedulerError> {
        course.follow_up = None;
        course.retry = Some(self.retry_pause(course));
        let Some((id, verdict)) = course.progress.forfeit() else {
            return Ok(());
        };
        let Some(persona) = course
            .last_persona
            .as_ref()
            .filter(|persona| persona.id == id)
        else {
            return Ok(());
        };
        self.verdicts
            .settle(
                (persona.id, &persona.seed),
                &course.domain,
                verdict.outcome,
                Some(&verdict),
                false,
                &self.verdicts.breaker_rules(&course.domain),
            )
            .await?;
        Ok(())
    }

    /// The next attempt of a request, as work for the scheduler.
    fn work(&self, course: &Course) -> Work {
        let order = &course.order;
        let engines: Vec<Engine> = course
            .policy
            .engines
            .iter()
            .copied()
            .filter(|engine| order.engines.is_empty() || order.engines.contains(engine))
            .collect();
        let rules = self.verdicts.breaker_rules(&course.domain);
        Work {
            domain: course.domain.clone(),
            priority: order.priority,
            render: course.progress.render(),
            // Its own second try, else its session's persona.
            pin: course.follow_up.or(order.pin),
            exclude: if course.follow_up.is_some() || order.pin.is_some() {
                Vec::new()
            } else {
                course.progress.used().to_vec()
            },
            avoid: course.avoid.clone(),
            engines,
            countries: course.policy.countries.clone(),
            prefer_countries: order.prefer_countries.clone(),
            pacing: course.pacing.profile,
            active_hours: course.policy.active_hours,
            breaker_threshold: rules.threshold,
            breaker_pause: rules.pause,
            follow_up: course.follow_up.is_some(),
            retry_pause: course.retry,
            not_before: None,
            give_up_at: course.give_up_at,
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
            return Ok(course.progress.record(dispatch.persona, verdict));
        };
        let seed = attempt_seed(&persona.seed, course.request.id, n);
        let job = self.job(course, &dispatch, &persona, n, seed);

        let started_at = self.now();
        let output = self.executor.run(&job).await;
        let finished_at = self.now();
        let outcome = output.outcome();
        let verdict = AttemptVerdict::of(&output.classification);
        let ruling = course.progress.record(persona.id, verdict);

        // What the persona answers for; an http block that its own
        // browser is about to cure is nothing yet.
        if ruling.step != Step::EscalateSamePersona {
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
        }
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
            persona_id: persona.id,
            engine: persona.engine,
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
        };
        self.backend().save_attempt(&attempt).await?;
        note(&attempt, &course.domain, &dispatch, &output);
        course.attempts.push(attempt);
        course.last = Some(output);
        course.last_persona = Some(persona);
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
        let block = last
            .as_ref()
            .and_then(|last| last.classification.block.clone())
            .filter(|_| outcome.is_block());
        let headers = last.as_ref().map_or_else(Vec::<Header>::new, |last| {
            if self.settings.expose_set_cookie {
                last.headers.clone()
            } else {
                redact_set_cookie(&last.headers)
            }
        });
        let archived = match (&self.archive, &last) {
            (Some(archive), Some(last)) => {
                archive
                    .keep(course.request.id, course.order.keep, last, self.now())
                    .await
            }
            _ => Archived::default(),
        };
        let record = FetchRecord {
            id: ResultId::new(),
            request_id: course.request.id,
            outcome,
            status: last.as_ref().and_then(|last| last.status),
            headers: headers.clone(),
            final_url: last.as_ref().and_then(|last| last.final_url.clone()),
            redirects: last
                .as_ref()
                .map_or_else(Vec::new, |last| last.redirects.clone()),
            extract: None,
            xhr: archived.xhr.clone(),
            artifacts: archived.artifacts.clone(),
            persona: persona.clone(),
            block: block.clone(),
            robots: None,
            cached: false,
            reason: reason.clone(),
            created_at: self.now(),
        };
        self.backend().finish(&record).await?;
        let first_vendor = last
            .as_ref()
            .and_then(|last| last.classification.vendors.first())
            .map_or("", |vendor| vendor.as_str());
        metrics::request(
            &course.domain,
            course.progress.render(),
            outcome,
            first_vendor,
        );
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
            pacing: PacingReport {
                applied: course.pacing.applied,
                request_ignored: course.pacing.request_ignored,
                waited: course.waited,
            },
            escalated: course.progress.escalated(),
        })
    }
}
