//! The job worker (spec §16.6): what claims jobs and works them.
//!
//! One loop claims jobs nobody works (`SELECT ... FOR UPDATE SKIP LOCKED`
//! under a lease) and gives each to a runner of its own. A runner renews
//! its lease while it works and reads, with every renewal, whether a
//! cancel was asked for. A job whose worker stopped is claimed again when
//! its lease ran out, by this server after a restart or by another one,
//! and goes on where its items stand.
//!
//! # A batch
//!
//! The items of a batch are worked site by site. Per site at most as many
//! are in the scheduler as the batch has visitors there (one, unless its
//! caller asked for more), in the order given, and each names the personas
//! that read the site for the batch so far: the scheduler then keeps the
//! batch with them through their gaps, their breaks and their nights. An
//! item that cannot be attempted now is not failed: it goes back into the
//! queue with the time to come back, its site is left alone until then,
//! and a job whose every remaining item waits is given back as `paused`
//! until the earliest of those times, so that it holds no place among the
//! running jobs meanwhile.
//!
//! # A flow
//!
//! A flow is one unit: one browser attempt that runs its steps. Its items
//! are the steps and one last item for the page it ended on; they are
//! written when the flow is over, each capture stored as a result of its
//! own.
//!
//! # A cancel, a stop, a deadline
//!
//! A cancel ends what waits at once, takes an item that waits for its
//! persona out of the scheduler's queue, and lets an attempt that is under
//! way finish: a page is never cut in the middle. A server that stops does
//! the same and gives its jobs back; nothing is lost, an item that was
//! being fetched is taken up again under its request. A job that waited as
//! long as it may (`jobs.max_age`) ends with what it has; the items left
//! fail with what they were waiting for.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use kage_core::domain::{
    ApiKey, Job, JobItem, JobItemState, JobKind, JobOrigin, JobState, SessionId, WebhookEvent,
};
use kage_core::ids::{JobId, JobItemId, PersonaId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::schedule::planner::{Refusal, Visitors};
use kage_core::schedule::queue::Priority;
use kage_proxy::CredentialCipher;
use kage_scheduler::{Capture, FetchOrder, FetchReport, JobFetch, Running, SchedulerError, Stop};
use kage_store::StoreError;
use serde_json::{Value, json};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use uuid::Uuid;

use super::request::{Checks, FlowJob, JobRequest, plan_flow};
use super::webhook::{self, WebhookSender};
use crate::error::ErrorCode;
use crate::fetch::conflict_of;
use crate::fetch::request::{FetchRequest, Planned, plan};
use crate::state::{ApiState, EngineParts};

/// How long a page waits in the scheduler for its persona before the job
/// would rather be told when to come back: longer than a gap or a break
/// between bursts, shorter than a visitor's long break or its night.
const PATIENCE: Duration = Duration::from_mins(5);

/// A wait shorter than this is waited out with the job in hand; for a
/// longer one the job is given back as `paused`.
const HOLD: Duration = Duration::from_secs(30);

/// How often a runner that could not start an item because every place in
/// the scheduler is taken looks again.
const STARVED: Duration = Duration::from_millis(250);

/// How long the worker waits for its runners to give their jobs back when
/// the server stops.
const WIND_DOWN: Duration = Duration::from_secs(20);

/// What a runner is told from outside. A later variant outranks an
/// earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Signal {
    /// Go on.
    Run,
    /// A cancel was asked for.
    Cancel,
    /// The server is stopping.
    Shutdown,
    /// The lease is another worker's: the job is not this runner's any
    /// more.
    Lost,
}

/// What the job endpoints, the worker and the webhook sender share.
pub(crate) struct JobsHub {
    /// This server's name among workers.
    pub(crate) worker: Uuid,
    /// Seals and opens webhook secrets.
    pub(crate) cipher: CredentialCipher,
    /// Signs idempotent requests' fingerprints.
    pub(crate) idempotency_key: [u8; 32],
    work: Notify,
    in_flight: Arc<Semaphore>,
    nudges: Mutex<HashMap<JobId, Arc<Notify>>>,
    sender: Mutex<Option<Arc<WebhookSender>>>,
}

impl std::fmt::Debug for JobsHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobsHub")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl JobsHub {
    pub(crate) fn new(cipher: CredentialCipher, master_key: &[u8; 32], in_flight: u32) -> Self {
        use hmac::{Hmac, KeyInit, Mac};
        let mut idempotency_key = [0_u8; 32];
        // A key of its own, so that the master key itself signs nothing.
        if let Ok(mut mac) = Hmac::<sha2::Sha256>::new_from_slice(master_key) {
            mac.update(b"kage-idempotency-key-v1");
            idempotency_key.copy_from_slice(&mac.finalize().into_bytes());
        }
        Self {
            worker: Uuid::now_v7(),
            cipher,
            idempotency_key,
            work: Notify::new(),
            in_flight: Arc::new(Semaphore::new(
                usize::try_from(in_flight.max(1)).unwrap_or(1),
            )),
            nudges: Mutex::new(HashMap::new()),
            sender: Mutex::new(None),
        }
    }

    /// Tells the worker that there is something to look at: a new job, or
    /// a cancel.
    pub(crate) fn wake(&self) {
        self.work.notify_one();
    }

    /// Tells the runner of a job, when this server has one, to read the
    /// job's row now: a cancel is then seen at once.
    pub(crate) fn nudge(&self, job: JobId) {
        let nudge = self
            .nudges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&job)
            .cloned();
        if let Some(nudge) = nudge {
            nudge.notify_one();
        }
        self.wake();
    }

    fn watch(&self, job: JobId) -> Arc<Notify> {
        Arc::clone(
            self.nudges
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(job)
                .or_default(),
        )
    }

    fn unwatch(&self, job: JobId) {
        self.nudges
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&job);
    }

    /// The sender whose events the worker queues, once the server runs
    /// one.
    pub(crate) fn set_sender(&self, sender: Arc<WebhookSender>) {
        *self.sender.lock().unwrap_or_else(PoisonError::into_inner) = Some(sender);
    }

    /// Tells the webhook sender that an event was queued.
    pub(crate) fn wake_sender(&self) {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(sender) = sender {
            sender.wake();
        }
    }
}

/// Why a job could not be worked just now.
#[derive(Debug, thiserror::Error)]
enum WorkError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error("a task of the job did not come back: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// How one unit of a job ended.
#[derive(Debug)]
enum ItemEnd {
    /// It was fetched now.
    Finished(Box<FetchReport>),
    /// Its result was stored already: the job changed hands between the
    /// result and the item.
    Stored {
        outcome: Outcome,
        reason: Option<String>,
    },
    /// It cannot be attempted now.
    Deferred {
        reason: String,
        until: Option<DateTime<Utc>>,
    },
    /// The stop signal took it out of the scheduler's queue.
    Stopped,
    /// It cannot be run at all.
    Failed {
        error: String,
        outcome: Option<Outcome>,
    },
}

/// An item that came back from the scheduler.
struct Ended {
    item: JobItem,
    site: String,
    end: ItemEnd,
}

/// Why a job stops being worked before its items are done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    Cancelled,
    Expired,
    Shutdown,
    Lost,
}

