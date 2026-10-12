//! Jobs (spec §16.3, §16.6): work that is accepted now and done in the
//! background.
//!
//! | Path | |
//! |---|---|
//! | `POST /v1/jobs` | accept a `batch` or a `flow` |
//! | `GET /v1/jobs/{id}` | its state, its counters, its webhook's deliveries |
//! | `GET /v1/jobs/{id}/items` | its items with their results, page by page |
//! | `POST /v1/jobs/{id}/cancel` | stop it |
//!
//! A job is accepted whole or refused whole: every item passes the checks
//! of a synchronous fetch, every page a flow is sent to passes the SSRF
//! guard, and the webhook's receiver is one the operator listed, before
//! anything is stored. An `Idempotency-Key` names the job it made for a
//! day: the same request again is answered with that job, another request
//! under the same key is a mistake (`idempotency_key_reused`).
//!
//! A job belongs to the key that made it; another key's job is not there.
//!
//! - [`request`]: what a job may be, and its checks.
//! - [`worker`]: what claims jobs and works them.
//! - [`webhook`]: the events of a job, signed and delivered.
//! - [`view`]: what the endpoints answer with.

// What `utoipa::path` writes for `params(SomeQuery)` names the type through
// its trait, with the span of the name given here.
#![allow(unused_qualifications)]

pub(crate) mod crawl;
pub(crate) mod request;
pub(crate) mod view;
pub mod webhook;
pub(crate) mod worker;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{HeaderMap, HeaderName, HeaderValue, LOCATION};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use kage_core::domain::{
    Job, JobItem, JobItemState, JobKind, JobOrigin, JobState, Scope, SessionId,
};
use kage_core::ids::{JobId, JobItemId};
use kage_store::repo::Created;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;
use utoipa::IntoParams;

use self::crawl::plan_crawl;
use self::request::{
    Checks, JobRequest, MAX_IDEMPOTENCY_KEY, PlannedWebhook, canonical, plan_batch, plan_flow,
    plan_webhook, stored_spec,
};
use self::view::{DeliveryView, JobItemView, JobItemsPage, JobView};
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult, ErrorCode, Problem};
use crate::extract::{Body, Params};
use crate::fetch::request::{FetchRequest, Included, Planned};
use crate::fetch::result::{Bodies, Viewer, from_stored};
use crate::fetch::{conflict_of, pinned};
use crate::state::ApiState;

/// The header that names a job for a day.
const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");

/// The header that says an answer is the job an earlier request made.
const IDEMPOTENCY_REPLAYED: HeaderName = HeaderName::from_static("idempotency-replayed");

/// Most items one page of a job's items holds.
const MAX_ITEMS_PAGE: u32 = 200;

/// Items a page holds when the request does not say.
const DEFAULT_ITEMS_PAGE: u32 = 50;

/// Deliveries of its webhook a job shows.
const SHOWN_DELIVERIES: u32 = 20;

fn job_not_found() -> ApiError {
    ApiError::new(
        ErrorCode::JobNotFound,
        "there is no such job among those this key made; it may have been deleted after its \
         retention",
    )
}

