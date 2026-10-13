//! What the job endpoints answer with (spec §16.3, §16.6).
//!
//! A job shows its key nothing of the pool: its visitors are never named,
//! an item's result is the stored result as `GET /v1/results/{id}` shows
//! it (a per-key persona handle, no proxy, no address, no seed, cookie
//! values redacted), and of its webhook only the receiver's host.

use chrono::{DateTime, Utc};
use kage_core::domain::{
    DeliveryState, Job, JobItem, JobItemState, JobKind, JobOrigin, JobState, WebhookAttempt,
    WebhookDelivery, WebhookEvent,
};
use kage_core::ids::{JobId, WebhookDeliveryId};
use serde::Serialize;
use serde_json::Value;
use url::Url;
use utoipa::ToSchema;

use super::crawl::{CrawlItemView, CrawlView};
use super::request::stored_progress;
use crate::fetch::result::{FetchResult, OutcomeField};

/// The kind of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobTypeField {
    /// Pages fetched one after another.
    Batch,
    /// A script of steps in one browser.
    Flow,
    /// A site read from its seeds along its links.
    Crawl,
}

impl From<JobKind> for JobTypeField {
    fn from(kind: JobKind) -> Self {
        match kind {
            JobKind::Batch => Self::Batch,
            JobKind::Flow => Self::Flow,
            JobKind::Crawl => Self::Crawl,
        }
    }
}

/// Where a job stands (spec §16.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum JobStateField {
    /// Accepted, not started.
    Queued,
    /// Being worked on.
    Running,
    /// Everything left of it waits: for a paused site, the bandwidth
    /// term, its visitor's break or night. `paused` says for what and
    /// until when.
    Paused,
    /// Every item succeeded.
    Succeeded,
    /// Some items succeeded and some failed.
    Partial,
    /// No item succeeded.
    Failed,
    /// Stopped at the caller's request before every item ran.
    Cancelled,
}

impl From<JobState> for JobStateField {
    fn from(state: JobState) -> Self {
        match state {
            JobState::Queued => Self::Queued,
            JobState::Running => Self::Running,
            JobState::Paused => Self::Paused,
            JobState::Succeeded => Self::Succeeded,
            JobState::Partial => Self::Partial,
            JobState::Failed => Self::Failed,
            JobState::Cancelled => Self::Cancelled,
        }
    }
}

/// Where an item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemStateField {
    /// Waiting its turn.
    Queued,
    /// Being fetched, or waiting for its persona.
    Running,
    /// Its page was fetched, or its step was done.
    Succeeded,
    /// It was not.
    Failed,
    /// It never ran because the job was cancelled.
    Cancelled,
}

impl From<JobItemState> for ItemStateField {
    fn from(state: JobItemState) -> Self {
        match state {
            JobItemState::Queued => Self::Queued,
            JobItemState::Running => Self::Running,
            JobItemState::Succeeded => Self::Succeeded,
            JobItemState::Failed => Self::Failed,
            JobItemState::Cancelled => Self::Cancelled,
        }
    }
}

/// How far a job has come.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct Counters {
    /// Items the job consists of.
    pub(crate) total: u32,
    /// Items that succeeded.
    pub(crate) succeeded: u32,
    /// Items that failed.
    pub(crate) failed: u32,
    /// Items that never ran because the job was cancelled.
    pub(crate) cancelled: u32,
    /// Items that are not over yet.
    pub(crate) pending: u32,
}

/// What a paused job waits for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct PausedView {
    /// What stands in the way: `domain_paused`, `bandwidth_exhausted`,
    /// `egress_unsafe`, `budget_exhausted`, or `no_persona` (its visitor
    /// is on a break or asleep).
    #[schema(example = "domain_paused")]
    pub(crate) reason: String,
    /// When the job looks again.
    pub(crate) until: Option<DateTime<Utc>>,
}

