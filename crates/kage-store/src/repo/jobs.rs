//! The `jobs`, `job_items` and `crawl_frontier` tables.
//!
//! These repositories cover storage only. Claiming work with
//! `FOR UPDATE SKIP LOCKED` belongs to the worker loops of milestones M9 and
//! M10, which add their queries here.

use chrono::{DateTime, Utc};
use kage_core::domain::{FrontierEntry, FrontierState, Job, JobItem};
use kage_core::ids::{ApiKeyId, FrontierId, JobId, JobItemId, RequestId};
use kage_core::secret::SealedSecret;
use uuid::Uuid;

use super::{page_limit, repository};
use crate::StoreError;
use crate::convert::{bits_to_i64, bits_to_u64, parse, to_i32, to_i64, to_u32, to_u64};

repository!(
    /// Jobs: batch, flow and crawl.
    JobRepo
);

struct JobRow {
    id: Uuid,
    api_key_id: Uuid,
    kind: String,
    state: String,
    spec: serde_json::Value,
    idempotency_key: Option<String>,
    webhook_url: Option<String>,
    webhook_secret: Option<Vec<u8>>,
    items_total: i32,
    items_succeeded: i32,
    items_failed: i32,
    cancel_requested: bool,
    error: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<JobRow> for Job {
    type Error = StoreError;

    fn try_from(row: JobRow) -> Result<Self, StoreError> {
        const TABLE: &str = "jobs";
        Ok(Self {
            id: JobId::from_uuid(row.id),
            api_key_id: ApiKeyId::from_uuid(row.api_key_id),
            kind: parse((TABLE, "kind"), &row.kind)?,
            state: parse((TABLE, "state"), &row.state)?,
            spec: row.spec,
            idempotency_key: row.idempotency_key,
            webhook_url: row.webhook_url,
            webhook_secret: row.webhook_secret.map(SealedSecret::new),
            items_total: to_u32((TABLE, "items_total"), row.items_total)?,
            items_succeeded: to_u32((TABLE, "items_succeeded"), row.items_succeeded)?,
            items_failed: to_u32((TABLE, "items_failed"), row.items_failed)?,
            cancel_requested: row.cancel_requested,
            error: row.error,
            created_at: row.created_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
    }
}

impl JobRepo<'_> {
    /// Stores a new job.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the key already has a job with this
    /// idempotency key.
    pub async fn insert(&self, job: &Job) -> Result<(), StoreError> {
        const TABLE: &str = "jobs";
        sqlx::query!(
            "INSERT INTO jobs (id, api_key_id, kind, state, spec, idempotency_key, webhook_url,
                               webhook_secret, items_total, items_succeeded, items_failed,
                               cancel_requested, error, created_at, started_at, finished_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
            job.id.as_uuid(),
            job.api_key_id.as_uuid(),
            job.kind.as_str(),
            job.state.as_str(),
            job.spec,
            job.idempotency_key,
            job.webhook_url,
            job.webhook_secret.as_ref().map(SealedSecret::as_bytes),
            to_i32((TABLE, "items_total"), job.items_total)?,
            to_i32((TABLE, "items_succeeded"), job.items_succeeded)?,
            to_i32((TABLE, "items_failed"), job.items_failed)?,
            job.cancel_requested,
            job.error,
            job.created_at,
            job.started_at,
            job.finished_at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The job with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        sqlx::query_as!(
            JobRow,
            "SELECT id, api_key_id, kind, state, spec, idempotency_key, webhook_url,
                    webhook_secret, items_total, items_succeeded, items_failed, cancel_requested,
                    error, created_at, started_at, finished_at
             FROM jobs WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(Job::try_from)
        .transpose()
    }

    /// The job a key created with this idempotency key, if any.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn find_by_idempotency_key(
        &self,
        api_key_id: ApiKeyId,
        idempotency_key: &str,
    ) -> Result<Option<Job>, StoreError> {
        sqlx::query_as!(
            JobRow,
            "SELECT id, api_key_id, kind, state, spec, idempotency_key, webhook_url,
                    webhook_secret, items_total, items_succeeded, items_failed, cancel_requested,
                    error, created_at, started_at, finished_at
             FROM jobs WHERE api_key_id = $1 AND idempotency_key = $2",
            api_key_id.as_uuid(),
            idempotency_key,
        )
        .fetch_optional(self.pool)
        .await?
        .map(Job::try_from)
        .transpose()
    }

    /// Writes back everything about a job that changes during its life:
    /// state, counters, the cancel flag, the error and the timestamps.
    /// Returns whether the job exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a counter is out of range.
    pub async fn update(&self, job: &Job) -> Result<bool, StoreError> {
        const TABLE: &str = "jobs";
        let done = sqlx::query!(
            "UPDATE jobs
             SET state = $2, items_total = $3, items_succeeded = $4, items_failed = $5,
                 cancel_requested = $6, error = $7, started_at = $8, finished_at = $9
             WHERE id = $1",
            job.id.as_uuid(),
            job.state.as_str(),
            to_i32((TABLE, "items_total"), job.items_total)?,
            to_i32((TABLE, "items_succeeded"), job.items_succeeded)?,
            to_i32((TABLE, "items_failed"), job.items_failed)?,
            job.cancel_requested,
            job.error,
            job.started_at,
            job.finished_at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}

repository!(
    /// Job items: the units a job consists of.
    JobItemRepo
);

struct JobItemRow {
    id: Uuid,
    job_id: Uuid,
    seq: i64,
    state: String,
    input: serde_json::Value,
    request_id: Option<Uuid>,
    error: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<JobItemRow> for JobItem {
    type Error = StoreError;

    fn try_from(row: JobItemRow) -> Result<Self, StoreError> {
        const TABLE: &str = "job_items";
        Ok(Self {
            id: JobItemId::from_uuid(row.id),
            job_id: JobId::from_uuid(row.job_id),
            seq: to_u64((TABLE, "seq"), row.seq)?,
            state: parse((TABLE, "state"), &row.state)?,
            input: row.input,
            request_id: row.request_id.map(RequestId::from_uuid),
            error: row.error,
            created_at: row.created_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
    }
}

impl JobItemRepo<'_> {
    /// Stores a new item.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the job already has an item at this
    /// position.
    pub async fn insert(&self, item: &JobItem) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO job_items (id, job_id, seq, state, input, request_id, error, created_at,
                                    started_at, finished_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            item.id.as_uuid(),
            item.job_id.as_uuid(),
            to_i64(("job_items", "seq"), item.seq)?,
            item.state.as_str(),
            item.input,
            item.request_id.map(|id| id.as_uuid()),
            item.error,
            item.created_at,
            item.started_at,
            item.finished_at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// A page of a job's items in position order. Pass the `seq` of the last
    /// item of the previous page as `after` to continue.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list(
        &self,
        job_id: JobId,
        after: Option<u64>,
        limit: u32,
    ) -> Result<Vec<JobItem>, StoreError> {
        let after = after
            .map(|seq| to_i64(("job_items", "seq"), seq))
            .transpose()?;
        sqlx::query_as!(
            JobItemRow,
            "SELECT id, job_id, seq, state, input, request_id, error, created_at, started_at,
                    finished_at
             FROM job_items
             WHERE job_id = $1 AND ($2::bigint IS NULL OR seq > $2)
             ORDER BY seq
             LIMIT $3",
            job_id.as_uuid(),
            after,
            page_limit(limit),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(JobItem::try_from)
        .collect()
    }

    /// Writes back an item's state, request, error and timestamps. Returns
    /// whether the item exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn update(&self, item: &JobItem) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE job_items
             SET state = $2, request_id = $3, error = $4, started_at = $5, finished_at = $6
             WHERE id = $1",
            item.id.as_uuid(),
            item.state.as_str(),
            item.request_id.map(|id| id.as_uuid()),
            item.error,
            item.started_at,
            item.finished_at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}

repository!(
    /// The crawl frontier: one entry per normalized URL per crawl.
    FrontierRepo
);

struct FrontierRow {
    id: Uuid,
    job_id: Uuid,
    url: String,
    url_key: String,
    depth: i32,
    state: String,
    discovered_from: Option<String>,
    content_hash: Option<String>,
    simhash: Option<i64>,
    near_duplicate: bool,
    created_at: DateTime<Utc>,
    fetched_at: Option<DateTime<Utc>>,
}

impl TryFrom<FrontierRow> for FrontierEntry {
    type Error = StoreError;

    fn try_from(row: FrontierRow) -> Result<Self, StoreError> {
        const TABLE: &str = "crawl_frontier";
        Ok(Self {
            id: FrontierId::from_uuid(row.id),
            job_id: JobId::from_uuid(row.job_id),
            url: row.url,
            url_key: row.url_key,
            depth: to_u32((TABLE, "depth"), row.depth)?,
            state: parse((TABLE, "state"), &row.state)?,
            discovered_from: row.discovered_from,
            content_hash: row.content_hash,
            simhash: row.simhash.map(bits_to_u64),
            near_duplicate: row.near_duplicate,
            created_at: row.created_at,
            fetched_at: row.fetched_at,
        })
    }
}

impl FrontierRepo<'_> {
    /// Adds a discovered URL unless the crawl already has its key. Returns
    /// whether it was added: `false` means the URL is a duplicate.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn insert_if_new(&self, entry: &FrontierEntry) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "INSERT INTO crawl_frontier (id, job_id, url, url_key, depth, state, discovered_from,
                                         content_hash, simhash, near_duplicate, created_at,
                                         fetched_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
             ON CONFLICT (job_id, url_key) DO NOTHING",
            entry.id.as_uuid(),
            entry.job_id.as_uuid(),
            entry.url,
            entry.url_key,
            to_i32(("crawl_frontier", "depth"), entry.depth)?,
            entry.state.as_str(),
            entry.discovered_from,
            entry.content_hash,
            entry.simhash.map(bits_to_i64),
            entry.near_duplicate,
            entry.created_at,
            entry.fetched_at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// A crawl's entries in `state`, shallowest and oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list(
        &self,
        job_id: JobId,
        state: FrontierState,
        limit: u32,
    ) -> Result<Vec<FrontierEntry>, StoreError> {
        sqlx::query_as!(
            FrontierRow,
            "SELECT id, job_id, url, url_key, depth, state, discovered_from, content_hash, simhash,
                    near_duplicate, created_at, fetched_at
             FROM crawl_frontier
             WHERE job_id = $1 AND state = $2
             ORDER BY depth, created_at, id
             LIMIT $3",
            job_id.as_uuid(),
            state.as_str(),
            page_limit(limit),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(FrontierEntry::try_from)
        .collect()
    }

    /// How many of a crawl's entries are in `state`.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn count(&self, job_id: JobId, state: FrontierState) -> Result<u64, StoreError> {
        let row = sqlx::query!(
            r#"SELECT count(*) AS "count!" FROM crawl_frontier WHERE job_id = $1 AND state = $2"#,
            job_id.as_uuid(),
            state.as_str(),
        )
        .fetch_one(self.pool)
        .await?;
        to_u64(("crawl_frontier", "count"), row.count)
    }

    /// Writes back an entry's state and what fetching it found. Returns
    /// whether the entry exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn update(&self, entry: &FrontierEntry) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE crawl_frontier
             SET state = $2, content_hash = $3, simhash = $4, near_duplicate = $5, fetched_at = $6
             WHERE id = $1",
            entry.id.as_uuid(),
            entry.state.as_str(),
            entry.content_hash,
            entry.simhash.map(bits_to_i64),
            entry.near_duplicate,
            entry.fetched_at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}
