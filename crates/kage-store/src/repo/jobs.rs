//! The `jobs`, `job_items` and `crawl_frontier` tables.
//!
//! A job is worked by one worker at a time. The worker claims it with
//! `SELECT ... FOR UPDATE SKIP LOCKED` under a lease that it renews while
//! it works ([`JobRepo::claim`], [`JobRepo::renew`]); a job whose lease ran
//! out is claimed again, by the same process after a restart or by another
//! one. The items of a claimed job are its worker's: it reads the open ones
//! and writes each back as it ends, together with the job's counters.
//!
//! Claiming the frontier belongs to the crawler of milestone M10.

use chrono::{DateTime, Utc};
use kage_core::domain::{
    FrontierEntry, FrontierState, Job, JobItem, JobItemState, JobKind, JobState,
};
use kage_core::ids::{ApiKeyId, FrontierId, JobId, JobItemId, RequestId};
use kage_core::outcome::Outcome;
use kage_core::secret::SealedSecret;
use uuid::Uuid;

use super::{page_limit, repository};
use crate::StoreError;
use crate::convert::{bits_to_i64, bits_to_u64, parse, parse_opt, to_i32, to_i64, to_u32, to_u64};

/// How many items one statement inserts.
const INSERT_CHUNK: usize = 500;

/// How many rows one statement of a deletion removes.
const DELETE_CHUNK: i64 = 500;

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
    idempotency_hash: Option<Vec<u8>>,
    idempotency_until: Option<DateTime<Utc>>,
    origin: String,
    webhook_url: Option<String>,
    webhook_secret: Option<Vec<u8>>,
    items_total: i32,
    items_succeeded: i32,
    items_failed: i32,
    items_cancelled: i32,
    cancel_requested: bool,
    error: Option<String>,
    deadline: Option<DateTime<Utc>>,
    paused_until: Option<DateTime<Utc>>,
    paused_reason: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<JobRow> for Job {
    type Error = StoreError;

    fn try_from(row: JobRow) -> Result<Self, StoreError> {
        const TABLE: &str = "jobs";
        let idempotency_hash = row
            .idempotency_hash
            .map(|hash| {
                <[u8; 32]>::try_from(hash).map_err(|_| StoreError::Corrupt {
                    table: TABLE,
                    column: "idempotency_hash",
                    reason: "not 32 bytes".to_owned(),
                })
            })
            .transpose()?;
        Ok(Self {
            id: JobId::from_uuid(row.id),
            api_key_id: ApiKeyId::from_uuid(row.api_key_id),
            kind: parse((TABLE, "kind"), &row.kind)?,
            state: parse((TABLE, "state"), &row.state)?,
            spec: row.spec,
            idempotency_key: row.idempotency_key,
            idempotency_hash,
            idempotency_until: row.idempotency_until,
            origin: parse((TABLE, "origin"), &row.origin)?,
            webhook_url: row.webhook_url,
            webhook_secret: row.webhook_secret.map(SealedSecret::new),
            items_total: to_u32((TABLE, "items_total"), row.items_total)?,
            items_succeeded: to_u32((TABLE, "items_succeeded"), row.items_succeeded)?,
            items_failed: to_u32((TABLE, "items_failed"), row.items_failed)?,
            items_cancelled: to_u32((TABLE, "items_cancelled"), row.items_cancelled)?,
            cancel_requested: row.cancel_requested,
            error: row.error,
            deadline: row.deadline,
            paused_until: row.paused_until,
            paused_reason: row.paused_reason,
            created_at: row.created_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
    }
}

/// What storing a new job came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Created {
    /// The job and its items are stored.
    New,
    /// The key already made a job under this idempotency key, and the key
    /// still names it: nothing was stored.
    Existing(Box<Job>),
}

/// How one item's end moves its job's counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tally {
    /// Items that succeeded.
    pub succeeded: u32,
    /// Items that failed.
    pub failed: u32,
    /// Items that were cancelled.
    pub cancelled: u32,
}