/// One event on its way to the job's webhook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct DeliveryView {
    /// The event's id, which the receiver sees as `X-Kage-Delivery`.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) id: WebhookDeliveryId,
    /// `job.completed`, `job.failed` or `job.progress`.
    #[serde(rename = "type")]
    #[schema(example = "job.completed")]
    pub(crate) kind: String,
    /// `pending`, `delivered`, `failed`, or `superseded` (a progress event
    /// that a later one overtook).
    #[schema(example = "delivered")]
    pub(crate) state: String,
    /// Attempts made so far.
    pub(crate) attempts: u32,
    /// When the next attempt is due, while pending.
    pub(crate) next_attempt_at: Option<DateTime<Utc>>,
    /// The status of the last attempt, when the receiver answered.
    pub(crate) last_status: Option<u16>,
    /// Why the last attempt failed when the receiver did not answer:
    /// `timeout`, `connect`, `unresolved`, `address_denied`, …
    pub(crate) last_error: Option<String>,
    /// When the event happened.
    pub(crate) created_at: DateTime<Utc>,
}

impl DeliveryView {
    pub(crate) fn of(delivery: &WebhookDelivery, last: Option<&WebhookAttempt>) -> Self {
        Self {
            id: delivery.id,
            kind: WebhookEvent::as_str(delivery.event).to_owned(),
            state: DeliveryState::as_str(delivery.state).to_owned(),
            attempts: delivery.attempts,
            next_attempt_at: delivery.next_attempt_at,
            last_status: last.and_then(|attempt| attempt.status),
            last_error: last.and_then(|attempt| attempt.error.clone()),
            created_at: delivery.created_at,
        }
    }
}

/// A job's webhook, as far as it is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct WebhookView {
    /// The receiver's host. The rest of its URL is not shown again: a
    /// path or a query often is a token.
    #[schema(example = "hooks.example.org")]
    pub(crate) host: String,
    /// `job.progress` every so many items; null for none.
    pub(crate) progress_every: Option<u32>,
    /// The newest events and how their delivery stands; empty in a
    /// webhook's own body.
    pub(crate) deliveries: Vec<DeliveryView>,
}

/// A job (spec §16.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct JobView {
    /// The job's id.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) id: JobId,
    /// What kind of job it is.
    #[serde(rename = "type")]
    pub(crate) kind: JobTypeField,
    /// Where it stands.
    pub(crate) state: JobStateField,
    /// How far it has come.
    pub(crate) items: Counters,
    /// Whether a cancel was asked for.
    pub(crate) cancel_requested: bool,
    /// What a paused job waits for.
    pub(crate) paused: Option<PausedView>,
    /// Why the job failed as a whole, when it did.
    pub(crate) error: Option<String>,
    /// `api` for a job that was submitted, `sync_overflow` for a fetch
    /// that outlasted its wall time and is carried on as a job.
    #[schema(example = "api")]
    pub(crate) origin: String,
    /// How many personas read one site for a batch or a crawl at a time.
    pub(crate) visitors: Option<u32>,
    /// How far a crawl has come; null for every other job.
    pub(crate) crawl: Option<CrawlView>,
    /// The session the job runs in, when it names one.
    pub(crate) session_id: Option<String>,
    /// The job's webhook.
    pub(crate) webhook: Option<WebhookView>,
    /// When the job was accepted.
    pub(crate) created_at: DateTime<Utc>,
    /// When work on it began.
    pub(crate) started_at: Option<DateTime<Utc>>,
    /// When it ended.
    pub(crate) finished_at: Option<DateTime<Utc>>,
    /// When it stops waiting for what holds its items: the items left then
    /// fail with what they were waiting for.
    pub(crate) deadline: Option<DateTime<Utc>>,
    /// Where its items are read, page by page.
    #[schema(example = "/v1/jobs/0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e/items")]
    pub(crate) items_url: String,
}

