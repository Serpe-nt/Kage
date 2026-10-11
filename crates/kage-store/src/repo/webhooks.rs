//! The `webhook_deliveries` and `webhook_attempts` tables (spec §16.6).
//!
//! A delivery is one event of a job on its way to the job's webhook. It is
//! `pending` with the time its next attempt is due until the receiver took
//! it, every attempt failed, or a later event of the job made it pointless.
//! The sender claims a due delivery with `SELECT ... FOR UPDATE SKIP
//! LOCKED` under a short lease, so a delivery that was being attempted when
//! the server stopped is due again after a restart. Every attempt is a row
//! of the delivery log.

use chrono::{DateTime, Utc};
use kage_core::domain::{DeliveryState, WebhookAttempt, WebhookDelivery, WebhookEvent};
use kage_core::ids::{JobId, WebhookDeliveryId};
use uuid::Uuid;

use super::{page_limit, repository};
use crate::StoreError;
use crate::convert::{parse, to_i32, to_u16_opt, to_u32, to_u64};

repository!(
    /// Webhook deliveries and the log of their attempts.
    WebhookRepo
);

struct DeliveryRow {
    id: Uuid,
    job_id: Uuid,
    event: String,
    origin: String,
    body: String,
    state: String,
    attempts: i32,
    next_attempt_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<DeliveryRow> for WebhookDelivery {
    type Error = StoreError;

    fn try_from(row: DeliveryRow) -> Result<Self, StoreError> {
        const TABLE: &str = "webhook_deliveries";
        Ok(Self {
            id: WebhookDeliveryId::from_uuid(row.id),
            job_id: JobId::from_uuid(row.job_id),
            event: parse((TABLE, "event"), &row.event)?,
            origin: row.origin,
            body: row.body,
            state: parse((TABLE, "state"), &row.state)?,
            attempts: to_u32((TABLE, "attempts"), row.attempts)?,
            next_attempt_at: row.next_attempt_at,
            created_at: row.created_at,
            finished_at: row.finished_at,
        })
    }
}

/// What one attempt at a delivery came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptEnd {
    /// The receiver took the event.
    Delivered,
    /// It did not; the next attempt is due then.
    Retry(DateTime<Utc>),
    /// It did not, and no attempt is left, or the receiver is gone.
    Failed,
}