impl Tally {
    /// The tally of one item that ended in `state`; nothing for a state
    /// that is not an end.
    pub const fn of(state: JobItemState) -> Self {
        let (succeeded, failed, cancelled) = match state {
            JobItemState::Succeeded => (1, 0, 0),
            JobItemState::Failed => (0, 1, 0),
            JobItemState::Cancelled => (0, 0, 1),
            JobItemState::Queued | JobItemState::Running => (0, 0, 0),
        };
        Self {
            succeeded,
            failed,
            cancelled,
        }
    }
}

/// How many jobs of a kind are in a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobCount {
    /// The kind.
    pub kind: JobKind,
    /// The state.
    pub state: JobState,
    /// How many.
    pub jobs: u64,
}

/// Inserts the items of a new job, a chunk per statement.
async fn insert_items(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    job_id: JobId,
    items: &[JobItem],
) -> Result<(), StoreError> {
    for chunk in items.chunks(INSERT_CHUNK) {
        let mut ids = Vec::with_capacity(chunk.len());
        let mut seqs = Vec::with_capacity(chunk.len());
        let mut states = Vec::with_capacity(chunk.len());
        let mut inputs = Vec::with_capacity(chunk.len());
        let mut sites = Vec::with_capacity(chunk.len());
        let mut requests = Vec::with_capacity(chunk.len());
        let mut created = Vec::with_capacity(chunk.len());
        let mut started = Vec::with_capacity(chunk.len());
        for item in chunk {
            ids.push(item.id.as_uuid());
            seqs.push(to_i64(("job_items", "seq"), item.seq)?);
            states.push(item.state.as_str().to_owned());
            inputs.push(item.input.clone());
            sites.push(item.site.clone());
            requests.push(item.request_id.map(|id| id.as_uuid()));
            created.push(item.created_at);
            started.push(item.started_at);
        }
        sqlx::query!(
            "INSERT INTO job_items (id, job_id, seq, state, input, site, request_id,
                                    created_at, started_at)
             SELECT item.id, $1, item.seq, item.state, item.input, item.site,
                    item.request_id, item.created_at, item.started_at
             FROM UNNEST($2::uuid[], $3::bigint[], $4::text[], $5::jsonb[], $6::text[],
                         $7::uuid[], $8::timestamptz[], $9::timestamptz[])
                  AS item (id, seq, state, input, site, request_id, created_at, started_at)",
            job_id.as_uuid(),
            &ids,
            &seqs,
            &states,
            &inputs,
            &sites as _,
            &requests as _,
            &created,
            &started as _,
        )
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

impl JobRepo<'_> {
    /// Stores a new job without items.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the key already has a job with this
    /// idempotency key.
    pub async fn insert(&self, job: &Job) -> Result<(), StoreError> {
        match self.create(job, &[], job.created_at).await? {
            Created::New => Ok(()),
            Created::Existing(_) => Err(StoreError::Conflict {
                constraint: "jobs_api_key_id_idempotency_key_key".to_owned(),
            }),
        }
    }

    /// Stores a new job with its items, all or nothing. An idempotency key
    /// that still names an earlier job of the same key stores nothing and
    /// returns that job; one whose time is over is let go first.
    ///
    /// # Errors
    ///
    /// Fails when a statement fails or a counter is out of range.
    pub async fn create(
        &self,
        job: &Job,
        items: &[JobItem],
        now: DateTime<Utc>,
    ) -> Result<Created, StoreError> {
        self.create_with(job, items, now, None).await
    }

    /// Stores a new job that a worker holds from its first moment, under a
    /// lease until `lease_until`: a fetch that is under way in that worker
    /// and is carried on as a job. No other worker claims it while the
    /// lease is renewed.
    ///
    /// # Errors
    ///
    /// As [`Self::create`].
    pub async fn create_leased(
        &self,
        job: &Job,
        items: &[JobItem],
        now: DateTime<Utc>,
        worker: Uuid,
        lease_until: DateTime<Utc>,
    ) -> Result<Created, StoreError> {
        self.create_with(job, items, now, Some((worker, lease_until)))
            .await
    }

    async fn create_with(
        &self,
        job: &Job,
        items: &[JobItem],
        now: DateTime<Utc>,
        lease: Option<(Uuid, DateTime<Utc>)>,
    ) -> Result<Created, StoreError> {
        const TABLE: &str = "jobs";
        let mut transaction = self.pool.begin().await?;
        if let Some(key) = &job.idempotency_key {
            // A key whose time is over names nothing any more.
            sqlx::query!(
                "UPDATE jobs SET idempotency_key = NULL
                 WHERE api_key_id = $1 AND idempotency_key = $2
                   AND idempotency_until IS NOT NULL AND idempotency_until <= $3",
                job.api_key_id.as_uuid(),
                key,
                now,
            )
            .execute(&mut *transaction)
            .await?;
        }
        let inserted = sqlx::query!(
            "INSERT INTO jobs (id, api_key_id, kind, state, spec, idempotency_key,
                               idempotency_hash, idempotency_until, origin, webhook_url,
                               webhook_secret, items_total, items_succeeded, items_failed,
                               items_cancelled, cancel_requested, error, deadline, paused_until,
                               paused_reason, created_at, started_at, finished_at, worker,
                               lease_until)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                     $18, $19, $20, $21, $22, $23, $24, $25)
             ON CONFLICT (api_key_id, idempotency_key) DO NOTHING",
            job.id.as_uuid(),
            job.api_key_id.as_uuid(),
            job.kind.as_str(),
            job.state.as_str(),
            job.spec,
            job.idempotency_key,
            job.idempotency_hash.as_ref().map(<[u8; 32]>::as_slice),
            job.idempotency_until,
            job.origin.as_str(),
            job.webhook_url,
            job.webhook_secret.as_ref().map(SealedSecret::as_bytes),
            to_i32((TABLE, "items_total"), job.items_total)?,
            to_i32((TABLE, "items_succeeded"), job.items_succeeded)?,
            to_i32((TABLE, "items_failed"), job.items_failed)?,
            to_i32((TABLE, "items_cancelled"), job.items_cancelled)?,
            job.cancel_requested,
            job.error,
            job.deadline,
            job.paused_until,
            job.paused_reason,
            job.created_at,
            job.started_at,
            job.finished_at,
            lease.map(|(worker, _)| worker),
            lease.map(|(_, until)| until),
        )
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() == 0 {
            transaction.rollback().await?;
            let key = job.idempotency_key.as_deref().unwrap_or_default();
            return match self.find_by_idempotency_key(job.api_key_id, key).await? {
                Some(existing) => Ok(Created::Existing(Box::new(existing))),
                // It was there a moment ago and is gone: the caller asks again.
                None => Err(StoreError::Conflict {
                    constraint: "jobs_api_key_id_idempotency_key_key".to_owned(),
                }),
            };
        }
        insert_items(&mut transaction, job.id, items).await?;
        transaction.commit().await?;
        Ok(Created::New)
    }

    /// The job with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        sqlx::query_as!(
            JobRow,
            "SELECT id, api_key_id, kind, state, spec, idempotency_key, idempotency_hash,
                    idempotency_until, origin, webhook_url, webhook_secret, items_total,
                    items_succeeded, items_failed, items_cancelled, cancel_requested, error,
                    deadline, paused_until, paused_reason, created_at, started_at, finished_at
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
            "SELECT id, api_key_id, kind, state, spec, idempotency_key, idempotency_hash,
                    idempotency_until, origin, webhook_url, webhook_secret, items_total,
                    items_succeeded, items_failed, items_cancelled, cancel_requested, error,
                    deadline, paused_until, paused_reason, created_at, started_at, finished_at
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
    /// state, counters, the cancel flag, the error, the pause and the
    /// timestamps. Returns whether the job exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a counter is out of range.
    pub async fn update(&self, job: &Job) -> Result<bool, StoreError> {
        const TABLE: &str = "jobs";
        let done = sqlx::query!(
            "UPDATE jobs
             SET state = $2, items_total = $3, items_succeeded = $4, items_failed = $5,
                 items_cancelled = $6, cancel_requested = $7, error = $8, started_at = $9,
                 finished_at = $10, paused_until = $11, paused_reason = $12
             WHERE id = $1",
            job.id.as_uuid(),
            job.state.as_str(),
            to_i32((TABLE, "items_total"), job.items_total)?,
            to_i32((TABLE, "items_succeeded"), job.items_succeeded)?,
            to_i32((TABLE, "items_failed"), job.items_failed)?,
            to_i32((TABLE, "items_cancelled"), job.items_cancelled)?,
            job.cancel_requested,
            job.error,
            job.started_at,
            job.finished_at,
            job.paused_until,
            job.paused_reason,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Claims up to `limit` jobs that nobody works: queued ones, and
    /// running or paused ones whose worker's lease ran out, oldest first,
    /// each under a lease until `lease_until`. A paused job is left alone
    /// until what it waits for is over, unless it was cancelled meanwhile.
    /// Rows another worker is claiming at this moment are skipped, not
    /// waited for. A queued job becomes `running`.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn claim(
        &self,
        worker: Uuid,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<Job>, StoreError> {
        sqlx::query_as!(
            JobRow,
            "UPDATE jobs
             SET lease_until = $3, worker = $1, started_at = COALESCE(started_at, $2),
                 state = CASE WHEN state = 'queued' THEN 'running' ELSE state END
             WHERE id IN (
                 SELECT id FROM jobs
                 WHERE state IN ('queued', 'running', 'paused')
                   AND (lease_until IS NULL OR lease_until <= $2)
                   AND (state <> 'paused' OR cancel_requested OR paused_until IS NULL
                        OR paused_until <= $2 OR deadline <= $2)
                 ORDER BY created_at, id
                 LIMIT $4
                 FOR UPDATE SKIP LOCKED)
             RETURNING id, api_key_id, kind, state, spec, idempotency_key, idempotency_hash,
                       idempotency_until, origin, webhook_url, webhook_secret, items_total,
                       items_succeeded, items_failed, items_cancelled, cancel_requested, error,
                       deadline, paused_until, paused_reason, created_at, started_at, finished_at",
            worker,
            now,
            lease_until,
            page_limit(limit),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Job::try_from)
        .collect::<Result<Vec<Job>, StoreError>>()
        .map(|mut jobs| {
            jobs.sort_by_key(|job| (job.created_at, job.id));
            jobs
        })
    }

    /// Renews a worker's lease on a job. `None` when the job is no longer
    /// this worker's (its lease ran out and another took it, or it is
    /// gone); otherwise the job as it stands, so that the worker sees a
    /// cancel that was asked for meanwhile.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn renew(
        &self,
        id: JobId,
        worker: Uuid,
        lease_until: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError> {
        sqlx::query_as!(
            JobRow,
            "UPDATE jobs SET lease_until = $3
             WHERE id = $1 AND worker = $2 AND finished_at IS NULL
             RETURNING id, api_key_id, kind, state, spec, idempotency_key, idempotency_hash,
                       idempotency_until, origin, webhook_url, webhook_secret, items_total,
                       items_succeeded, items_failed, items_cancelled, cancel_requested, error,
                       deadline, paused_until, paused_reason, created_at, started_at, finished_at",
            id.as_uuid(),
            worker,
            lease_until,
        )
        .fetch_optional(self.pool)
        .await?
        .map(Job::try_from)
        .transpose()
    }

    /// Gives a job back before it is done: its worker stops, or everything
    /// that is left of it waits. With `paused` the job is `paused` until
    /// that moment for that reason; without, it is `running` and free to be
    /// claimed at once.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn release(
        &self,
        id: JobId,
        worker: Uuid,
        paused: Option<(DateTime<Utc>, &str)>,
    ) -> Result<bool, StoreError> {
        let (until, reason) = match paused {
            Some((until, reason)) => (Some(until), Some(reason)),
            None => (None, None),
        };
        let done = sqlx::query!(
            "UPDATE jobs
             SET lease_until = NULL, worker = NULL, paused_until = $3, paused_reason = $4,
                 state = CASE WHEN $3::timestamptz IS NULL THEN 'running' ELSE 'paused' END
             WHERE id = $1 AND worker = $2 AND finished_at IS NULL",
            id.as_uuid(),
            worker,
            until,
            reason,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Marks a job that is being worked as `running` again after a pause,
    /// or as `paused` while its worker keeps it.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn set_paused(
        &self,
        id: JobId,
        paused: Option<(DateTime<Utc>, &str)>,
    ) -> Result<bool, StoreError> {
        let (until, reason) = match paused {
            Some((until, reason)) => (Some(until), Some(reason)),
            None => (None, None),
        };
        let done = sqlx::query!(
            "UPDATE jobs
             SET paused_until = $2, paused_reason = $3,
                 state = CASE WHEN $2::timestamptz IS NULL THEN 'running' ELSE 'paused' END
             WHERE id = $1 AND finished_at IS NULL",
            id.as_uuid(),
            until,
            reason,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Asks a job to stop (spec §16.3, cooperative cancel). Returns the job
    /// as it stands afterwards, or `None` when there is no such job. A job
    /// that has ended is returned unchanged.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn request_cancel(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        sqlx::query!(
            "UPDATE jobs SET cancel_requested = true WHERE id = $1 AND finished_at IS NULL",
            id.as_uuid(),
        )
        .execute(self.pool)
        .await?;
        self.get(id).await
    }

    /// Ends a job: its state, why it failed when it did, and when.
    /// Returns the job as it stands, or `None` when it had ended already or
    /// is gone.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn finish(
        &self,
        id: JobId,
        state: JobState,
        error: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError> {
        sqlx::query_as!(
            JobRow,
            "UPDATE jobs
             SET state = $2, error = $3, finished_at = $4, lease_until = NULL, worker = NULL,
                 paused_until = NULL, paused_reason = NULL
             WHERE id = $1 AND finished_at IS NULL
             RETURNING id, api_key_id, kind, state, spec, idempotency_key, idempotency_hash,
                       idempotency_until, origin, webhook_url, webhook_secret, items_total,
                       items_succeeded, items_failed, items_cancelled, cancel_requested, error,
                       deadline, paused_until, paused_reason, created_at, started_at, finished_at",
            id.as_uuid(),
            state.as_str(),
            error,
            at,
        )
        .fetch_optional(self.pool)
        .await?
        .map(Job::try_from)
        .transpose()
    }

    /// The personas that visit each site for a job, as the worker keeps
    /// them: an object from site to a list of persona ids.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn visitors(&self, id: JobId) -> Result<serde_json::Value, StoreError> {
        Ok(
            sqlx::query_scalar!("SELECT visitors FROM jobs WHERE id = $1", id.as_uuid())
                .fetch_optional(self.pool)
                .await?
                .unwrap_or_else(|| serde_json::json!({})),
        )
    }

    /// Writes back the personas that visit each site for a job.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn set_visitors(
        &self,
        id: JobId,
        visitors: &serde_json::Value,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE jobs SET visitors = $2 WHERE id = $1",
            id.as_uuid(),
            visitors,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// How many jobs of each kind are in each state.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn counts(&self) -> Result<Vec<JobCount>, StoreError> {
        const TABLE: &str = "jobs";
        sqlx::query!(
            r#"SELECT kind, state, count(*) AS "jobs!" FROM jobs GROUP BY kind, state
               ORDER BY kind, state"#,
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(JobCount {
                kind: parse((TABLE, "kind"), &row.kind)?,
                state: parse((TABLE, "state"), &row.state)?,
                jobs: to_u64((TABLE, "count"), row.jobs)?,
            })
        })
        .collect()
    }

    /// Deletes the jobs that ended before `before`, of one key or of all,
    /// with their items, the requests of their items and what those kept:
    /// results, attempts, artifact rows and deliveries. The artifact files
    /// go with the artifact store's next sweep of orphans. Returns how many
    /// jobs, requests and artifact rows were deleted.
    ///
    /// # Errors
    ///
    /// Fails when a statement fails.
    pub async fn delete_finished(
        &self,
        before: DateTime<Utc>,
        api_key_id: Option<ApiKeyId>,
    ) -> Result<Deleted, StoreError> {
        let key = api_key_id.map(|id| id.as_uuid());
        let mut deleted = Deleted::default();
        loop {
            let mut transaction = self.pool.begin().await?;
            let ids: Vec<Uuid> = sqlx::query_scalar!(
                "SELECT id FROM jobs
                 WHERE finished_at IS NOT NULL AND finished_at < $1
                   AND ($2::uuid IS NULL OR api_key_id = $2)
                 ORDER BY finished_at
                 LIMIT $3
                 FOR UPDATE SKIP LOCKED",
                before,
                key,
                DELETE_CHUNK,
            )
            .fetch_all(&mut *transaction)
            .await?;
            if ids.is_empty() {
                transaction.rollback().await?;
                return Ok(deleted);
            }
            // The artifact rows point at requests with SET NULL: they are
            // removed by name, or nobody could read or purge them later.
            deleted.artifacts += sqlx::query!(
                "DELETE FROM artifacts WHERE request_id IN (
                     SELECT id FROM requests WHERE job_id = ANY($1))",
                &ids,
            )
            .execute(&mut *transaction)
            .await?
            .rows_affected();
            // The requests go with their job; they are deleted by name so
            // that they are counted.
            deleted.requests += sqlx::query!("DELETE FROM requests WHERE job_id = ANY($1)", &ids)
                .execute(&mut *transaction)
                .await?
                .rows_affected();
            deleted.jobs += sqlx::query!("DELETE FROM jobs WHERE id = ANY($1)", &ids)
                .execute(&mut *transaction)
                .await?
                .rows_affected();
            transaction.commit().await?;
        }
    }
}