/// How long an item that was refused without a time waits.
fn default_wait(reason: &str) -> Duration {
    match reason {
        "bandwidth_exhausted" => Duration::from_mins(10),
        _ => Duration::from_mins(1),
    }
}

fn later(now: DateTime<Utc>, wait: Duration) -> DateTime<Utc> {
    chrono::Duration::from_std(wait)
        .ok()
        .and_then(|wait| now.checked_add_signed(wait))
        .unwrap_or(now)
}

/// What an item that failed says: the reason the scheduler gave, else the
/// outcome's name.
fn error_of(outcome: Outcome, reason: Option<&str>) -> Option<String> {
    (!outcome.is_success()).then(|| reason.unwrap_or(outcome.as_str()).to_owned())
}

/// What working one job needs.
#[derive(Clone)]
struct Work {
    state: ApiState,
    engine: EngineParts,
    job: JobId,
    key: Arc<ApiKey>,
    session: Option<SessionId>,
}

impl Work {
    fn now(&self) -> DateTime<Utc> {
        self.state.clock().now()
    }

    /// The request of an item as the pipeline takes it, checked again
    /// under the policies in force now.
    fn planned(&self, input: &Value) -> Result<Planned, String> {
        let request: FetchRequest = serde_json::from_value(input.clone())
            .map_err(|_| ErrorCode::ValidationFailed.as_str().to_owned())?;
        let policies = self.state.policies().current();
        plan(&request, &policies.pacing).map_err(|error| error.code().as_str().to_owned())
    }

    /// Pins an order to the job's session, when it has one.
    async fn pin(&self, planned: &mut Planned) -> Result<(), String> {
        let Some(session) = &self.session else {
            return Ok(());
        };
        let found = self
            .engine
            .personas
            .sessions()
            .resolve(self.key.id, session)
            .await
            .map_err(|_| "internal".to_owned())?;
        let Some(found) = found else {
            return Err(ErrorCode::SessionNotFound.as_str().to_owned());
        };
        let host = planned.order.url.host_str().unwrap_or_default();
        let policy = self.state.policies().current().domains.resolve(host);
        if conflict_of(&found.persona, planned, &policy).is_some() {
            return Err(ErrorCode::SessionPersonaConflict.as_str().to_owned());
        }
        planned.order.pin = Some(found.persona.id);
        Ok(())
    }

    /// What is stored about the request an item was given, when it was
    /// begun before: `Err` with the end of the item when its result exists
    /// already.
    async fn earlier(
        &self,
        request: Option<RequestId>,
    ) -> Result<Result<Option<Vec<kage_core::domain::Attempt>>, ItemEnd>, StoreError> {
        let Some(id) = request else {
            return Ok(Ok(None));
        };
        let store = self.state.store();
        if let Some(record) = store.results().get_by_request(id).await? {
            return Ok(Err(ItemEnd::Stored {
                outcome: record.outcome,
                reason: record.reason,
            }));
        }
        if store.requests().get(id).await?.is_none() {
            // Its id was written down and the process went away before the
            // request was: it begins now.
            return Ok(Ok(None));
        }
        Ok(Ok(Some(store.attempts().list_for_request(id).await?)))
    }

    /// Counts a page against the key's daily budget; `Err` with the time
    /// to come back when it is used up.
    async fn spend_page(&self) -> Result<(), ItemEnd> {
        let store = self.state.store().clone();
        let key = self.key.id;
        let spent = self
            .state
            .limits()
            .spend_page(&self.key, self.now(), async move |since| {
                store
                    .requests()
                    .count_for_key_since(key, since)
                    .await
                    .map_err(|error| crate::error::ApiError::internal(&error))
            })
            .await;
        match spent {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ErrorCode::BudgetExhausted => {
                let tomorrow = self
                    .now()
                    .date_naive()
                    .succ_opt()
                    .and_then(|day| day.and_hms_opt(0, 0, 0))
                    .map(|midnight| midnight.and_utc());
                Err(ItemEnd::Deferred {
                    reason: ErrorCode::BudgetExhausted.as_str().to_owned(),
                    until: tomorrow,
                })
            }
            Err(_) => Err(ItemEnd::Failed {
                error: "internal".to_owned(),
                outcome: None,
            }),
        }
    }

    /// Runs an order for the job under a request id, new or taken up
    /// again.
    async fn fetch(
        &self,
        request: RequestId,
        order: FetchOrder,
        earlier: Option<Vec<kage_core::domain::Attempt>>,
        stop: Stop,
    ) -> ItemEnd {
        match self
            .engine
            .pipeline
            .fetch_for_job(request, order, earlier, stop)
            .await
        {
            Ok(JobFetch::Finished(report)) => ItemEnd::Finished(report),
            Ok(JobFetch::Deferred(deferred)) => ItemEnd::Deferred {
                reason: deferred.reason.as_str().to_owned(),
                until: deferred.retry_at,
            },
            Ok(JobFetch::Stopped) | Err(SchedulerError::ShuttingDown) => ItemEnd::Stopped,
            Err(SchedulerError::Pacing(_)) => ItemEnd::Failed {
                error: ErrorCode::InvalidPacing.as_str().to_owned(),
                outcome: None,
            },
            Err(SchedulerError::NoHost(_)) => ItemEnd::Failed {
                error: ErrorCode::InvalidUrl.as_str().to_owned(),
                outcome: None,
            },
            Err(error) => {
                tracing::error!(job = %self.job, %request, %error, "an item of a job failed inside");
                ItemEnd::Failed {
                    error: "internal".to_owned(),
                    outcome: None,
                }
            }
        }
    }

    /// One item of a batch, from its stored request to its end.
    async fn item(
        self,
        mut item: JobItem,
        site: String,
        visitors: Option<Visitors>,
        stop: Stop,
        _place: OwnedSemaphorePermit,
    ) -> Ended {
        let end = self.run_item(&mut item, visitors, stop).await;
        Ended { item, site, end }
    }