impl WebhookRepo<'_> {
    /// Stores a new event for delivery. A `job.progress` event of the same
    /// job that still waits and is not being attempted is overtaken by it:
    /// the receiver is told the newer figure, not both.
    ///
    /// # Errors
    ///
    /// Fails when a statement fails.
    pub async fn enqueue(&self, delivery: &WebhookDelivery) -> Result<(), StoreError> {
        const TABLE: &str = "webhook_deliveries";
        let mut transaction = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE webhook_deliveries
             SET state = 'superseded', next_attempt_at = NULL, finished_at = $2
             WHERE job_id = $1 AND event = 'job.progress' AND state = 'pending'
               AND (lease_until IS NULL OR lease_until <= $2)",
            delivery.job_id.as_uuid(),
            delivery.created_at,
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query!(
            "INSERT INTO webhook_deliveries (id, job_id, event, origin, body, state, attempts,
                                             next_attempt_at, created_at, finished_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            delivery.id.as_uuid(),
            delivery.job_id.as_uuid(),
            delivery.event.as_str(),
            delivery.origin,
            delivery.body,
            delivery.state.as_str(),
            to_i32((TABLE, "attempts"), delivery.attempts)?,
            delivery.next_attempt_at,
            delivery.created_at,
            delivery.finished_at,
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// The delivery with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: WebhookDeliveryId) -> Result<Option<WebhookDelivery>, StoreError> {
        sqlx::query_as!(
            DeliveryRow,
            "SELECT id, job_id, event, origin, body, state, attempts, next_attempt_at,
                    created_at, finished_at
             FROM webhook_deliveries WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(WebhookDelivery::try_from)
        .transpose()
    }

    /// Claims the oldest delivery that is due at `now` and whose receiver is
    /// none of `busy` (the origins a delivery is in flight to, or that are
    /// left alone after a failure), under a lease until `lease_until`. A
    /// row another sender is claiming at this moment is skipped.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn claim_due(
        &self,
        now: DateTime<Utc>,
        lease_until: DateTime<Utc>,
        busy: &[String],
    ) -> Result<Option<WebhookDelivery>, StoreError> {
        sqlx::query_as!(
            DeliveryRow,
            "UPDATE webhook_deliveries SET lease_until = $2
             WHERE id = (
                 SELECT id FROM webhook_deliveries
                 WHERE state = 'pending' AND next_attempt_at <= $1
                   AND (lease_until IS NULL OR lease_until <= $1)
                   AND origin <> ALL($3)
                 ORDER BY created_at, id
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED)
             RETURNING id, job_id, event, origin, body, state, attempts, next_attempt_at,
                       created_at, finished_at",
            now,
            lease_until,
            busy,
        )
        .fetch_optional(self.pool)
        .await?
        .map(WebhookDelivery::try_from)
        .transpose()
    }

    /// Gives a claimed delivery back without an attempt: its receiver is
    /// left alone for now.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn unclaim(&self, id: WebhookDeliveryId) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE webhook_deliveries SET lease_until = NULL WHERE id = $1",
            id.as_uuid(),
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Writes one attempt into the delivery log and moves the delivery by
    /// it, in one transaction.
    ///
    /// # Errors
    ///
    /// Fails when a statement fails or a number is out of range.
    pub async fn record(
        &self,
        attempt: &WebhookAttempt,
        end: AttemptEnd,
    ) -> Result<(), StoreError> {
        const TABLE: &str = "webhook_attempts";
        let ended_at =
            attempt.started_at + chrono::Duration::milliseconds(i64::from(attempt.duration_ms));
        let (state, next, finished) = match end {
            AttemptEnd::Delivered => (DeliveryState::Delivered, None, Some(ended_at)),
            AttemptEnd::Retry(at) => (DeliveryState::Pending, Some(at), None),
            AttemptEnd::Failed => (DeliveryState::Failed, None, Some(ended_at)),
        };
        let mut transaction = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO webhook_attempts (delivery_id, n, started_at, duration_ms, status, error)
             VALUES ($1, $2, $3, $4, $5, $6)",
            attempt.delivery_id.as_uuid(),
            to_i32((TABLE, "n"), attempt.n)?,
            attempt.started_at,
            to_i32((TABLE, "duration_ms"), attempt.duration_ms)?,
            attempt.status.map(i32::from),
            attempt.error,
        )
        .execute(&mut *transaction)
        .await?;
        sqlx::query!(
            "UPDATE webhook_deliveries
             SET attempts = $2, state = $3, next_attempt_at = $4, finished_at = $5,
                 lease_until = NULL
             WHERE id = $1",
            attempt.delivery_id.as_uuid(),
            to_i32((TABLE, "n"), attempt.n)?,
            state.as_str(),
            next,
            finished,
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// The newest deliveries of a job, newest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_for_job(
        &self,
        job_id: JobId,
        limit: u32,
    ) -> Result<Vec<WebhookDelivery>, StoreError> {
        sqlx::query_as!(
            DeliveryRow,
            "SELECT id, job_id, event, origin, body, state, attempts, next_attempt_at,
                    created_at, finished_at
             FROM webhook_deliveries WHERE job_id = $1
             ORDER BY created_at DESC, id DESC
             LIMIT $2",
            job_id.as_uuid(),
            page_limit(limit),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(WebhookDelivery::try_from)
        .collect()
    }

    /// The attempts at one delivery, in order.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn attempts(
        &self,
        delivery_id: WebhookDeliveryId,
    ) -> Result<Vec<WebhookAttempt>, StoreError> {
        const TABLE: &str = "webhook_attempts";
        sqlx::query!(
            "SELECT delivery_id, n, started_at, duration_ms, status, error
             FROM webhook_attempts WHERE delivery_id = $1 ORDER BY n",
            delivery_id.as_uuid(),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok(WebhookAttempt {
                delivery_id: WebhookDeliveryId::from_uuid(row.delivery_id),
                n: to_u32((TABLE, "n"), row.n)?,
                started_at: row.started_at,
                duration_ms: to_u32((TABLE, "duration_ms"), row.duration_ms)?,
                status: to_u16_opt((TABLE, "status"), row.status)?,
                error: row.error,
            })
        })
        .collect()
    }

    /// How many deliveries wait, and when the earliest of them is due.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn pending(&self) -> Result<(u64, Option<DateTime<Utc>>), StoreError> {
        let row = sqlx::query!(
            r#"SELECT count(*) AS "pending!", min(next_attempt_at) AS due
               FROM webhook_deliveries WHERE state = 'pending'"#,
        )
        .fetch_one(self.pool)
        .await?;
        Ok((
            to_u64(("webhook_deliveries", "count"), row.pending)?,
            row.due,
        ))
    }

    /// Which events a job has for delivery or delivered: for a worker that
    /// took a job up again and must not report an end twice.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn events_of(&self, job_id: JobId) -> Result<Vec<WebhookEvent>, StoreError> {
        sqlx::query_scalar!(
            "SELECT DISTINCT event FROM webhook_deliveries WHERE job_id = $1",
            job_id.as_uuid(),
        )
        .fetch_all(self.pool)
        .await?
        .iter()
        .map(|event| parse(("webhook_deliveries", "event"), event))
        .collect()
    }
}
