//! Jobs, job items and the crawl frontier (spec §13, §16.6, §18).

use chrono::{DateTime, Utc};

use crate::ids::{ApiKeyId, FrontierId, JobId, JobItemId, RequestId};
use crate::macros::string_enum;
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
    /// Whether a cooperative cancel was requested.
    pub cancel_requested: bool,
    /// Why the job failed as a whole, if it did.
    pub error: Option<String>,
    /// When the job was accepted.
    pub created_at: DateTime<Utc>,
    /// When work started.
    pub started_at: Option<DateTime<Utc>>,
    /// When the job reached a terminal state.
    pub finished_at: Option<DateTime<Utc>>,
}

impl Job {
    /// Items that have not finished yet.
    pub const fn items_pending(&self) -> u32 {
        self.items_total
            .saturating_sub(self.items_succeeded)
            .saturating_sub(self.items_failed)
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
    /// The fetch request that carried the item out.
    pub request_id: Option<RequestId>,
    /// Why the item failed, if it did.
    pub error: Option<String>,
    /// When the item was created.
    pub created_at: DateTime<Utc>,
    /// When a worker claimed it.
    pub started_at: Option<DateTime<Utc>>,
    /// When it finished.
    pub finished_at: Option<DateTime<Utc>>,
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

    use super::{FrontierState, Job, JobItemState, JobKind, JobState};
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

    #[test]
    fn pending_items_never_underflow() {
        let mut job = Job {
            id: JobId::new(),
            api_key_id: ApiKeyId::new(),
            kind: JobKind::Batch,
            state: JobState::Running,
            spec: serde_json::json!({}),
            idempotency_key: None,
            webhook_url: None,
            webhook_secret: None,
            items_total: 50,
            items_succeeded: 30,
            items_failed: 5,
            cancel_requested: false,
            error: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        };
        assert_eq!(job.items_pending(), 15);

        job.items_failed = 40;
        assert_eq!(job.items_pending(), 0);
    }
}