/// The job with this id, when the caller may read it.
async fn owned_job(state: &ApiState, caller: &Caller, id: &str) -> ApiResult<Job> {
    let id = id.parse::<JobId>().map_err(|_| job_not_found())?;
    let job = state
        .store()
        .jobs()
        .get(id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(job_not_found)?;
    // Another key's job is not there, as far as this key can tell.
    if job.api_key_id == caller.key.id || caller.has(Scope::Admin) {
        Ok(job)
    } else {
        Err(job_not_found())
    }
}

/// The job as its key sees it, with the newest deliveries of its webhook.
async fn view_of(state: &ApiState, job: &Job) -> ApiResult<JobView> {
    let mut deliveries = Vec::new();
    if job.webhook_url.is_some() {
        let hooks = state.store().webhooks();
        let listed = hooks
            .list_for_job(job.id, SHOWN_DELIVERIES)
            .await
            .map_err(|error| ApiError::internal(&error))?;
        for delivery in listed {
            let attempts = hooks
                .attempts(delivery.id)
                .await
                .map_err(|error| ApiError::internal(&error))?;
            deliveries.push(DeliveryView::of(&delivery, attempts.last()));
        }
    }
    let crawl = if job.kind == JobKind::Crawl {
        let kept = state
            .store()
            .jobs()
            .crawl(job.id)
            .await
            .map_err(|error| ApiError::internal(&error))?;
        Some(crawl::Kept::view(kept).unwrap_or_default())
    } else {
        None
    };
    Ok(JobView::of(job, &deliveries, crawl))
}

/// The `Idempotency-Key` of a request, when it carries one that can be
/// one.
fn idempotency_key(headers: &HeaderMap) -> ApiResult<Option<String>> {
    let Some(value) = headers.get(&IDEMPOTENCY_KEY) else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|key| {
            !key.is_empty()
                && key.len() <= MAX_IDEMPOTENCY_KEY
                && key.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::ValidationFailed,
                format!(
                    "Idempotency-Key is 1 to {MAX_IDEMPOTENCY_KEY} visible ASCII characters \
                     without spaces"
                ),
            )
            .with("header", "Idempotency-Key")
        })?;
    Ok(Some(key.to_owned()))
}

/// A keyed fingerprint of a request's body, whatever the order of its
/// members: what an idempotency key is compared by. Keyed, because the
/// body holds the webhook's secret.
fn fingerprint(key: &[u8; 32], body: &Value) -> [u8; 32] {
    let mut hash = [0_u8; 32];
    if let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) {
        mac.update(canonical(body).to_string().as_bytes());
        hash.copy_from_slice(&mac.finalize().into_bytes());
    }
    hash
}

/// What a job that was accepted before is answered with when its key
/// comes again.
async fn replayed(state: &ApiState, existing: &Job, hash: [u8; 32]) -> ApiResult<Response> {
    if existing.idempotency_hash != Some(hash) {
        return Err(ApiError::new(
            ErrorCode::IdempotencyKeyReused,
            "this Idempotency-Key made another job within the last day; the same key names the \
             same request. Send the request it was first used with, or another key",
        )
        .with("job_id", existing.id.to_string()));
    }
    let view = view_of(state, existing).await?;
    let mut response = (StatusCode::OK, Json(view)).into_response();
    response
        .headers_mut()
        .insert(IDEMPOTENCY_REPLAYED, HeaderValue::from_static("true"));
    Ok(response)
}

/// The session a job names must be open, and its persona one every item
/// may use: checked when the job is accepted, as a fetch's is.
async fn check_session(
    state: &ApiState,
    caller: &Caller,
    session: Option<&SessionId>,
    planned: &[&Planned],
) -> ApiResult<()> {
    let Some(session) = session else {
        return Ok(());
    };
    let (Some(engine), Some(first)) = (state.engine(), planned.first()) else {
        return Ok(());
    };
    // The same rules as a fetch: the scope, 404 when it is not open, 409
    // when its persona is not one the item may use. The session is looked
    // up once; every other item is held against its persona.
    let persona = pinned(state, engine, caller, session, first).await?;
    let policies = state.policies().current();
    for (index, one) in planned.iter().enumerate().skip(1) {
        let host = one.order.url.host_str().unwrap_or_default();
        let policy = policies.domains.resolve(host);
        if let Some(constraint) = conflict_of(&persona, one, &policy) {
            return Err(ApiError::new(
                ErrorCode::SessionPersonaConflict,
                format!(
                    "the session's persona does not satisfy {constraint} for item {index}; \
                     nothing was accepted. A session never moves to another persona"
                ),
            )
            .with("session_id", session.as_str())
            .with("constraint", constraint)
            .with("item", u64::try_from(index).unwrap_or(u64::MAX)));
        }
    }
    Ok(())
}

