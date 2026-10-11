//! Jobs, job items and the crawl frontier (spec §13, §16.6, §18).

use chrono::{DateTime, Utc};

use crate::ids::{ApiKeyId, FrontierId, JobId, JobItemId, RequestId, WebhookDeliveryId};
use crate::macros::string_enum;
use crate::outcome::Outcome;
use crate::secret::SealedSecret;

string_enum! {
    /// The kind of asynchronous unit (spec §2).
    pub enum JobKind {
        /// A list of URLs fetched independently.
        Batch => "batch",
        /// A scripted navigation flow.
        Flow => "flow",
        /// A frontier-based crawl.
        Crawl => "crawl",
    }
}

string_enum! {
    /// Job lifecycle (spec §16.6).
    pub enum JobState {
        /// Accepted, not started.
        Queued => "queued",
        /// Being worked on.
        Running => "running",
        /// Held, for example while its domain's breaker is open.
        Paused => "paused",
        /// Every item succeeded.
        Succeeded => "succeeded",
        /// Finished with some failed items.
        Partial => "partial",
        /// Finished with no successful item, or failed as a whole.
        Failed => "failed",
        /// Stopped at the caller's request.
        Cancelled => "cancelled",
    }
}

impl JobState {
    /// Whether the job will never run again.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Partial | Self::Failed | Self::Cancelled
        )
    }
}

string_enum! {
    /// Where a job came from.
    pub enum JobOrigin {
        /// `POST /v1/jobs`.
        Api => "api",
        /// A synchronous fetch that outlasted its wall time (spec §8.4).
        SyncOverflow => "sync_overflow",
    }
}

/// An asynchronous unit of work.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// Identity.
    pub id: JobId,
    /// The key that created the job.
    pub api_key_id: ApiKeyId,
    /// What kind of job it is.
    pub kind: JobKind,
    /// Lifecycle state.
    pub state: JobState,
    /// The job definition as accepted.
    pub spec: serde_json::Value,
    /// `Idempotency-Key` of the creating request, unique per key.
    pub idempotency_key: Option<String>,
    /// A keyed hash of the request that came with the idempotency key: the
    /// same key with another request is a mistake, not a repeat.
    pub idempotency_hash: Option<[u8; 32]>,
    /// Until when the idempotency key names this job.
    pub idempotency_until: Option<DateTime<Utc>>,
    /// Where the job came from.
    pub origin: JobOrigin,
    /// Where to deliver webhook events.
    pub webhook_url: Option<String>,
    /// HMAC secret for webhook signatures, encrypted at rest.
    pub webhook_secret: Option<SealedSecret>,
    /// Items the job consists of.
    pub items_total: u32,
    /// Items that finished successfully.
    pub items_succeeded: u32,
    /// Items that finished unsuccessfully.
    pub items_failed: u32,
    /// Items that never ran because the job was cancelled.
    pub items_cancelled: u32,
    /// Whether a cooperative cancel was requested.
    pub cancel_requested: bool,
    /// Why the job failed as a whole, if it did.
    pub error: Option<String>,
    /// When the job stops waiting: the items left then fail with what they
    /// were waiting for.
    pub deadline: Option<DateTime<Utc>>,
    /// While the job is paused: when its waiting items may be tried again.
    pub paused_until: Option<DateTime<Utc>>,
    /// While the job is paused: what its items wait for.
    pub paused_reason: Option<String>,
    /// When the job was accepted.
    pub created_at: DateTime<Utc>,
    /// When work started.
    pub started_at: Option<DateTime<Utc>>,
    /// When the job reached a terminal state.
    pub finished_at: Option<DateTime<Utc>>,
}

impl Job {
    /// Items that are over, however they ended.
    pub const fn items_finished(&self) -> u32 {
        self.items_succeeded
            .saturating_add(self.items_failed)
            .saturating_add(self.items_cancelled)
    }

    /// Items that have not finished yet.
    pub const fn items_pending(&self) -> u32 {
        self.items_total.saturating_sub(self.items_finished())
    }

    /// The state a job ends in when nothing of it is left to do (spec
    /// §16.6): `cancelled` when a cancel ended it before every item ran,
    /// `succeeded` when every item succeeded, `failed` when none did,
    /// `partial` otherwise.
    pub const fn final_state(&self) -> JobState {
        if self.items_cancelled > 0 || (self.cancel_requested && self.items_pending() > 0) {
            JobState::Cancelled
        } else if self.items_failed == 0 {
            JobState::Succeeded
        } else if self.items_succeeded == 0 {
            JobState::Failed
        } else {
            JobState::Partial
        }
    }
}