/// What a deletion removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Deleted {
    /// Jobs, with their items and deliveries.
    pub jobs: u64,
    /// Requests, with their attempts and results.
    pub requests: u64,
    /// Rows of artifacts; their files go with the next sweep of orphans.
    pub artifacts: u64,
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
    site: Option<String>,
    request_id: Option<Uuid>,
    outcome: Option<String>,
    output: Option<serde_json::Value>,
    error: Option<String>,
    not_before: Option<DateTime<Utc>>,
    claims: i32,
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
            site: row.site,
            request_id: row.request_id.map(RequestId::from_uuid),
            outcome: parse_opt::<Outcome>((TABLE, "outcome"), row.outcome.as_deref())?,
            output: row.output,
            error: row.error,
            not_before: row.not_before,
            claims: to_u32((TABLE, "claims"), row.claims)?,
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
        const TABLE: &str = "job_items";
        sqlx::query!(
            "INSERT INTO job_items (id, job_id, seq, state, input, site, request_id, outcome,
                                    output, error, not_before, claims, created_at, started_at,
                                    finished_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
            item.id.as_uuid(),
            item.job_id.as_uuid(),
            to_i64((TABLE, "seq"), item.seq)?,
            item.state.as_str(),
            item.input,
            item.site,
            item.request_id.map(|id| id.as_uuid()),
            item.outcome.map(Outcome::as_str),
            item.output,
            item.error,
            item.not_before,
            to_i32((TABLE, "claims"), item.claims)?,
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
            "SELECT id, job_id, seq, state, input, site, request_id, outcome, output, error,
                    not_before, claims, created_at, started_at, finished_at
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

    /// Every item of a job that is not over, in position order: what its
    /// worker still has to do. An item that is `running` was being worked
    /// when its job changed hands.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn open(&self, job_id: JobId) -> Result<Vec<JobItem>, StoreError> {
        sqlx::query_as!(
            JobItemRow,
            "SELECT id, job_id, seq, state, input, site, request_id, outcome, output, error,
                    not_before, claims, created_at, started_at, finished_at
             FROM job_items
             WHERE job_id = $1 AND state IN ('queued', 'running')
             ORDER BY seq",
            job_id.as_uuid(),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(JobItem::try_from)
        .collect()
    }

    /// The item a request carried out, if it is one of a job's.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn find_by_request(
        &self,
        request_id: RequestId,
    ) -> Result<Option<JobItem>, StoreError> {
        sqlx::query_as!(
            JobItemRow,
            "SELECT id, job_id, seq, state, input, site, request_id, outcome, output, error,
                    not_before, claims, created_at, started_at, finished_at
             FROM job_items WHERE request_id = $1
             ORDER BY seq LIMIT 1",
            request_id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(JobItem::try_from)
        .transpose()
    }

    /// Marks an item as being worked under the request that carries it
    /// out, and counts the claim.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn start(
        &self,
        id: JobItemId,
        request_id: Option<RequestId>,
        at: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE job_items
             SET state = 'running', request_id = COALESCE($2, request_id),
                 started_at = COALESCE(started_at, $3), claims = claims + 1, not_before = NULL
             WHERE id = $1 AND state IN ('queued', 'running')",
            id.as_uuid(),
            request_id.map(|id| id.as_uuid()),
            at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Puts an item that could not be attempted back in the queue, not to
    /// be taken up before `not_before`.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn defer(
        &self,
        id: JobItemId,
        not_before: Option<DateTime<Utc>>,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE job_items SET state = 'queued', not_before = $2
             WHERE id = $1 AND state IN ('queued', 'running')",
            id.as_uuid(),
            not_before,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Writes back an item's state, request, outcome, output, error and
    /// timestamps. Returns whether the item exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn update(&self, item: &JobItem) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE job_items
             SET state = $2, request_id = $3, error = $4, started_at = $5, finished_at = $6,
                 outcome = $7, output = $8, not_before = $9
             WHERE id = $1",
            item.id.as_uuid(),
            item.state.as_str(),
            item.request_id.map(|id| id.as_uuid()),
            item.error,
            item.started_at,
            item.finished_at,
            item.outcome.map(Outcome::as_str),
            item.output,
            item.not_before,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Ends an item and moves its job's counters by it, in one
    /// transaction, so that the counters are always the items'. An item
    /// that had ended already changes nothing. Returns the job as it stands
    /// afterwards.
    ///
    /// # Errors
    ///
    /// Fails when a statement fails or a row does not parse.
    pub async fn finish(&self, item: &JobItem) -> Result<Option<Job>, StoreError> {
        let tally = Tally::of(item.state);
        let mut transaction = self.pool.begin().await?;
        let ended = sqlx::query!(
            "UPDATE job_items
             SET state = $2, request_id = $3, error = $4, started_at = $5, finished_at = $6,
                 outcome = $7, output = $8, not_before = NULL
             WHERE id = $1 AND state IN ('queued', 'running')",
            item.id.as_uuid(),
            item.state.as_str(),
            item.request_id.map(|id| id.as_uuid()),
            item.error,
            item.started_at,
            item.finished_at,
            item.outcome.map(Outcome::as_str),
            item.output,
        )
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;
        let (succeeded, failed, cancelled) = if ended {
            (
                to_i32(("jobs", "items_succeeded"), tally.succeeded)?,
                to_i32(("jobs", "items_failed"), tally.failed)?,
                to_i32(("jobs", "items_cancelled"), tally.cancelled)?,
            )
        } else {
            (0, 0, 0)
        };
        let job = sqlx::query_as!(
            JobRow,
            "UPDATE jobs
             SET items_succeeded = items_succeeded + $2, items_failed = items_failed + $3,
                 items_cancelled = items_cancelled + $4
             WHERE id = $1
             RETURNING id, api_key_id, kind, state, spec, idempotency_key, idempotency_hash,
                       idempotency_until, origin, webhook_url, webhook_secret, items_total,
                       items_succeeded, items_failed, items_cancelled, cancel_requested, error,
                       deadline, paused_until, paused_reason, created_at, started_at, finished_at",
            item.job_id.as_uuid(),
            succeeded,
            failed,
            cancelled,
        )
        .fetch_optional(&mut *transaction)
        .await?
        .map(Job::try_from)
        .transpose()?;
        transaction.commit().await?;
        Ok(job)
    }

    /// Ends every item of a job that waits in the queue as `state` with
    /// `error` (a cancel, or the job's deadline), and moves the job's
    /// counters by them. Items that are being worked are left to their
    /// worker. Returns the job as it stands afterwards.
    ///
    /// # Errors
    ///
    /// Fails when a statement fails, a row does not parse, or `state` is
    /// not an end.
    pub async fn end_queued(
        &self,
        job_id: JobId,
        state: JobItemState,
        error: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<Option<Job>, StoreError> {
        let mut transaction = self.pool.begin().await?;
        let ended = sqlx::query!(
            "UPDATE job_items SET state = $2, error = $3, finished_at = $4, not_before = NULL
             WHERE job_id = $1 AND state = 'queued'",
            job_id.as_uuid(),
            state.as_str(),
            error,
            at,
        )
        .execute(&mut *transaction)
        .await?
        .rows_affected();
        let ended = i32::try_from(ended).map_err(|_| StoreError::OutOfRange {
            table: "jobs",
            column: "items_total",
        })?;
        let tally = Tally::of(state);
        let job = sqlx::query_as!(
            JobRow,
            "UPDATE jobs
             SET items_succeeded = items_succeeded + $2, items_failed = items_failed + $3,
                 items_cancelled = items_cancelled + $4
             WHERE id = $1
             RETURNING id, api_key_id, kind, state, spec, idempotency_key, idempotency_hash,
                       idempotency_until, origin, webhook_url, webhook_secret, items_total,
                       items_succeeded, items_failed, items_cancelled, cancel_requested, error,
                       deadline, paused_until, paused_reason, created_at, started_at, finished_at",
            job_id.as_uuid(),
            if tally.succeeded == 1 { ended } else { 0 },
            if tally.failed == 1 { ended } else { 0 },
            if tally.cancelled == 1 { ended } else { 0 },
        )
        .fetch_optional(&mut *transaction)
        .await?
        .map(Job::try_from)
        .transpose()?;
        transaction.commit().await?;
        Ok(job)
    }

    /// How many items of all jobs wait and how many are being worked.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn backlog(&self) -> Result<(u64, u64), StoreError> {
        let row = sqlx::query!(
            r#"SELECT count(*) FILTER (WHERE state = 'queued') AS "queued!",
                      count(*) FILTER (WHERE state = 'running') AS "running!"
               FROM job_items WHERE state IN ('queued', 'running')"#,
        )
        .fetch_one(self.pool)
        .await?;
        Ok((
            to_u64(("job_items", "count"), row.queued)?,
            to_u64(("job_items", "count"), row.running)?,
        ))
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