/// Accept a job.
///
/// A `batch` (pages fetched one after another), a `flow` (a script of
/// steps in one browser) or a `crawl` (a site read from its seeds along
/// its links; it needs the `crawl` scope besides `jobs`).
///
/// `201` with the job, which then runs in the background: read it with
/// `GET /v1/jobs/{id}` and its items with `GET /v1/jobs/{id}/items`, or
/// name a `webhook` to be told. With an `Idempotency-Key` header the same
/// request within a day is answered `200` with the job it made the first
/// time (`Idempotency-Replayed: true`), and another request under that key
/// is `409 idempotency_key_reused`. Nothing is stored for a job that is
/// refused.
#[utoipa::path(
    post,
    path = "/v1/jobs",
    tag = "jobs",
    operation_id = "create_job",
    request_body = JobRequest,
    params(("Idempotency-Key" = Option<String>, Header, description = "Names the job this request makes for a day: 1 to 255 visible ASCII characters.")),
    responses(
        (status = 201, description = "The job was accepted.", body = JobView,
            headers(("Location" = String, description = "Where the job is read."))),
        (status = 200, description = "The job this Idempotency-Key made before.", body = JobView,
            headers(("Idempotency-Replayed" = String, description = "Always `true`."))),
        (status = 400, description = "The body is not JSON.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No key, or not a valid one.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The key lacks the `jobs` scope, `crawl` for a crawl, or `sessions` for `session_id`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`session_not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`idempotency_key_reused`, `session_persona_conflict`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The job cannot be accepted as asked; `code` says why, `item` (a batch's item, a crawl's seed) or `step` which part.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "A limit of the key or of the API.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Nothing can be fetched: no proxy pool, or the server stops.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["jobs"]))
)]
// One job, checked top to bottom and stored at the end.
#[allow(clippy::too_many_lines)]
pub(crate) async fn create(
    State(state): State<ApiState>,
    caller: Caller,
    headers: HeaderMap,
    Body(body): Body<Value>,
) -> ApiResult<Response> {
    caller.require(Scope::Jobs)?;
    if body.get("type").and_then(Value::as_str) == Some("crawl") {
        // A crawl asks a site for pages nobody named: a scope of its own.
        caller.require(Scope::Crawl)?;
    }
    if state.engine().is_none() || state.is_stopping() {
        return Err(ApiError::new(
            ErrorCode::Unavailable,
            "no proxy pool is configured, or the server is stopping: no job is accepted",
        ));
    }
    let request: JobRequest = serde_json::from_value(body.clone())
        .map_err(|error| ApiError::new(ErrorCode::ValidationFailed, error.to_string()))?;

    // The same key again is answered before anything is checked twice.
    let key = idempotency_key(&headers)?;
    let hash = fingerprint(&state.jobs().idempotency_key, &body);
    let now = crate::stored(state.clock().now());
    if let Some(key) = &key {
        let existing = state
            .store()
            .jobs()
            .find_by_idempotency_key(caller.key.id, key)
            .await
            .map_err(|error| ApiError::internal(&error))?
            .filter(|job| job.idempotency_until.is_none_or(|until| until > now));
        if let Some(existing) = existing {
            return replayed(&state, &existing, hash).await;
        }
    }

    let policies = state.policies().current();
    let checks = Checks {
        config: state.config(),
        policies: &policies,
        resolver: state.resolver(),
    };
    let webhook: Option<PlannedWebhook> = match request.webhook() {
        Some(webhook) => Some(plan_webhook(webhook, checks).await?),
        None => None,
    };
    let session = request
        .session_id()
        .map(SessionId::new)
        .transpose()
        .map_err(|error| {
            ApiError::new(ErrorCode::InvalidSessionId, error.to_string())
                .with("field", "session_id")
        })?;
    let id = JobId::new();
    let item = |seq: u64, input: Value, site: Option<String>| JobItem {
        id: JobItemId::new(),
        job_id: id,
        seq,
        state: JobItemState::Queued,
        input,
        site,
        request_id: None,
        outcome: None,
        output: None,
        error: None,
        not_before: None,
        claims: 0,
        created_at: now,
        started_at: None,
        finished_at: None,
    };
    let mut crawl_start = None;
    let (kind, items) = match &request {
        JobRequest::Batch(batch) => {
            let planned = plan_batch(batch, checks).await?;
            let orders: Vec<&Planned> = planned.iter().map(|item| &item.planned).collect();
            check_session(&state, &caller, session.as_ref(), &orders).await?;
            let items = planned
                .into_iter()
                .zip(0_u64..)
                .map(|(planned, seq)| item(seq, planned.input, Some(planned.site)))
                .collect::<Vec<JobItem>>();
            (JobKind::Batch, items)
        }
        JobRequest::Flow(flow) => {
            let planned = plan_flow(flow, checks).await?;
            check_session(&state, &caller, session.as_ref(), &[&planned.planned]).await?;
            // One item per step, and one for the page the flow ends on.
            let mut items = Vec::with_capacity(flow.actions.len() + 1);
            for (step, seq) in flow.actions.iter().zip(0_u64..) {
                let input =
                    serde_json::to_value(step).map_err(|error| ApiError::internal(&error))?;
                items.push(item(seq, input, Some(planned.site.clone())));
            }
            let last = u64::try_from(items.len()).unwrap_or(u64::MAX);
            items.push(item(last, json!({ "final": true }), Some(planned.site)));
            (JobKind::Flow, items)
        }
        JobRequest::Crawl(crawl) => {
            let planned = plan_crawl(crawl, checks).await?;
            let seeds: Vec<&Planned> = planned.seeds.iter().collect();
            check_session(&state, &caller, session.as_ref(), &seeds).await?;
            crawl_start = Some(planned.start);
            // A crawl has no items yet: one is added for every page it
            // asks a site for.
            (JobKind::Crawl, Vec::new())
        }
    };

    let config = state.config().jobs;
    let later = |wait: std::time::Duration| {
        chrono::Duration::from_std(wait)
            .ok()
            .and_then(|wait| now.checked_add_signed(wait))
    };
    let webhook_secret = webhook
        .as_ref()
        .map(|webhook| {
            state
                .jobs()
                .cipher
                .seal_bound(&webhook::secret_binding(id), &webhook.secret)
        })
        .transpose()
        .map_err(|error| ApiError::internal(&error))?;
    let job = Job {
        id,
        api_key_id: caller.key.id,
        kind,
        state: JobState::Queued,
        spec: stored_spec(&request, items.len())?,
        idempotency_hash: key.as_ref().map(|_| hash),
        idempotency_until: key
            .as_ref()
            .and_then(|_| later(config.idempotency_ttl.get())),
        idempotency_key: key,
        origin: JobOrigin::Api,
        webhook_url: webhook.as_ref().map(|webhook| webhook.url.to_string()),
        webhook_secret,
        items_total: u32::try_from(items.len()).unwrap_or(u32::MAX),
        items_succeeded: 0,
        items_failed: 0,
        items_cancelled: 0,
        cancel_requested: false,
        error: None,
        deadline: later(config.max_age.get()),
        paused_until: None,
        paused_reason: None,
        created_at: now,
        started_at: None,
        finished_at: None,
    };
    let created = state
        .store()
        .jobs()
        .create(&job, &items, now)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    if let Created::Existing(existing) = created {
        // The same key came twice at the same moment: the first one won.
        return replayed(&state, &existing, hash).await;
    }
    if let Some(start) = crawl_start {
        // What the crawl shows until its worker has begun.
        let start = serde_json::to_value(start).map_err(|error| ApiError::internal(&error))?;
        state
            .store()
            .jobs()
            .set_crawl(id, &start)
            .await
            .map_err(|error| ApiError::internal(&error))?;
    }
    metrics::counter!("kage_jobs_accepted_total", "job_type" => kind.as_str()).increment(1);
    state.jobs().wake();

    let view = view_of(&state, &job).await?;
    let mut response = (StatusCode::CREATED, Json(view)).into_response();
    if let Ok(location) = HeaderValue::from_str(&format!("/v1/jobs/{id}")) {
        response.headers_mut().insert(LOCATION, location);
    }
    Ok(response)
}