string_enum! {
    /// Lifecycle of one job item.
    pub enum JobItemState {
        /// Waiting for a worker.
        Queued => "queued",
        /// Claimed by a worker.
        Running => "running",
        /// Finished with a usable result.
        Succeeded => "succeeded",
        /// Finished without one.
        Failed => "failed",
        /// Never ran because the job was cancelled.
        Cancelled => "cancelled",
    }
}

impl JobItemState {
    /// Whether the item will never run again.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// One unit inside a job: a URL of a batch, a flow, or a crawled page.
#[derive(Debug, Clone, PartialEq)]
pub struct JobItem {
    /// Identity.
    pub id: JobItemId,
    /// The job it belongs to.
    pub job_id: JobId,
    /// Position in the job; results are paginated in this order.
    pub seq: u64,
    /// Lifecycle state.
    pub state: JobItemState,
    /// What to do, in the shape the job kind defines.
    pub input: serde_json::Value,
    /// The site the item belongs to: a batch works one site's items in
    /// order, with as many visitors as it may have there.
    pub site: Option<String>,
    /// The fetch request that carried the item out.
    pub request_id: Option<RequestId>,
    /// How its fetch ended, when it had one.
    pub outcome: Option<Outcome>,
    /// What the item reports besides its result: a flow step's action,
    /// duration and error.
    pub output: Option<serde_json::Value>,
    /// Why the item failed, if it did.
    pub error: Option<String>,
    /// Not to be taken up before this moment: what it waits for is not
    /// over (a paused site, its visitor's break).
    pub not_before: Option<DateTime<Utc>>,
    /// How often a worker took it up.
    pub claims: u32,
    /// When the item was created.
    pub created_at: DateTime<Utc>,
    /// When a worker claimed it.
    pub started_at: Option<DateTime<Utc>>,
    /// When it finished.
    pub finished_at: Option<DateTime<Utc>>,
}

string_enum! {
    /// What a webhook reports (spec §16.6).
    pub enum WebhookEvent {
        /// The job ended with at least one item done: `succeeded`,
        /// `partial`, or `cancelled`.
        JobCompleted => "job.completed",
        /// The job ended with no item done.
        JobFailed => "job.failed",
        /// So many more items are over.
        JobProgress => "job.progress",
    }
}

string_enum! {
    /// Where the delivery of one event stands.
    pub enum DeliveryState {
        /// Not delivered yet; another attempt is due.
        Pending => "pending",
        /// The receiver answered `2xx`.
        Delivered => "delivered",
        /// Every attempt failed, or the receiver said it is gone.
        Failed => "failed",
        /// A later event of the same job made this one pointless.
        Superseded => "superseded",
    }
}

impl DeliveryState {
    /// Whether nothing more will be sent.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Pending)
    }
}

/// One event of a job on its way to the job's webhook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookDelivery {
    /// Identity; the receiver sees it as `X-Kage-Delivery` and as the
    /// event's `id`, the same on every attempt.
    pub id: WebhookDeliveryId,
    /// The job the event is about.
    pub job_id: JobId,
    /// What happened.
    pub event: WebhookEvent,
    /// The receiver's origin, `scheme://host:port`: one delivery is in
    /// flight per origin.
    pub origin: String,
    /// The body exactly as it is sent and signed.
    pub body: String,
    /// Where the delivery stands.
    pub state: DeliveryState,
    /// Attempts made.
    pub attempts: u32,
    /// When the next attempt is due, while pending.
    pub next_attempt_at: Option<DateTime<Utc>>,
    /// When the event happened.
    pub created_at: DateTime<Utc>,
    /// When the delivery reached a terminal state.
    pub finished_at: Option<DateTime<Utc>>,
}

/// One attempt at delivering an event: a line of the delivery log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookAttempt {
    /// The delivery.
    pub delivery_id: WebhookDeliveryId,
    /// 1 for the first attempt.
    pub n: u32,
    /// When it was made.
    pub started_at: DateTime<Utc>,
    /// How long it took, in milliseconds.
    pub duration_ms: u32,
    /// The status the receiver answered with, when it answered.
    pub status: Option<u16>,
    /// Why it failed when the receiver did not answer; never an address.
    pub error: Option<String>,
}

string_enum! {
    /// Lifecycle of a frontier entry.
    pub enum FrontierState {
        /// Discovered, waiting to be fetched.
        Queued => "queued",
        /// Claimed by a worker.
        Fetching => "fetching",
        /// Fetched.
        Done => "done",
        /// Could not be fetched.
        Failed => "failed",
        /// Left out by allow/deny rules, depth or page caps.
        Skipped => "skipped",
    }
}