    async fn run_item(
        &self,
        item: &mut JobItem,
        visitors: Option<Visitors>,
        stop: Stop,
    ) -> ItemEnd {
        let failed = |error: String| ItemEnd::Failed {
            error,
            outcome: None,
        };
        let mut planned = match self.planned(&item.input) {
            Ok(planned) => planned,
            Err(error) => return failed(error),
        };
        let earlier = match self.earlier(item.request_id).await {
            Ok(Ok(earlier)) => earlier,
            Ok(Err(end)) => return end,
            Err(error) => {
                tracing::warn!(job = %self.job, %error, "an item's request could not be read");
                return ItemEnd::Deferred {
                    reason: "store".to_owned(),
                    until: None,
                };
            }
        };
        if let Err(error) = self.pin(&mut planned).await {
            return failed(error);
        }
        // A page counts against the key's day when its request begins.
        if earlier.is_none()
            && let Err(end) = self.spend_page().await
        {
            return end;
        }
        let request = item.request_id.unwrap_or_default();
        let now = self.now();
        if let Err(error) = self
            .state
            .store()
            .job_items()
            .start(item.id, Some(request), now)
            .await
        {
            tracing::warn!(job = %self.job, %error, "an item could not be marked as running");
            return ItemEnd::Deferred {
                reason: "store".to_owned(),
                until: None,
            };
        }
        item.request_id = Some(request);
        item.started_at.get_or_insert(crate::stored(now));
        item.state = JobItemState::Running;
        let order = FetchOrder {
            priority: Priority::Job,
            api_key: Some(self.key.id),
            job: Some(self.job),
            spec: item.input.clone(),
            visitors,
            patience: Some(PATIENCE),
            ..planned.order
        };
        self.fetch(request, order, earlier, stop).await
    }

    /// Ends the request of an item nobody will attempt any more, so that
    /// whoever asks for its result is told why there is none.
    async fn abandon(&self, item: &JobItem, reason: &str) {
        let Some(request) = item.request_id else {
            return;
        };
        let Ok(planned) = self.planned(&item.input) else {
            return;
        };
        let store = self.state.store();
        let open = matches!(store.requests().get(request).await, Ok(Some(_)))
            && matches!(store.results().get_by_request(request).await, Ok(None));
        if !open {
            return;
        }
        let earlier = store
            .attempts()
            .list_for_request(request)
            .await
            .unwrap_or_default();
        let order = FetchOrder {
            priority: Priority::Job,
            api_key: Some(self.key.id),
            job: Some(self.job),
            spec: item.input.clone(),
            ..planned.order
        };
        if let Err(error) = self
            .engine
            .pipeline
            .abandon(request, order, earlier, reason)
            .await
        {
            tracing::warn!(job = %self.job, %request, %error, "a request could not be ended");
        }
    }
}

/// Raises a runner's signal; a signal never goes down.
fn raise(signal: &watch::Sender<Signal>, to: Signal) {
    signal.send_if_modified(|current| {
        if to > *current {
            *current = to;
            true
        } else {
            false
        }
    });
}

/// Keeps a job's lease while it is worked, and reads what the job's row
/// says: a cancel, or that the job is another worker's by now. Also keeps
/// the job's session in use.
async fn keep_lease(
    work: Work,
    signal: watch::Sender<Signal>,
    nudge: Arc<Notify>,
    mut stopping: watch::Receiver<bool>,
) {
    let lease = work.state.config().jobs.lease.get();
    let mut beat = tokio::time::interval(lease / 3);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut told = false;
    loop {
        tokio::select! {
            _ = beat.tick() => {}
            () = nudge.notified() => {}
            changed = stopping.changed(), if !told => {
                if changed.is_err() || *stopping.borrow() {
                    told = true;
                    raise(&signal, Signal::Shutdown);
                }
            }
        }
        let now = work.now();
        match work
            .state
            .store()
            .jobs()
            .renew(work.job, work.state.jobs().worker, later(now, lease))
            .await
        {
            Ok(Some(job)) if job.cancel_requested => raise(&signal, Signal::Cancel),
            Ok(Some(_)) => {}
            Ok(None) => {
                raise(&signal, Signal::Lost);
                return;
            }
            Err(error) => {
                tracing::warn!(job = %work.job, %error, "a job's lease could not be renewed");
            }
        }
        if let Some(session) = &work.session {
            // The job counts as use of its session while it has items left.
            let _ = work
                .engine
                .personas
                .sessions()
                .resolve(work.key.id, session)
                .await;
        }
    }
}

/// One job being worked.
struct Runner {
    work: Work,
    job: Job,
    signal: watch::Receiver<Signal>,
    stop: watch::Sender<bool>,
}

impl Runner {
    fn store(&self) -> &kage_store::Store {
        self.work.state.store()
    }

    fn now(&self) -> DateTime<Utc> {
        self.work.now()
    }

    fn hub(&self) -> &JobsHub {
        self.work.state.jobs()
    }

    /// Whether the job goes on, and if not, why.
    fn ending(&self) -> Option<Ending> {
        match *self.signal.borrow() {
            Signal::Lost => Some(Ending::Lost),
            Signal::Shutdown => Some(Ending::Shutdown),
            Signal::Cancel => Some(Ending::Cancelled),
            Signal::Run => self
                .job
                .deadline
                .is_some_and(|deadline| deadline <= self.now())
                .then_some(Ending::Expired),
        }
    }

    /// How many more items are over, and the event that reports it when
    /// the job asked for one.
    async fn progressed(&self, job: &Job) -> Result<(), StoreError> {
        metrics::gauge!("kage_job_items_pending", "job_type" => job.kind.as_str())
            .set(f64::from(job.items_pending()));
        let every = job
            .spec
            .get("progress_every")
            .and_then(Value::as_u64)
            .and_then(|every| u32::try_from(every).ok())
            .filter(|every| *every > 0);
        if let Some(every) = every
            && job.items_pending() > 0
            && job.items_finished() > 0
            && job.items_finished().is_multiple_of(every)
            && webhook::enqueue(self.store(), job, WebhookEvent::JobProgress, self.now()).await?
        {
            self.hub().wake_sender();
        }
        Ok(())
    }

    /// Writes an item's end and moves the job's counters by it.
    async fn finish_item(
        &self,
        mut item: JobItem,
        state: JobItemState,
        outcome: Option<Outcome>,
        error: Option<String>,
    ) -> Result<(), StoreError> {
        item.state = state;
        item.outcome = outcome;
        item.error = error;
        item.not_before = None;
        item.finished_at = Some(crate::stored(self.now()));
        metrics::counter!(
            "kage_job_items_total",
            "job_type" => self.job.kind.as_str(),
            "state" => state.as_str()
        )
        .increment(1);
        if let Some(job) = self.store().job_items().finish(&item).await? {
            self.progressed(&job).await?;
        }
        Ok(())
    }