/// A job: its state, how far it has come, and how the deliveries of its
/// webhook stand.
#[utoipa::path(
    get,
    path = "/v1/jobs/{id}",
    tag = "jobs",
    operation_id = "get_job",
    params(("id" = String, Path, format = Uuid, description = "The job's id.")),
    responses(
        (status = 200, description = "The job.", body = JobView),
        (status = 404, description = "`job_not_found`: no such job of this key.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["jobs"]))
)]
pub(crate) async fn show(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
) -> ApiResult<Json<JobView>> {
    caller.require(Scope::Jobs)?;
    let job = owned_job(&state, &caller, &id).await?;
    Ok(Json(view_of(&state, &job).await?))
}

/// Which items of a job to read.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub(crate) struct ItemsQuery {
    /// `next_cursor` of the page before; left out for the first page.
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Items per page: 50 unless said, at most 200.
    #[serde(default)]
    #[param(minimum = 1, maximum = 200)]
    pub(crate) limit: Option<u32>,
    /// False leaves every item's `result` out: states and outcomes only.
    #[serde(default)]
    pub(crate) results: Option<bool>,
}

/// The stored result of an item, as its job's key sees it.
async fn result_of(
    state: &ApiState,
    item: &JobItem,
    viewer: &Viewer<'_>,
) -> ApiResult<Option<crate::fetch::result::FetchResult>> {
    let Some(request) = item.request_id else {
        return Ok(None);
    };
    let store = state.store();
    let Some(record) = store
        .results()
        .get_by_request(request)
        .await
        .map_err(|error| ApiError::internal(&error))?
    else {
        return Ok(None);
    };
    let attempts = store
        .attempts()
        .list_for_request(request)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    // What the item asked for; a capture of a flow asked for what was
    // kept of it.
    let include = serde_json::from_value::<FetchRequest>(item.input.clone())
        .ok()
        .and_then(|request| Included::of(&request))
        .unwrap_or_else(Included::everything);
    let session = store
        .requests()
        .get(request)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .and_then(|request| request.session_id);
    // The document and the body stay artifacts: a page of items is a list.
    Ok(Some(from_stored(
        &record,
        &attempts,
        Bodies::default(),
        &include,
        session.as_ref().map(SessionId::as_str),
        viewer,
    )))
}