impl JobView {
    /// The job as its key sees it, with the deliveries given and, for a
    /// crawl, how far it has come.
    pub(crate) fn of(job: &Job, deliveries: &[DeliveryView], crawl: Option<CrawlView>) -> Self {
        let spec = |name: &str| job.spec.get(name);
        let number = |name: &str| {
            spec(name)
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
        };
        let webhook = job
            .webhook_url
            .as_deref()
            .and_then(|url| Url::parse(url).ok())
            .and_then(|url| url.host_str().map(str::to_owned))
            .map(|host| WebhookView {
                host,
                progress_every: stored_progress(&job.spec),
                deliveries: deliveries.to_vec(),
            });
        Self {
            id: job.id,
            kind: job.kind.into(),
            state: job.state.into(),
            items: Counters {
                total: job.items_total,
                succeeded: job.items_succeeded,
                failed: job.items_failed,
                cancelled: job.items_cancelled,
                pending: job.items_pending(),
            },
            cancel_requested: job.cancel_requested,
            paused: (job.state == JobState::Paused).then(|| PausedView {
                reason: job
                    .paused_reason
                    .clone()
                    .unwrap_or_else(|| "waiting".to_owned()),
                until: job.paused_until,
            }),
            error: job.error.clone(),
            origin: JobOrigin::as_str(job.origin).to_owned(),
            visitors: (job.kind != JobKind::Flow && job.origin == JobOrigin::Api)
                .then(|| number("visitors").unwrap_or(1)),
            crawl,
            session_id: spec("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            webhook,
            created_at: job.created_at,
            started_at: job.started_at,
            finished_at: job.finished_at,
            deadline: job.deadline,
            items_url: format!("/v1/jobs/{}/items", job.id),
        }
    }
}

/// What a step of a flow reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct StepView {
    /// The step's action; `final` for the page the flow ended on.
    #[schema(example = "click")]
    pub(crate) action: String,
    /// Whether the step succeeded; null while it has not run.
    pub(crate) ok: Option<bool>,
    /// How long it took, in milliseconds, the persona's own time on the
    /// page included.
    pub(crate) duration_ms: Option<u64>,
    /// How much of that was the persona's dwell and thinking time.
    pub(crate) paced_ms: Option<u64>,
    /// Why it failed.
    pub(crate) error: Option<String>,
}

/// One item of a job: a page of a batch or of a crawl, or a step of a
/// flow.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct JobItemView {
    /// Its position in the job, from 0.
    pub(crate) seq: u64,
    /// Where it stands.
    pub(crate) state: ItemStateField,
    /// The page, for an item of a batch or of a crawl.
    pub(crate) url: Option<String>,
    /// The step, for an item of a flow.
    pub(crate) step: Option<StepView>,
    /// What the page was to the crawl, for an item of a crawl: its depth,
    /// where its link was found, how it was reached, what it repeats.
    pub(crate) crawl: Option<CrawlItemView>,
    /// How its fetch or its capture ended.
    pub(crate) outcome: Option<OutcomeField>,
    /// Why it failed, or was not attempted: an outcome's name, a reason
    /// such as `domain_paused` or `cancelled`, or a step's own error.
    pub(crate) error: Option<String>,
    /// Not taken up before this moment: what it waits for is not over.
    pub(crate) not_before: Option<DateTime<Utc>>,
    /// The id its result is stored under: `GET /v1/results/{id}` returns
    /// it with the document and the body.
    #[schema(value_type = Option<String>, format = Uuid)]
    pub(crate) request_id: Option<uuid::Uuid>,
    /// The stored result, without the document and the body (they are
    /// artifacts, named in `artifacts`). Null while there is none, and
    /// when the page was asked for with `results=false`.
    pub(crate) result: Option<FetchResult>,
    /// When it was first taken up.
    pub(crate) started_at: Option<DateTime<Utc>>,
    /// When it ended.
    pub(crate) finished_at: Option<DateTime<Utc>>,
}

