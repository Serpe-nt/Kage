//! The `requests`, `attempts` and `results` tables.

use chrono::{DateTime, Utc};
use kage_core::domain::{Attempt, FetchRecord, RequestRecord, SessionId};
use kage_core::ids::{ApiKeyId, AttemptId, JobId, PersonaId, RequestId, ResultId};
use kage_core::outcome::Outcome;
use uuid::Uuid;

use super::repository;
use crate::StoreError;
use crate::convert::{
    from_json, from_json_opt, parse, parse_opt, to_i32, to_i64, to_json, to_json_opt, to_u16_opt,
    to_u32, to_u64,
};

repository!(
    /// Fetch requests as they were accepted.
    RequestRepo
);

struct RequestRow {
    id: Uuid,
    api_key_id: Option<Uuid>,
    job_id: Option<Uuid>,
    session_id: Option<String>,
    url: String,
    method: String,
    domain: String,
    render: String,
    spec: serde_json::Value,
    created_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    outcome: Option<String>,
}

impl TryFrom<RequestRow> for RequestRecord {
    type Error = StoreError;

    fn try_from(row: RequestRow) -> Result<Self, StoreError> {
        const TABLE: &str = "requests";
        Ok(Self {
            id: RequestId::from_uuid(row.id),
            api_key_id: row.api_key_id.map(ApiKeyId::from_uuid),
            job_id: row.job_id.map(JobId::from_uuid),
            session_id: parse_opt::<SessionId>((TABLE, "session_id"), row.session_id.as_deref())?,
            url: row.url,
            method: row.method,
            domain: row.domain,
            render: parse((TABLE, "render"), &row.render)?,
            spec: row.spec,
            created_at: row.created_at,
            finished_at: row.finished_at,
            outcome: parse_opt::<Outcome>((TABLE, "outcome"), row.outcome.as_deref())?,
        })
    }
}

impl RequestRepo<'_> {
    /// Stores a new request.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn insert(&self, request: &RequestRecord) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO requests (id, api_key_id, job_id, session_id, url, method, domain, render,
                                   spec, created_at, finished_at, outcome)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            request.id.as_uuid(),
            request.api_key_id.map(|id| id.as_uuid()),
            request.job_id.map(|id| id.as_uuid()),
            request.session_id.as_ref().map(SessionId::as_str),
            request.url,
            request.method,
            request.domain,
            request.render.as_str(),
            request.spec,
            request.created_at,
            request.finished_at,
            request.outcome.map(Outcome::as_str),
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The request with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: RequestId) -> Result<Option<RequestRecord>, StoreError> {
        sqlx::query_as!(
            RequestRow,
            "SELECT id, api_key_id, job_id, session_id, url, method, domain, render, spec,
                    created_at, finished_at, outcome
             FROM requests WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(RequestRecord::try_from)
        .transpose()
    }

    /// How many requests a key has made since `since` that were not served
    /// from the cache: what its daily page budget is counted against. A
    /// page that was not fetched is not a page of the budget (spec §15).
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn count_for_key_since(
        &self,
        api_key_id: ApiKeyId,
        since: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM requests
               WHERE api_key_id = $1 AND created_at >= $2
                 AND NOT EXISTS (SELECT 1 FROM results
                                 WHERE results.request_id = requests.id AND results.cached)"#,
            api_key_id.as_uuid(),
            since,
        )
        .fetch_one(self.pool)
        .await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    /// Records the final outcome of a request. Returns whether the request
    /// exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn finish(
        &self,
        id: RequestId,
        outcome: Outcome,
        at: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE requests SET outcome = $2, finished_at = $3 WHERE id = $1",
            id.as_uuid(),
            outcome.as_str(),
            at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Makes a request that was accepted by itself part of a job: a
    /// synchronous fetch that outlasted its wall time and is carried on as
    /// one (spec §8.4). Returns whether the request exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn adopt(&self, id: RequestId, job_id: JobId) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE requests SET job_id = $2 WHERE id = $1",
            id.as_uuid(),
            job_id.as_uuid(),
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Deletes the requests that were accepted before `before`, of one key
    /// or of all, with their attempts, their results and the rows of what
    /// they kept: those that are no job's, and with `of_jobs` also those
    /// of jobs that have not been deleted (a job that goes takes its
    /// requests along). A request that has not ended is left unless it is
    /// a day older than `before`, and the request an open item of a job
    /// waits on is always left: both are still somebody's work. The
    /// artifact files go with the artifact store's next sweep of orphans.
    /// Returns how many requests and how many artifact rows were deleted.
    ///
    /// # Errors
    ///
    /// Fails when a statement fails.
    pub async fn delete_before(
        &self,
        before: DateTime<Utc>,
        api_key_id: Option<ApiKeyId>,
        of_jobs: bool,
    ) -> Result<(u64, u64), StoreError> {
        let key = api_key_id.map(|id| id.as_uuid());
        let (mut requests, mut artifacts) = (0, 0);
        loop {
            let mut transaction = self.pool.begin().await?;
            let ids: Vec<Uuid> = sqlx::query_scalar!(
                "SELECT id FROM requests
                 WHERE created_at < $1
                   AND ($2::uuid IS NULL OR api_key_id = $2)
                   AND ($3 OR job_id IS NULL)
                   AND (finished_at IS NOT NULL OR created_at < $1 - interval '1 day')
                   AND NOT EXISTS (
                       SELECT 1 FROM job_items
                       WHERE job_items.request_id = requests.id
                         AND job_items.state IN ('queued', 'running'))
                 ORDER BY created_at
                 LIMIT 500
                 FOR UPDATE SKIP LOCKED",
                before,
                key,
                of_jobs,
            )
            .fetch_all(&mut *transaction)
            .await?;
            if ids.is_empty() {
                transaction.rollback().await?;
                return Ok((requests, artifacts));
            }
            // Artifact rows point at their request with SET NULL: they are
            // removed by name, or nobody could read or purge them later.
            artifacts += sqlx::query!("DELETE FROM artifacts WHERE request_id = ANY($1)", &ids)
                .execute(&mut *transaction)
                .await?
                .rows_affected();
            requests += sqlx::query!("DELETE FROM requests WHERE id = ANY($1)", &ids)
                .execute(&mut *transaction)
                .await?
                .rows_affected();
            transaction.commit().await?;
        }
    }
}