    /// Ends the job as its items ended, and reports it.
    async fn finalize(&self, error: Option<&str>) -> Result<(), StoreError> {
        let Some(job) = self.store().jobs().get(self.job.id).await? else {
            return Ok(());
        };
        let state = if error.is_some() && job.items_succeeded == 0 {
            JobState::Failed
        } else {
            job.final_state()
        };
        let now = self.now();
        let Some(ended) = self
            .store()
            .jobs()
            .finish(job.id, state, error, crate::stored(now))
            .await?
        else {
            return Ok(());
        };
        metrics::counter!(
            "kage_jobs_finished_total",
            "job_type" => ended.kind.as_str(),
            "state" => ended.state.as_str()
        )
        .increment(1);
        tracing::info!(
            job = %ended.id,
            kind = ended.kind.as_str(),
            state = ended.state.as_str(),
            succeeded = ended.items_succeeded,
            failed = ended.items_failed,
            cancelled = ended.items_cancelled,
            "a job has ended"
        );
        if webhook::enqueue(self.store(), &ended, webhook::end_event(&ended), now).await? {
            self.hub().wake_sender();
        }
        Ok(())
    }

    /// Gives the job back without ending it.
    async fn release(&self, paused: Option<(DateTime<Utc>, &str)>) -> Result<(), StoreError> {
        self.store()
            .jobs()
            .release(self.job.id, self.hub().worker, paused)
            .await?;
        Ok(())
    }

    /// Waits until `until`, unless something is said meanwhile. Returns
    /// whether the time passed undisturbed.
    async fn wait_until(&mut self, until: DateTime<Utc>) -> bool {
        let wait = (until - self.now()).to_std().unwrap_or(Duration::ZERO);
        tokio::select! {
            () = tokio::time::sleep(wait) => true,
            _ = self.signal.changed() => false,
        }
    }

    async fn run(mut self) {
        if self.job.state == JobState::Paused {
            // It is being worked again.
            let _ = self.store().jobs().set_paused(self.job.id, None).await;
        }
        let worked = match self.job.kind {
            JobKind::Batch => self.batch().await,
            JobKind::Flow => Box::pin(self.flow()).await,
            JobKind::Crawl => self.fail_all("not_available_yet").await,
        };
        if let Err(error) = worked {
            tracing::error!(job = %self.job.id, %error, "a job could not be worked; it is tried again later");
            let again = later(self.now(), Duration::from_secs(30));
            let _ = self.release(Some((again, "error"))).await;
        }
    }

    /// Fails everything that is left of a job for one reason.
    async fn fail_all(&mut self, reason: &str) -> Result<(), WorkError> {
        self.store()
            .job_items()
            .end_queued(self.job.id, JobItemState::Failed, Some(reason), self.now())
            .await?;
        for item in self.store().job_items().open(self.job.id).await? {
            self.finish_item(item, JobItemState::Failed, None, Some(reason.to_owned()))
                .await?;
        }
        self.finalize(Some(reason)).await?;
        Ok(())
    }

    // --- a batch ---------------------------------------------------------

    /// The personas of an item's report that are the site's visitors from
    /// now on.
    fn learn(known: &mut Vec<PersonaId>, report: &FetchReport, most: u32) -> bool {
        let before = known.clone();
        // Whoever was turned away is not waited for.
        for attempt in &report.attempts {
            if attempt.outcome.is_block() {
                known.retain(|persona| *persona != attempt.persona_id);
            }
        }
        if report.outcome.is_success()
            && let Some(persona) = &report.persona
        {
            known.retain(|known| *known != persona.id);
            known.push(persona.id);
        }
        let keep = usize::try_from(most.max(1)).unwrap_or(1);
        if known.len() > keep {
            known.drain(..known.len() - keep);
        }
        *known != before
    }