impl JobItemView {
    /// The item as its job's key sees it.
    pub(crate) fn of(item: &JobItem, kind: JobKind, result: Option<FetchResult>) -> Self {
        let output = |name: &str| item.output.as_ref().and_then(|output| output.get(name));
        let step = (kind == JobKind::Flow).then(|| StepView {
            action: item
                .input
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("final")
                .to_owned(),
            ok: output("ok").and_then(Value::as_bool),
            duration_ms: output("duration_ms").and_then(Value::as_u64),
            paced_ms: output("paced_ms").and_then(Value::as_u64),
            error: output("error").and_then(Value::as_str).map(str::to_owned),
        });
        Self {
            seq: item.seq,
            state: item.state.into(),
            url: (kind != JobKind::Flow)
                .then(|| item.input.get("url").and_then(Value::as_str))
                .flatten()
                .map(str::to_owned),
            step,
            crawl: (kind == JobKind::Crawl)
                .then(|| CrawlItemView::of(item.output.as_ref()))
                .flatten(),
            outcome: item.outcome.map(Into::into),
            error: item.error.clone(),
            not_before: item.not_before,
            request_id: item.request_id.map(|id| id.as_uuid()),
            result,
            started_at: item.started_at,
            finished_at: item.finished_at,
        }
    }
}

/// A page of a job's items.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct JobItemsPage {
    /// The items, in the job's order.
    pub(crate) items: Vec<JobItemView>,
    /// What to pass as `cursor` for the next page; null on the last.
    pub(crate) next_cursor: Option<String>,
}