repository!(
    /// Attempts: each try at a request with one persona.
    AttemptRepo
);

struct AttemptRow {
    id: Uuid,
    request_id: Uuid,
    n: i32,
    persona_id: Uuid,
    engine: String,
    render: String,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: Option<i32>,
    final_url: Option<String>,
    outcome: String,
    block: Option<serde_json::Value>,
    timing: serde_json::Value,
    seed: Option<String>,
    reason: Option<String>,
    patience_ms: Option<i64>,
}

impl TryFrom<AttemptRow> for Attempt {
    type Error = StoreError;

    fn try_from(row: AttemptRow) -> Result<Self, StoreError> {
        const TABLE: &str = "attempts";
        Ok(Self {
            id: AttemptId::from_uuid(row.id),
            request_id: RequestId::from_uuid(row.request_id),
            n: to_u32((TABLE, "n"), row.n)?,
            persona_id: PersonaId::from_uuid(row.persona_id),
            engine: parse((TABLE, "engine"), &row.engine)?,
            render: parse((TABLE, "render"), &row.render)?,
            started_at: row.started_at,
            finished_at: row.finished_at,
            status: to_u16_opt((TABLE, "status"), row.status)?,
            final_url: row.final_url,
            outcome: parse((TABLE, "outcome"), &row.outcome)?,
            block: from_json_opt((TABLE, "block"), row.block)?,
            timing: from_json((TABLE, "timing"), row.timing)?,
            seed: parse_opt((TABLE, "seed"), row.seed.as_deref())?,
            reason: row.reason,
            patience_ms: row
                .patience_ms
                .map(|millis| to_u64((TABLE, "patience_ms"), millis))
                .transpose()?,
        })
    }
}

impl AttemptRepo<'_> {
    /// Stores a finished attempt.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the request already has an attempt with
    /// this number.
    pub async fn insert(&self, attempt: &Attempt) -> Result<(), StoreError> {
        const TABLE: &str = "attempts";
        sqlx::query!(
            "INSERT INTO attempts (id, request_id, n, persona_id, engine, render, started_at,
                                   finished_at, status, final_url, outcome, block, timing, seed,
                                   reason, patience_ms)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
            attempt.id.as_uuid(),
            attempt.request_id.as_uuid(),
            to_i32((TABLE, "n"), attempt.n)?,
            attempt.persona_id.as_uuid(),
            attempt.engine.as_str(),
            attempt.render.as_str(),
            attempt.started_at,
            attempt.finished_at,
            attempt.status.map(i32::from),
            attempt.final_url,
            attempt.outcome.as_str(),
            to_json_opt((TABLE, "block"), attempt.block.as_ref())?,
            to_json((TABLE, "timing"), &attempt.timing)?,
            attempt.seed.map(|seed| seed.to_string()),
            attempt.reason,
            attempt
                .patience_ms
                .map(|millis| to_i64((TABLE, "patience_ms"), millis))
                .transpose()?,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Every attempt of a request, in order.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_for_request(
        &self,
        request_id: RequestId,
    ) -> Result<Vec<Attempt>, StoreError> {
        sqlx::query_as!(
            AttemptRow,
            "SELECT id, request_id, n, persona_id, engine, render, started_at, finished_at, status,
                    final_url, outcome, block, timing, seed, reason, patience_ms
             FROM attempts WHERE request_id = $1 ORDER BY n",
            request_id.as_uuid(),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Attempt::try_from)
        .collect()
    }
}