/// The items of a job, in the job's order, each with its stored result.
///
/// A result here is what `GET /v1/results/{request_id}` returns without
/// the document and the body, which are artifacts (named in `artifacts`).
/// An item of a flow is one of its steps; the last item is the page the
/// flow ended on. An item of a crawl is a page the crawl asked for, in
/// the order it asked, with what the page was to the crawl in `crawl`;
/// the list grows while the crawl reads.
#[utoipa::path(
    get,
    path = "/v1/jobs/{id}/items",
    tag = "jobs",
    operation_id = "list_job_items",
    params(("id" = String, Path, format = Uuid, description = "The job's id."), ItemsQuery),
    responses(
        (status = 200, description = "A page of items.", body = JobItemsPage),
        (status = 404, description = "`job_not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The cursor or the limit is not one.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["jobs"]))
)]
pub(crate) async fn items(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
    Params(query): Params<ItemsQuery>,
) -> ApiResult<Json<JobItemsPage>> {
    caller.require(Scope::Jobs)?;
    let job = owned_job(&state, &caller, &id).await?;
    let after = query
        .cursor
        .as_deref()
        .map(|cursor| {
            cursor
                .parse::<u64>()
                .map_err(|_| ApiError::field("cursor", "the next_cursor of the page before"))
        })
        .transpose()?;
    let limit = query.limit.unwrap_or(DEFAULT_ITEMS_PAGE);
    if !(1..=MAX_ITEMS_PAGE).contains(&limit) {
        return Err(ApiError::field(
            "limit",
            format!("between 1 and {MAX_ITEMS_PAGE}"),
        ));
    }
    // One more than asked for says whether there is a next page.
    let mut listed = state
        .store()
        .job_items()
        .list(job.id, after, limit + 1)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let more = listed.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    listed.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    let viewer = Viewer {
        key: caller.key.id,
        admin: caller.has(Scope::Admin),
        handles: state.handles(),
    };
    let with_results = query.results.unwrap_or(true);
    let mut items = Vec::with_capacity(listed.len());
    for item in &listed {
        let result = if with_results {
            result_of(&state, item, &viewer).await?
        } else {
            None
        };
        items.push(JobItemView::of(item, job.kind, result));
    }
    Ok(Json(JobItemsPage {
        next_cursor: listed
            .last()
            .filter(|_| more)
            .map(|item| item.seq.to_string()),
        items,
    }))
}