/// What a purge deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct Purged {
    /// Fetches that were not part of a job, with their results.
    pub(crate) requests: u64,
    /// Finished jobs, with their items and their items' results.
    pub(crate) jobs: u64,
    /// Bodies, documents, screenshots and PDFs whose rows were deleted.
    /// Their files go with the artifact store's next sweep.
    pub(crate) artifacts: u64,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use kage_core::domain::{
        DeliveryState, Job, JobItem, JobItemState, JobKind, JobOrigin, JobState, WebhookAttempt,
        WebhookDelivery, WebhookEvent,
    };
    use kage_core::ids::{ApiKeyId, JobId, JobItemId, RequestId, WebhookDeliveryId};
    use kage_core::outcome::Outcome;
    use kage_core::secret::SealedSecret;
    use serde_json::json;

    use super::{DeliveryView, ItemStateField, JobItemView, JobStateField, JobView};

    fn job() -> Job {
        let at = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        Job {
            id: JobId::new(),
            api_key_id: ApiKeyId::new(),
            kind: JobKind::Batch,
            state: JobState::Paused,
            spec: json!({ "type": "batch", "visitors": 2, "progress_every": 10, "session_id": "s-1" }),
            idempotency_key: Some("order-17".to_owned()),
            idempotency_hash: Some([1; 32]),
            idempotency_until: Some(at),
            origin: JobOrigin::Api,
            webhook_url: Some("https://hooks.example.org/kage/T0P-S3CR3T?token=abc".to_owned()),
            webhook_secret: Some(SealedSecret::new(vec![1, 2, 3])),
            items_total: 50,
            items_succeeded: 20,
            items_failed: 2,
            items_cancelled: 0,
            cancel_requested: false,
            error: None,
            deadline: Some(at),
            paused_until: Some(at),
            paused_reason: Some("domain_paused".to_owned()),
            created_at: at,
            started_at: Some(at),
            finished_at: None,
        }
    }

    #[test]
    fn a_job_shows_its_progress_and_of_its_webhook_only_the_host() {
        let job = job();
        let delivery = WebhookDelivery {
            id: WebhookDeliveryId::new(),
            job_id: job.id,
            event: WebhookEvent::JobProgress,
            origin: "https://hooks.example.org:443".to_owned(),
            body: "{}".to_owned(),
            state: DeliveryState::Pending,
            attempts: 1,
            next_attempt_at: job.deadline,
            created_at: job.created_at,
            finished_at: None,
        };
        let failed = WebhookAttempt {
            delivery_id: delivery.id,
            n: 1,
            started_at: job.created_at,
            duration_ms: 10_000,
            status: None,
            error: Some("timeout".to_owned()),
        };

        let view = JobView::of(&job, &[DeliveryView::of(&delivery, Some(&failed))], None);
        let text = serde_json::to_string(&view).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(value["type"], "batch");
        assert_eq!(value["state"], "paused");
        assert_eq!(
            value["items"],
            json!({ "total": 50, "succeeded": 20, "failed": 2, "cancelled": 0, "pending": 28 })
        );
        assert_eq!(value["paused"]["reason"], "domain_paused");
        assert_eq!(value["visitors"], 2);
        assert_eq!(value["session_id"], "s-1");
        assert_eq!(value["origin"], "api");
        assert_eq!(value["items_url"], format!("/v1/jobs/{}/items", job.id));
        // Of the webhook: the host, and how its deliveries stand.
        assert_eq!(value["webhook"]["host"], "hooks.example.org");
        assert_eq!(value["webhook"]["progress_every"], 10);
        assert_eq!(value["webhook"]["deliveries"][0]["type"], "job.progress");
        assert_eq!(value["webhook"]["deliveries"][0]["last_error"], "timeout");
        // Never its path, its query, its secret, or the idempotency key.
        for hidden in [
            "T0P-S3CR3T",
            "token=abc",
            "order-17",
            "secret",
            "idempotency",
        ] {
            assert!(!text.contains(hidden), "{hidden} in {text}");
        }
        assert_eq!(view.state, JobStateField::Paused);

        // A job that runs is not paused, whatever its row still says; one
        // without a webhook shows none; a carried-on fetch has no visitors.
        let mut running = job;
        running.state = JobState::Running;
        running.webhook_url = None;
        running.origin = JobOrigin::SyncOverflow;
        let view = JobView::of(&running, &[], None);
        assert_eq!(view.paused, None);
        assert_eq!(view.webhook, None);
        assert_eq!(view.visitors, None);
        assert_eq!(view.origin, "sync_overflow");
    }

    #[test]
    fn an_item_is_a_page_of_a_batch_or_a_step_of_a_flow() {
        let at = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        let request = RequestId::new();
        let page = JobItem {
            id: JobItemId::new(),
            job_id: JobId::new(),
            seq: 3,
            state: JobItemState::Failed,
            input: json!({ "url": "https://shop.example/3", "include": ["dom"] }),
            site: Some("shop.example".to_owned()),
            request_id: Some(request),
            outcome: Some(Outcome::Forbidden),
            output: None,
            error: Some("forbidden".to_owned()),
            not_before: None,
            claims: 1,
            created_at: at,
            started_at: Some(at),
            finished_at: Some(at),
        };

        let view = JobItemView::of(&page, JobKind::Batch, None);
        let value = serde_json::to_value(&view).unwrap();
        assert_eq!(value["seq"], 3);
        assert_eq!(value["state"], "failed");
        assert_eq!(value["url"], "https://shop.example/3");
        assert_eq!(value["outcome"], "forbidden");
        assert_eq!(value["request_id"], request.to_string());
        assert!(value["step"].is_null() && value["result"].is_null());
        assert_eq!(view.state, ItemStateField::Failed);

        let step = JobItem {
            input: json!({ "action": "click", "selector": "#next" }),
            site: None,
            state: JobItemState::Succeeded,
            outcome: None,
            output: Some(
                json!({ "ok": true, "duration_ms": 4200, "paced_ms": 2900, "error": null }),
            ),
            error: None,
            request_id: None,
            ..page.clone()
        };
        let value = serde_json::to_value(JobItemView::of(&step, JobKind::Flow, None)).unwrap();
        assert!(value["url"].is_null());
        assert_eq!(
            value["step"],
            json!({ "action": "click", "ok": true, "duration_ms": 4200, "paced_ms": 2900, "error": null })
        );
        // The last item of a flow is the page it ended on; a step that has
        // not run yet says nothing.
        let last = JobItem {
            input: json!({ "final": true }),
            output: None,
            state: JobItemState::Queued,
            ..step
        };
        let value = serde_json::to_value(JobItemView::of(&last, JobKind::Flow, None)).unwrap();
        assert_eq!(value["step"]["action"], "final");
        assert!(value["step"]["ok"].is_null());
    }
}