repository!(
    /// Stored results, one per request.
    ResultRepo
);

struct ResultRow {
    id: Uuid,
    request_id: Uuid,
    outcome: String,
    status: Option<i32>,
    headers: serde_json::Value,
    final_url: Option<String>,
    redirects: Vec<String>,
    extract: Option<serde_json::Value>,
    xhr: Option<serde_json::Value>,
    artifacts: serde_json::Value,
    persona: Option<serde_json::Value>,
    block: Option<serde_json::Value>,
    robots: Option<serde_json::Value>,
    cached: bool,
    cached_at: Option<DateTime<Utc>>,
    reason: Option<String>,
    visit: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
}

impl TryFrom<ResultRow> for FetchRecord {
    type Error = StoreError;

    fn try_from(row: ResultRow) -> Result<Self, StoreError> {
        const TABLE: &str = "results";
        Ok(Self {
            id: ResultId::from_uuid(row.id),
            request_id: RequestId::from_uuid(row.request_id),
            outcome: parse((TABLE, "outcome"), &row.outcome)?,
            status: to_u16_opt((TABLE, "status"), row.status)?,
            headers: from_json((TABLE, "headers"), row.headers)?,
            final_url: row.final_url,
            redirects: row.redirects,
            extract: row.extract,
            xhr: row.xhr,
            artifacts: from_json((TABLE, "artifacts"), row.artifacts)?,
            persona: from_json_opt((TABLE, "persona"), row.persona)?,
            block: from_json_opt((TABLE, "block"), row.block)?,
            robots: from_json_opt((TABLE, "robots"), row.robots)?,
            cached: row.cached,
            cached_at: row.cached_at,
            reason: row.reason,
            // A row stored before the column, or one that keeps nothing.
            visit: from_json_opt((TABLE, "visit"), row.visit)?.unwrap_or_default(),
            created_at: row.created_at,
        })
    }
}

impl ResultRepo<'_> {
    /// Stores the result of a request.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the request already has a result.
    pub async fn insert(&self, record: &FetchRecord) -> Result<(), StoreError> {
        const TABLE: &str = "results";
        sqlx::query!(
            "INSERT INTO results (id, request_id, outcome, status, headers, final_url, redirects,
                                  extract, xhr, artifacts, persona, block, robots, cached,
                                  cached_at, reason, visit, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16,
                     $17, $18)",
            record.id.as_uuid(),
            record.request_id.as_uuid(),
            record.outcome.as_str(),
            record.status.map(i32::from),
            to_json((TABLE, "headers"), &record.headers)?,
            record.final_url,
            &record.redirects,
            record.extract,
            record.xhr,
            to_json((TABLE, "artifacts"), &record.artifacts)?,
            to_json_opt((TABLE, "persona"), record.persona.as_ref())?,
            to_json_opt((TABLE, "block"), record.block.as_ref())?,
            to_json_opt((TABLE, "robots"), record.robots.as_ref())?,
            record.cached,
            record.cached_at,
            record.reason,
            to_json_opt(
                (TABLE, "visit"),
                Some(&record.visit).filter(|visit| !visit.is_empty()),
            )?,
            record.created_at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The result with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: ResultId) -> Result<Option<FetchRecord>, StoreError> {
        sqlx::query_as!(
            ResultRow,
            "SELECT id, request_id, outcome, status, headers, final_url, redirects, extract, xhr,
                    artifacts, persona, block, robots, cached, cached_at, reason, visit,
                    created_at
             FROM results WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(FetchRecord::try_from)
        .transpose()
    }

    /// The result of this request.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get_by_request(
        &self,
        request_id: RequestId,
    ) -> Result<Option<FetchRecord>, StoreError> {
        sqlx::query_as!(
            ResultRow,
            "SELECT id, request_id, outcome, status, headers, final_url, redirects, extract, xhr,
                    artifacts, persona, block, robots, cached, cached_at, reason, visit,
                    created_at
             FROM results WHERE request_id = $1",
            request_id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(FetchRecord::try_from)
        .transpose()
    }
}