/// Stop a job.
///
/// What waits is cancelled at once; an item that waits for its persona is
/// taken out of the queue; a page that is being fetched at this moment is
/// finished, so at most one more item per visitor ends after the cancel.
/// The answer is the job as it stands; it is `cancelled` once what was
/// under way is over. A job that has ended is answered as it ended.
#[utoipa::path(
    post,
    path = "/v1/jobs/{id}/cancel",
    tag = "jobs",
    operation_id = "cancel_job",
    params(("id" = String, Path, format = Uuid, description = "The job's id.")),
    responses(
        (status = 200, description = "The job as it stands.", body = JobView),
        (status = 404, description = "`job_not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["jobs"]))
)]
pub(crate) async fn cancel(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
) -> ApiResult<Json<JobView>> {
    caller.require(Scope::Jobs)?;
    let job = owned_job(&state, &caller, &id).await?;
    let job = state
        .store()
        .jobs()
        .request_cancel(job.id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(job_not_found)?;
    state.jobs().nudge(job.id);
    Ok(Json(view_of(&state, &job).await?))
}

/// What the retention job does for jobs and results every sweep: finished
/// jobs older than `jobs.retention_days` go with everything of theirs,
/// and fetches that are no job's go when their results are as old as
/// `artifacts.retention_days`. Returns how many jobs and how many requests
/// went, those of the jobs among them.
///
/// # Errors
///
/// Fails when the store fails.
pub async fn sweep(
    state: &ApiState,
    now: DateTime<Utc>,
) -> Result<(u64, u64), kage_store::StoreError> {
    let config = state.config();
    let before = |kept: std::time::Duration| {
        chrono::Duration::from_std(kept)
            .ok()
            .and_then(|kept| now.checked_sub_signed(kept))
            .unwrap_or(now)
    };
    let ended = state
        .store()
        .jobs()
        .delete_finished(before(config.jobs.retention()), None)
        .await?;
    let (requests, _) = state
        .store()
        .requests()
        .delete_before(before(config.artifacts.retention()), None, false)
        .await?;
    Ok((ended.jobs, ended.requests + requests))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use serde_json::json;

    use super::{fingerprint, idempotency_key};
    use crate::error::ErrorCode;

    #[test]
    fn an_idempotency_key_is_a_short_word_without_spaces() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("idempotency-key", value.parse().unwrap());
            idempotency_key(&headers)
        };

        assert_eq!(idempotency_key(&HeaderMap::new()).unwrap(), None);
        assert_eq!(with("order-17").unwrap().as_deref(), Some("order-17"));
        assert_eq!(
            with(" 0199c1f2-6b7a-7c3e ").unwrap().as_deref(),
            Some("0199c1f2-6b7a-7c3e")
        );
        for refused in ["", "two words", &"x".repeat(256)] {
            let error = with(refused).unwrap_err();
            assert_eq!(error.code(), ErrorCode::ValidationFailed, "{refused:?}");
            assert_eq!(error.problem().extensions["header"], "Idempotency-Key");
        }
    }

    #[test]
    fn a_fingerprint_is_the_requests_whatever_the_order_of_its_members() {
        let key = [7_u8; 32];
        let one = json!({ "type": "batch", "urls": ["https://a.example/"], "visitors": 2 });
        let same = json!({ "visitors": 2, "urls": ["https://a.example/"], "type": "batch" });
        let other = json!({ "type": "batch", "urls": ["https://b.example/"], "visitors": 2 });

        assert_eq!(fingerprint(&key, &one), fingerprint(&key, &same));
        assert_ne!(fingerprint(&key, &one), fingerprint(&key, &other));
        // The order of a list is part of what it says.
        let swapped =
            json!({ "type": "batch", "urls": ["https://b.example/", "https://a.example/"] });
        let listed =
            json!({ "type": "batch", "urls": ["https://a.example/", "https://b.example/"] });
        assert_ne!(fingerprint(&key, &swapped), fingerprint(&key, &listed));
        // Keyed: another server's fingerprint of the same body is another.
        assert_ne!(fingerprint(&key, &one), fingerprint(&[8; 32], &one));
    }
}