    #[allow(clippy::too_many_lines)] // One loop: start what may start, wait, take what came back.
    async fn batch(&mut self) -> Result<(), WorkError> {
        let job = self.job.id;
        let most = if self.work.session.is_some() || self.job.origin != JobOrigin::Api {
            1
        } else {
            self.job
                .spec
                .get("visitors")
                .and_then(Value::as_u64)
                .and_then(|visitors| u32::try_from(visitors).ok())
                .unwrap_or(1)
                .max(1)
        };
        let named = self.work.session.is_none() && self.job.origin == JobOrigin::Api;
        let mut visitors: HashMap<String, Vec<PersonaId>> =
            serde_json::from_value(self.store().jobs().visitors(job).await?).unwrap_or_default();
        let mut queues: BTreeMap<String, VecDeque<JobItem>> = BTreeMap::new();
        for item in self.store().job_items().open(job).await? {
            queues
                .entry(item.site.clone().unwrap_or_default())
                .or_default()
                .push_back(item);
        }
        let waited_for = self
            .job
            .paused_reason
            .clone()
            .unwrap_or_else(|| "waiting".to_owned());
        // A site whose next item is not to be taken up yet is left alone.
        let mut held: HashMap<String, (DateTime<Utc>, String)> = queues
            .iter()
            .filter_map(|(site, queue)| {
                let until = queue.front()?.not_before?;
                Some((site.clone(), (until, waited_for.clone())))
            })
            .collect();
        let mut running: HashMap<String, u32> = HashMap::new();
        let mut set: JoinSet<Ended> = JoinSet::new();
        let stop = self.stop.subscribe();

        loop {
            if let Some(ending) = self.ending() {
                let reason = held
                    .values()
                    .map(|(_, reason)| reason.as_str())
                    .next()
                    .unwrap_or("waiting")
                    .to_owned();
                return self
                    .end_batch(ending, set, &mut visitors, most, &reason)
                    .await;
            }
            let now = self.now();
            let mut starved = false;
            'sites: for (site, queue) in &mut queues {
                loop {
                    let in_flight = running.get(site).copied().unwrap_or(0);
                    if in_flight >= most || queue.is_empty() {
                        break;
                    }
                    if held.get(site).is_some_and(|(until, _)| *until > now) {
                        break;
                    }
                    let Ok(place) = Arc::clone(&self.hub().in_flight).try_acquire_owned() else {
                        starved = true;
                        break 'sites;
                    };
                    let Some(item) = queue.pop_front() else {
                        break;
                    };
                    held.remove(site);
                    *running.entry(site.clone()).or_default() += 1;
                    let known = named.then(|| Visitors {
                        known: visitors.get(site).cloned().unwrap_or_default(),
                        max: most,
                    });
                    set.spawn(self.work.clone().item(
                        item,
                        site.clone(),
                        known,
                        stop.clone(),
                        place,
                    ));
                }
            }
            queues.retain(|_, queue| !queue.is_empty());

            // When to look again if nothing comes back first.
            let next = queues
                .keys()
                .filter(|site| running.get(*site).copied().unwrap_or(0) < most)
                .filter_map(|site| held.get(site).map(|(until, _)| *until))
                .min();
            if set.is_empty() {
                if queues.is_empty() {
                    self.finalize(None).await?;
                    return Ok(());
                }
                if let Some(until) = next.filter(|until| *until > later(now, HOLD)) {
                    // Everything left waits: the job is given back.
                    let reason = held
                        .values()
                        .min_by_key(|(until, _)| *until)
                        .map_or("waiting", |(_, reason)| reason.as_str());
                    self.release(Some((until, reason))).await?;
                    return Ok(());
                }
            }
            let wake = if starved {
                STARVED
            } else {
                next.map_or(Duration::from_hours(1), |until| {
                    (until - now).to_std().unwrap_or(Duration::ZERO)
                })
            };
            tokio::select! {
                Some(joined) = set.join_next(), if !set.is_empty() => {
                    let ended = joined?;
                    if let Some(count) = running.get_mut(&ended.site) {
                        *count = count.saturating_sub(1);
                    }
                    self.item_back(ended, &mut queues, &mut held, &mut visitors, most).await?;
                }
                _ = self.signal.changed() => {}
                () = tokio::time::sleep(wake) => {}
            }
        }
    }

    /// Takes an item that came back while the job goes on.
    async fn item_back(
        &self,
        ended: Ended,
        queues: &mut BTreeMap<String, VecDeque<JobItem>>,
        held: &mut HashMap<String, (DateTime<Utc>, String)>,
        visitors: &mut HashMap<String, Vec<PersonaId>>,
        most: u32,
    ) -> Result<(), WorkError> {
        let Ended {
            mut item,
            site,
            end,
        } = ended;
        match end {
            ItemEnd::Finished(report) => {
                let known = visitors.entry(site).or_default();
                if Self::learn(known, &report, most) {
                    self.store()
                        .jobs()
                        .set_visitors(self.job.id, &json!(visitors))
                        .await?;
                }
                let state = if report.outcome.is_success() {
                    JobItemState::Succeeded
                } else {
                    JobItemState::Failed
                };
                let error = error_of(report.outcome, report.reason.as_deref());
                self.finish_item(item, state, Some(report.outcome), error)
                    .await?;
            }
            ItemEnd::Stored { outcome, reason } => {
                let state = if outcome.is_success() {
                    JobItemState::Succeeded
                } else {
                    JobItemState::Failed
                };
                let error = error_of(outcome, reason.as_deref());
                self.finish_item(item, state, Some(outcome), error).await?;
            }
            ItemEnd::Failed { error, outcome } => {
                self.finish_item(item, JobItemState::Failed, outcome, Some(error))
                    .await?;
            }
            ItemEnd::Deferred { reason, until } => {
                let until = until.unwrap_or_else(|| later(self.now(), default_wait(&reason)));
                self.store().job_items().defer(item.id, Some(until)).await?;
                item.state = JobItemState::Queued;
                item.not_before = Some(until);
                held.insert(site.clone(), (until, reason));
                queues.entry(site).or_default().push_front(item);
            }
            ItemEnd::Stopped => {
                // Nobody asked it to stop yet: it waits its turn again.
                self.store().job_items().defer(item.id, None).await?;
                item.state = JobItemState::Queued;
                queues.entry(site).or_default().push_front(item);
            }
        }
        Ok(())
    }

    /// Stops working a batch: what waits is ended or left as the reason
    /// says, what is in the scheduler comes back, and the job is ended or
    /// given back.
    async fn end_batch(
        &mut self,
        ending: Ending,
        mut set: JoinSet<Ended>,
        visitors: &mut HashMap<String, Vec<PersonaId>>,
        most: u32,
        waited_for: &str,
    ) -> Result<(), WorkError> {
        let _ = self.stop.send(true);
        let job = self.job.id;
        let expired = format!("expired: {waited_for}");
        let (state, reason) = match ending {
            Ending::Cancelled => (JobItemState::Cancelled, "cancelled"),
            Ending::Expired => (JobItemState::Failed, expired.as_str()),
            Ending::Shutdown | Ending::Lost => (JobItemState::Queued, ""),
        };
        if matches!(ending, Ending::Cancelled | Ending::Expired) {
            self.store()
                .job_items()
                .end_queued(job, state, Some(reason), self.now())
                .await?;
        }
        while let Some(joined) = set.join_next().await {
            let Ended { item, site, end } = joined?;
            match (ending, end) {
                // Not this runner's job any more: whoever has it finds the
                // item as it stands and takes it up.
                (Ending::Lost, _) => {}
                (_, ItemEnd::Finished(report)) => {
                    if Self::learn(visitors.entry(site).or_default(), &report, most) {
                        self.store()
                            .jobs()
                            .set_visitors(job, &json!(visitors))
                            .await?;
                    }
                    let done = if report.outcome.is_success() {
                        JobItemState::Succeeded
                    } else {
                        JobItemState::Failed
                    };
                    let error = error_of(report.outcome, report.reason.as_deref());
                    self.finish_item(item, done, Some(report.outcome), error)
                        .await?;
                }
                (_, ItemEnd::Stored { outcome, reason }) => {
                    let done = if outcome.is_success() {
                        JobItemState::Succeeded
                    } else {
                        JobItemState::Failed
                    };
                    let error = error_of(outcome, reason.as_deref());
                    self.finish_item(item, done, Some(outcome), error).await?;
                }
                (_, ItemEnd::Failed { error, outcome }) => {
                    self.finish_item(item, JobItemState::Failed, outcome, Some(error))
                        .await?;
                }
                (Ending::Shutdown, ItemEnd::Deferred { until, .. }) => {
                    self.store().job_items().defer(item.id, until).await?;
                }
                (Ending::Shutdown, ItemEnd::Stopped) => {
                    self.store().job_items().defer(item.id, None).await?;
                }
                (
                    Ending::Cancelled | Ending::Expired,
                    ItemEnd::Deferred { .. } | ItemEnd::Stopped,
                ) => {
                    self.work.abandon(&item, reason).await;
                    self.finish_item(item, state, None, Some(reason.to_owned()))
                        .await?;
                }
            }
        }
        match ending {
            Ending::Cancelled | Ending::Expired => self.finalize(None).await?,
            Ending::Shutdown => self.release(None).await?,
            Ending::Lost => {}
        }
        Ok(())
    }

    // --- a flow ----------------------------------------------------------

    /// Runs the flow of a job once: every check again under the policies
    /// in force, then one browser attempt (and its retries).
    async fn flow_unit(
        work: Work,
        spec: Value,
        request: Option<RequestId>,
        first: JobItemId,
        stop: Stop,
    ) -> (RequestId, Vec<Option<kage_scheduler::Keep>>, ItemEnd) {
        let failed = |error: &str| ItemEnd::Failed {
            error: error.to_owned(),
            outcome: None,
        };
        let id = request.unwrap_or_default();
        let Ok(JobRequest::Flow(flow)) = serde_json::from_value::<JobRequest>(spec.clone()) else {
            return (id, Vec::new(), failed(ErrorCode::ValidationFailed.as_str()));
        };
        let flow: FlowJob = *flow;
        let policies = work.state.policies().current();
        let checks = Checks {
            config: work.state.config(),
            policies: &policies,
            resolver: work.state.resolver(),
        };
        let mut planned = match plan_flow(&flow, checks).await {
            Ok(planned) => planned,
            Err(error) => return (id, Vec::new(), failed(error.code().as_str())),
        };
        let keeps = planned.keeps.clone();
        let earlier = match work.earlier(request).await {
            Ok(Ok(earlier)) => earlier,
            Ok(Err(end)) => return (id, keeps, end),
            Err(_) => {
                return (
                    id,
                    keeps,
                    ItemEnd::Deferred {
                        reason: "store".to_owned(),
                        until: None,
                    },
                );
            }
        };
        if let Err(error) = work.pin(&mut planned.planned).await {
            return (id, keeps, failed(&error));
        }
        if earlier.is_none()
            && let Err(end) = work.spend_page().await
        {
            return (id, keeps, end);
        }
        // The flow's request is written on its first open item, so that it
        // is found again when the job changes hands.
        if work
            .state
            .store()
            .job_items()
            .start(first, Some(id), work.now())
            .await
            .is_err()
        {
            return (
                id,
                keeps,
                ItemEnd::Deferred {
                    reason: "store".to_owned(),
                    until: None,
                },
            );
        }
        let order = FetchOrder {
            priority: Priority::Job,
            api_key: Some(work.key.id),
            job: Some(work.job),
            spec,
            patience: Some(PATIENCE),
            ..planned.planned.order
        };
        let end = work.fetch(id, order, earlier, stop).await;
        (id, keeps, end)
    }

    #[allow(clippy::too_many_lines)] // One flow from its stored steps to its written items.
    async fn flow(&mut self) -> Result<(), WorkError> {
        let job = self.job.id;
        loop {
            let open = self.store().job_items().open(job).await?;
            let Some(first) = open.first() else {
                self.finalize(None).await?;
                return Ok(());
            };
            if let Some(ending) = self.ending() {
                return self.end_flow(ending, open, None).await;
            }
            // Not before what it was told to wait for is over.
            let now = self.now();
            if let Some(until) = first.not_before.filter(|until| *until > now) {
                if until > later(now, HOLD) {
                    let reason = self.job.paused_reason.clone();
                    self.release(Some((until, reason.as_deref().unwrap_or("waiting"))))
                        .await?;
                    return Ok(());
                }
                self.wait_until(until).await;
                continue;
            }
            let request = open.iter().find_map(|item| item.request_id);
            let unit = Self::flow_unit(
                self.work.clone(),
                self.job.spec.clone(),
                request,
                first.id,
                self.stop.subscribe(),
            );
            tokio::pin!(unit);
            let (request, keeps, end) = loop {
                tokio::select! {
                    done = &mut unit => break done,
                    _ = self.signal.changed() => {
                        if self.ending().is_some() {
                            // What waits for a persona comes back; a flow
                            // that is under way is finished.
                            let _ = self.stop.send(true);
                        }
                    }
                }
            };
            match end {
                ItemEnd::Finished(report) => {
                    self.write_flow(open, &report, &keeps).await?;
                    self.finalize(None).await?;
                    return Ok(());
                }
                ItemEnd::Stored { outcome, reason } => {
                    // The flow's own result was stored and its steps were
                    // not written: what they did is not known any more.
                    let last = open.len().saturating_sub(1);
                    for (index, mut item) in open.into_iter().enumerate() {
                        if index == last {
                            item.request_id = Some(request);
                            let state = if outcome.is_success() {
                                JobItemState::Succeeded
                            } else {
                                JobItemState::Failed
                            };
                            let error = error_of(outcome, reason.as_deref());
                            self.finish_item(item, state, Some(outcome), error).await?;
                        } else {
                            item.request_id = None;
                            self.finish_item(
                                item,
                                JobItemState::Failed,
                                None,
                                Some("interrupted".to_owned()),
                            )
                            .await?;
                        }
                    }
                    self.finalize(None).await?;
                    return Ok(());
                }
                ItemEnd::Failed { error, outcome } => {
                    for item in open {
                        self.finish_item(item, JobItemState::Failed, outcome, Some(error.clone()))
                            .await?;
                    }
                    self.finalize(Some(&error)).await?;
                    return Ok(());
                }
                ItemEnd::Deferred { reason, until } => {
                    let until = until.unwrap_or_else(|| later(self.now(), default_wait(&reason)));
                    for item in &open {
                        self.store().job_items().defer(item.id, Some(until)).await?;
                    }
                    if until > later(self.now(), HOLD) {
                        self.release(Some((until, &reason))).await?;
                        return Ok(());
                    }
                    self.job.paused_reason = Some(reason);
                }
                ItemEnd::Stopped => {
                    let ending = self.ending().unwrap_or(Ending::Shutdown);
                    return self.end_flow(ending, open, Some(request)).await;
                }
            }
        }
    }

    /// Stops working a flow before it ran, or after it was taken out of
    /// the scheduler's queue.
    async fn end_flow(
        &mut self,
        ending: Ending,
        open: Vec<JobItem>,
        request: Option<RequestId>,
    ) -> Result<(), WorkError> {
        let waited_for = self.job.paused_reason.as_deref().unwrap_or("waiting");
        let expired = format!("expired: {waited_for}");
        let (state, reason) = match ending {
            Ending::Lost => return Ok(()),
            Ending::Shutdown => {
                for item in &open {
                    self.store().job_items().defer(item.id, None).await?;
                }
                return Ok(self.release(None).await?);
            }
            Ending::Cancelled => (JobItemState::Cancelled, "cancelled"),
            Ending::Expired => (JobItemState::Failed, expired.as_str()),
        };
        if let Some(request) = request
            && matches!(self.store().requests().get(request).await, Ok(Some(_)))
            && matches!(
                self.store().results().get_by_request(request).await,
                Ok(None)
            )
            && let Ok(JobRequest::Flow(flow)) =
                serde_json::from_value::<JobRequest>(self.job.spec.clone())
        {
            let policies = self.work.state.policies().current();
            let checks = Checks {
                config: self.work.state.config(),
                policies: &policies,
                resolver: self.work.state.resolver(),
            };
            if let Ok(planned) = plan_flow(&flow, checks).await {
                let earlier = self
                    .store()
                    .attempts()
                    .list_for_request(request)
                    .await
                    .unwrap_or_default();
                let order = FetchOrder {
                    priority: Priority::Job,
                    api_key: Some(self.work.key.id),
                    job: Some(self.job.id),
                    spec: self.job.spec.clone(),
                    ..planned.planned.order
                };
                let _ = self
                    .work
                    .engine
                    .pipeline
                    .abandon(request, order, earlier, reason)
                    .await;
            }
        }
        for item in open {
            self.finish_item(item, state, None, Some(reason.to_owned()))
                .await?;
        }
        self.finalize(None).await?;
        Ok(())
    }

    /// Writes the items of a flow that is over: its steps as they ran,
    /// each capture as a result of its own, and the page it ended on.
    async fn write_flow(
        &self,
        open: Vec<JobItem>,
        report: &FetchReport,
        keeps: &[Option<kage_scheduler::Keep>],
    ) -> Result<(), WorkError> {
        let run = report.last.as_ref().and_then(|last| last.flow.as_ref());
        for mut item in open {
            let is_last = item.input.get("final").and_then(Value::as_bool) == Some(true);
            if is_last {
                item.request_id = Some(report.request);
                let state = if report.outcome.is_success() {
                    JobItemState::Succeeded
                } else {
                    JobItemState::Failed
                };
                let error = error_of(report.outcome, report.reason.as_deref());
                self.finish_item(item, state, Some(report.outcome), error)
                    .await?;
                continue;
            }
            // The flow's own request, which its first open item carried
            // while the flow ran, is the last item's: a step names a
            // request only when it captured the page.
            item.request_id = None;
            let index = usize::try_from(item.seq).unwrap_or(usize::MAX);
            let step = run.and_then(|run| {
                run.steps
                    .iter()
                    .find(|step| usize::try_from(step.index).ok() == Some(index))
            });
            let Some(step) = step else {
                // The flow ended before this step, or never ran.
                let why = if run.is_some() {
                    "not_run".to_owned()
                } else {
                    report
                        .reason
                        .clone()
                        .unwrap_or_else(|| report.outcome.as_str().to_owned())
                };
                self.finish_item(item, JobItemState::Failed, None, Some(why))
                    .await?;
                continue;
            };
            item.output = Some(json!({
                "ok": step.ok,
                "duration_ms": step.duration_ms,
                "paced_ms": step.paced_ms,
                "error": step.error,
            }));
            let mut outcome = None;
            if let Some(page) = step.capture.as_deref() {
                let kept = self
                    .work
                    .engine
                    .pipeline
                    .keep_capture(Capture {
                        page,
                        keep: keeps.get(index).copied().flatten().unwrap_or_default(),
                        api_key: Some(self.work.key.id),
                        job: Some(self.job.id),
                        session: self.work.session.clone(),
                        persona: report.persona.clone(),
                        spec: json!({
                            "flow": report.request,
                            "step": index,
                            "action": step.action,
                            "include": item.input.get("include"),
                        }),
                    })
                    .await?;
                item.request_id = Some(kept.request);
                outcome = Some(kept.outcome);
            }
            let state = if step.ok {
                JobItemState::Succeeded
            } else {
                JobItemState::Failed
            };
            let error = step.error.clone().filter(|_| !step.ok);
            self.finish_item(item, state, outcome, error).await?;
        }
        Ok(())
    }
}