/// One URL discovered by a crawl.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierEntry {
    /// Identity.
    pub id: FrontierId,
    /// The crawl job.
    pub job_id: JobId,
    /// The URL exactly as discovered; this is what gets requested.
    pub url: String,
    /// Its normalized form; a crawl holds one entry per key (spec §13).
    pub url_key: String,
    /// Link distance from a seed.
    pub depth: u32,
    /// Lifecycle state.
    pub state: FrontierState,
    /// The page the URL was found on.
    pub discovered_from: Option<String>,
    /// Hash of the fetched content, for exact-duplicate detection.
    pub content_hash: Option<String>,
    /// SimHash of the fetched content, for near-duplicate detection.
    pub simhash: Option<u64>,
    /// Whether the content nearly duplicates an earlier page of the crawl.
    pub near_duplicate: bool,
    /// When the URL was discovered.
    pub created_at: DateTime<Utc>,
    /// When it was fetched.
    pub fetched_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{
        DeliveryState, FrontierState, Job, JobItemState, JobKind, JobOrigin, JobState, WebhookEvent,
    };
    use crate::ids::{ApiKeyId, JobId};
    use crate::macros::test_support::assert_string_enum;

    #[test]
    fn enum_names_are_the_spec_names() {
        assert_string_enum(JobKind::ALL, JobKind::as_str, &["batch", "flow", "crawl"]);
        assert_string_enum(
            JobState::ALL,
            JobState::as_str,
            &[
                "queued",
                "running",
                "paused",
                "succeeded",
                "partial",
                "failed",
                "cancelled",
            ],
        );
        assert_string_enum(
            JobItemState::ALL,
            JobItemState::as_str,
            &["queued", "running", "succeeded", "failed", "cancelled"],
        );
        assert_string_enum(
            FrontierState::ALL,
            FrontierState::as_str,
            &["queued", "fetching", "done", "failed", "skipped"],
        );
        assert_string_enum(JobOrigin::ALL, JobOrigin::as_str, &["api", "sync_overflow"]);
        assert_string_enum(
            WebhookEvent::ALL,
            WebhookEvent::as_str,
            &["job.completed", "job.failed", "job.progress"],
        );
        assert_string_enum(
            DeliveryState::ALL,
            DeliveryState::as_str,
            &["pending", "delivered", "failed", "superseded"],
        );
        assert!(!DeliveryState::Pending.is_terminal());
        assert!(DeliveryState::Superseded.is_terminal());
    }

    #[test]
    fn terminal_states() {
        let terminal: Vec<JobState> = JobState::ALL
            .iter()
            .copied()
            .filter(|state| state.is_terminal())
            .collect();
        assert_eq!(
            terminal,
            [
                JobState::Succeeded,
                JobState::Partial,
                JobState::Failed,
                JobState::Cancelled
            ]
        );

        let terminal: Vec<JobItemState> = JobItemState::ALL
            .iter()
            .copied()
            .filter(|state| state.is_terminal())
            .collect();
        assert_eq!(
            terminal,
            [
                JobItemState::Succeeded,
                JobItemState::Failed,
                JobItemState::Cancelled
            ]
        );
    }

    fn job() -> Job {
        Job {
            id: JobId::new(),
            api_key_id: ApiKeyId::new(),
            kind: JobKind::Batch,
            state: JobState::Running,
            spec: serde_json::json!({}),
            idempotency_key: None,
            idempotency_hash: None,
            idempotency_until: None,
            origin: JobOrigin::Api,
            webhook_url: None,
            webhook_secret: None,
            items_total: 50,
            items_succeeded: 30,
            items_failed: 5,
            items_cancelled: 0,
            cancel_requested: false,
            error: None,
            deadline: None,
            paused_until: None,
            paused_reason: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        }
    }

    #[test]
    fn pending_items_never_underflow() {
        let mut job = job();
        assert_eq!(job.items_finished(), 35);
        assert_eq!(job.items_pending(), 15);

        job.items_cancelled = 10;
        assert_eq!(job.items_pending(), 5);

        job.items_failed = 40;
        assert_eq!(job.items_pending(), 0);
    }

    #[test]
    fn a_job_ends_as_its_items_did() {
        let ended = |succeeded, failed, cancelled, cancel_requested| {
            let mut job = job();
            job.items_total = 10;
            job.items_succeeded = succeeded;
            job.items_failed = failed;
            job.items_cancelled = cancelled;
            job.cancel_requested = cancel_requested;
            job.final_state()
        };

        assert_eq!(ended(10, 0, 0, false), JobState::Succeeded);
        assert_eq!(ended(7, 3, 0, false), JobState::Partial);
        assert_eq!(ended(0, 10, 0, false), JobState::Failed);
        // A cancel that found items still to do ends the job cancelled.
        assert_eq!(ended(4, 1, 5, true), JobState::Cancelled);
        assert_eq!(ended(4, 1, 0, true), JobState::Cancelled);
        // One that came when everything was done changes nothing.
        assert_eq!(ended(10, 0, 0, true), JobState::Succeeded);
        assert_eq!(ended(0, 10, 0, true), JobState::Failed);
    }
}