/// Works one claimed job to its end, or until it is given back.
async fn run_job(state: ApiState, engine: EngineParts, job: Job, stopping: watch::Receiver<bool>) {
    let id = job.id;
    let Ok(Some(key)) = state.store().api_keys().get(job.api_key_id).await else {
        // Without its key nothing of the job can be fetched; it is looked
        // at again later.
        let again = later(state.clock().now(), Duration::from_mins(1));
        let _ = state
            .store()
            .jobs()
            .release(id, state.jobs().worker, Some((again, "error")))
            .await;
        return;
    };
    let key = Arc::new(key);
    let session = job
        .spec
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(|session| SessionId::new(session).ok());
    let work = Work {
        state: state.clone(),
        engine,
        job: id,
        key: Arc::clone(&key),
        session,
    };
    // What the row said when it was claimed holds from the first moment:
    // a paused job is claimed for its cancel, and must not be given back
    // as paused before the first renewal of its lease has read it.
    let (signal_tx, signal) = watch::channel(if job.cancel_requested {
        Signal::Cancel
    } else {
        Signal::Run
    });
    let nudge = state.jobs().watch(id);
    let lease = tokio::spawn(keep_lease(work.clone(), signal_tx, nudge, stopping));
    let (stop, _) = watch::channel(false);
    let mut runner = Runner {
        work,
        job,
        signal,
        stop,
    };
    if key.is_active() {
        Box::pin(runner.run()).await;
    } else if let Err(error) = runner.fail_all("key_disabled").await {
        tracing::warn!(job = %id, %error, "a job of a disabled key could not be ended");
    }
    lease.abort();
    state.jobs().unwatch(id);
}

/// Runs the job worker until `shutdown` resolves: claims jobs nobody
/// works and gives each to a runner. Without a fetch engine (no proxy
/// pool) nothing is claimed.
pub async fn run_worker(state: ApiState, shutdown: impl Future<Output = ()> + Send) {
    let Some(engine) = state.engine().cloned() else {
        shutdown.await;
        return;
    };
    let config = state.config().jobs;
    let most = usize::try_from(config.max_running).unwrap_or(usize::MAX);
    let (stopping_tx, stopping) = watch::channel(false);
    let mut runners: JoinSet<()> = JoinSet::new();
    tokio::pin!(shutdown);
    tracing::info!(worker = %state.jobs().worker, "the job worker is running");
    loop {
        while runners.try_join_next().is_some() {}
        let free = most.saturating_sub(runners.len());
        if free > 0 {
            let now = state.clock().now();
            let claimed = state
                .store()
                .jobs()
                .claim(
                    state.jobs().worker,
                    crate::stored(now),
                    later(now, config.lease.get()),
                    u32::try_from(free).unwrap_or(u32::MAX),
                )
                .await;
            match claimed {
                Ok(jobs) => {
                    for job in jobs {
                        tracing::debug!(job = %job.id, kind = job.kind.as_str(), "a job is taken up");
                        runners.spawn(run_job(
                            state.clone(),
                            engine.clone(),
                            job,
                            stopping.clone(),
                        ));
                    }
                }
                Err(error) => tracing::warn!(%error, "jobs could not be claimed"),
            }
        }
        tokio::select! {
            () = &mut shutdown => break,
            () = state.jobs().work.notified() => {}
            () = tokio::time::sleep(config.poll_interval.get()) => {}
            Some(_) = runners.join_next(), if !runners.is_empty() => {}
        }
    }
    // The runners take what waits out of the scheduler, let what is under
    // way finish, and give their jobs back.
    let _ = stopping_tx.send(true);
    let wound_down = tokio::time::timeout(WIND_DOWN, async {
        while runners.join_next().await.is_some() {}
    })
    .await;
    if wound_down.is_err() {
        tracing::warn!(
            "some jobs were still being worked when the server stopped; their leases run out"
        );
    }
}

/// Carries a synchronous fetch that outlasted its wall time on as a job
/// (spec §8.4): a batch of one item under the fetch's own request, held
/// by this server while the fetch runs. Returns the job's id. When the
/// server stops meanwhile, the job is taken up again under the same
/// request after the restart.
///
/// # Errors
///
/// Fails when the job cannot be stored.
pub(crate) async fn carry_on(
    state: &ApiState,
    key: &Arc<ApiKey>,
    running: Running,
    spec: Value,
    site: String,
) -> Result<JobId, StoreError> {
    let now = crate::stored(state.clock().now());
    let config = state.config().jobs;
    let request = running.request;
    let job = Job {
        id: JobId::new(),
        api_key_id: key.id,
        kind: JobKind::Batch,
        state: JobState::Running,
        spec: json!({ "type": "batch", "item_count": 1 }),
        idempotency_key: None,
        idempotency_hash: None,
        idempotency_until: None,
        origin: JobOrigin::SyncOverflow,
        webhook_url: None,
        webhook_secret: None,
        items_total: 1,
        items_succeeded: 0,
        items_failed: 0,
        items_cancelled: 0,
        cancel_requested: false,
        error: None,
        deadline: Some(later(now, config.max_age.get())),
        paused_until: None,
        paused_reason: None,
        created_at: now,
        started_at: Some(now),
        finished_at: None,
    };
    let item = JobItem {
        id: JobItemId::new(),
        job_id: job.id,
        seq: 0,
        state: JobItemState::Running,
        input: spec,
        site: Some(site.clone()),
        request_id: Some(request),
        outcome: None,
        output: None,
        error: None,
        not_before: None,
        claims: 1,
        created_at: now,
        started_at: Some(now),
        finished_at: None,
    };
    let worker = state.jobs().worker;
    state
        .store()
        .jobs()
        .create_leased(
            &job,
            std::slice::from_ref(&item),
            now,
            worker,
            later(now, config.lease.get()),
        )
        .await?;
    state.store().requests().adopt(request, job.id).await?;

    let id = job.id;
    tokio::spawn(carried(state.clone(), Arc::clone(key), job, item, running));
    Ok(id)
}

/// Waits for a fetch that is carried on as a job and writes what it came
/// to into the job: its end, or that the job worker is to take it up
/// again.
async fn carried(state: ApiState, key: Arc<ApiKey>, job: Job, item: JobItem, running: Running) {
    let id = job.id;
    let Some(engine) = state.engine().cloned() else {
        return;
    };
    let work = Work {
        state: state.clone(),
        engine,
        job: id,
        key,
        session: None,
    };
    let (signal_tx, signal) = watch::channel(Signal::Run);
    let (_never, stopping) = watch::channel(false);
    let lease = tokio::spawn(keep_lease(
        work.clone(),
        signal_tx,
        state.jobs().watch(id),
        stopping,
    ));
    let (stop, _) = watch::channel(false);
    let runner = Runner {
        work,
        job,
        signal,
        stop,
    };
    let written = match running.wait().await {
        Ok(JobFetch::Finished(report)) => {
            let state = if report.outcome.is_success() {
                JobItemState::Succeeded
            } else {
                JobItemState::Failed
            };
            let error = error_of(report.outcome, report.reason.as_deref());
            match runner
                .finish_item(item, state, Some(report.outcome), error)
                .await
            {
                Ok(()) => runner.finalize(None).await,
                Err(error) => Err(error),
            }
        }
        // It waits, or the server stops: the job worker takes it up again
        // under its request.
        Ok(JobFetch::Deferred(deferred)) => {
            let until = deferred
                .retry_at
                .filter(|_| deferred.reason != Refusal::ShuttingDown);
            match runner.store().job_items().defer(item.id, until).await {
                Ok(_) => {
                    runner
                        .release(until.map(|until| (until, deferred.reason.as_str())))
                        .await
                }
                Err(error) => Err(error),
            }
        }
        Ok(JobFetch::Stopped) | Err(_) => {
            match runner.store().job_items().defer(item.id, None).await {
                Ok(_) => runner.release(None).await,
                Err(error) => Err(error),
            }
        }
    };
    if let Err(error) = written {
        tracing::warn!(job = %id, %error, "a fetch that is carried on as a job could not be written back; its lease runs out");
    }
    lease.abort();
    state.jobs().unwatch(id);
    state.jobs().wake();
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{TimeZone, Utc};
    use kage_core::outcome::Outcome;
    use tokio::sync::watch;

    use super::{Signal, default_wait, error_of, later, raise};

    #[test]
    fn a_signal_only_ever_rises() {
        let (signal, seen) = watch::channel(Signal::Run);

        raise(&signal, Signal::Cancel);
        assert_eq!(*seen.borrow(), Signal::Cancel);
        // A stop outranks a cancel; a cancel does not undo a stop.
        raise(&signal, Signal::Shutdown);
        raise(&signal, Signal::Cancel);
        assert_eq!(*seen.borrow(), Signal::Shutdown);
        raise(&signal, Signal::Lost);
        raise(&signal, Signal::Run);
        assert_eq!(*seen.borrow(), Signal::Lost);
    }

    #[test]
    fn an_item_says_why_it_failed_and_nothing_when_it_did_not() {
        assert_eq!(error_of(Outcome::Ok, None), None);
        assert_eq!(error_of(Outcome::ChallengePassed, Some("x")), None);
        assert_eq!(
            error_of(Outcome::Forbidden, None).as_deref(),
            Some("forbidden")
        );
        assert_eq!(
            error_of(Outcome::ProviderError, Some("no_persona")).as_deref(),
            Some("no_persona")
        );
    }

    #[test]
    fn what_was_refused_without_a_time_is_asked_again_soon() {
        assert_eq!(default_wait("egress_unsafe"), Duration::from_mins(1));
        assert_eq!(default_wait("bandwidth_exhausted"), Duration::from_mins(10));
        let noon = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        assert_eq!(
            later(noon, Duration::from_mins(10)),
            Utc.with_ymd_and_hms(2026, 10, 11, 12, 10, 0).unwrap()
        );
        // A wait nobody can add to a date changes nothing.
        assert_eq!(later(noon, Duration::MAX), noon);
    }
}
