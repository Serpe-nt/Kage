//! The `api_keys`, `sessions` and `audit_events` tables.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use kage_core::domain::{
    ApiKey, AuditActor, AuditEvent, AuditOutcome, KeyLimits, Scope, Session, SessionId,
};
use kage_core::ids::{ApiKeyId, AuditEventId, PersonaId};
use uuid::Uuid;

use super::{page_limit, repository};
use crate::StoreError;
use crate::convert::{parse, to_i32, to_u32};

repository!(
    /// API keys. Only the hash of a key's secret is stored.
    ApiKeyRepo
);

struct ApiKeyRow {
    id: Uuid,
    name: String,
    secret_hash: Vec<u8>,
    scopes: Vec<String>,
    requests_per_minute: i32,
    max_concurrent: i32,
    daily_page_budget: Option<i32>,
    disabled_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

impl TryFrom<ApiKeyRow> for ApiKey {
    type Error = StoreError;

    fn try_from(row: ApiKeyRow) -> Result<Self, StoreError> {
        const TABLE: &str = "api_keys";
        let secret_hash: [u8; 32] =
            row.secret_hash
                .try_into()
                .map_err(|bytes: Vec<u8>| StoreError::Corrupt {
                    table: TABLE,
                    column: "secret_hash",
                    reason: format!("{} bytes instead of 32", bytes.len()),
                })?;
        let scopes = row
            .scopes
            .iter()
            .map(|scope| parse::<Scope>((TABLE, "scopes"), scope))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Self {
            id: ApiKeyId::from_uuid(row.id),
            name: row.name,
            secret_hash,
            scopes,
            limits: KeyLimits {
                requests_per_minute: to_u32(
                    (TABLE, "requests_per_minute"),
                    row.requests_per_minute,
                )?,
                max_concurrent: to_u32((TABLE, "max_concurrent"), row.max_concurrent)?,
                daily_page_budget: row
                    .daily_page_budget
                    .map(|budget| to_u32((TABLE, "daily_page_budget"), budget))
                    .transpose()?,
            },
            disabled_at: row.disabled_at,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
        })
    }
}

impl ApiKeyRepo<'_> {
    /// Stores a new key.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a limit is out of range.
    pub async fn insert(&self, key: &ApiKey) -> Result<(), StoreError> {
        const TABLE: &str = "api_keys";
        let scopes: Vec<String> = key
            .scopes
            .iter()
            .map(|scope| scope.as_str().to_owned())
            .collect();
        sqlx::query!(
            "INSERT INTO api_keys (id, name, secret_hash, scopes, requests_per_minute,
                                   max_concurrent, daily_page_budget, disabled_at, created_at,
                                   last_used_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            key.id.as_uuid(),
            key.name,
            key.secret_hash.as_slice(),
            &scopes,
            to_i32(
                (TABLE, "requests_per_minute"),
                key.limits.requests_per_minute
            )?,
            to_i32((TABLE, "max_concurrent"), key.limits.max_concurrent)?,
            key.limits
                .daily_page_budget
                .map(|budget| to_i32((TABLE, "daily_page_budget"), budget))
                .transpose()?,
            key.disabled_at,
            key.created_at,
            key.last_used_at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The key with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: ApiKeyId) -> Result<Option<ApiKey>, StoreError> {
        sqlx::query_as!(
            ApiKeyRow,
            "SELECT id, name, secret_hash, scopes, requests_per_minute, max_concurrent,
                    daily_page_budget, disabled_at, created_at, last_used_at
             FROM api_keys WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(ApiKey::try_from)
        .transpose()
    }

    /// Every key, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list(&self) -> Result<Vec<ApiKey>, StoreError> {
        sqlx::query_as!(
            ApiKeyRow,
            "SELECT id, name, secret_hash, scopes, requests_per_minute, max_concurrent,
                    daily_page_budget, disabled_at, created_at, last_used_at
             FROM api_keys ORDER BY id",
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(ApiKey::try_from)
        .collect()
    }

    /// Disables a key at `at`, or enables it again with `None`. Returns
    /// whether the key exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn set_disabled(
        &self,
        id: ApiKeyId,
        at: Option<DateTime<Utc>>,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE api_keys SET disabled_at = $2 WHERE id = $1",
            id.as_uuid(),
            at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Replaces a key's secret hash (rotation). Returns whether the key
    /// exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn set_secret_hash(
        &self,
        id: ApiKeyId,
        secret_hash: &[u8; 32],
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE api_keys SET secret_hash = $2 WHERE id = $1",
            id.as_uuid(),
            secret_hash.as_slice(),
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Records that the key authenticated a request at `at`.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn touch(&self, id: ApiKeyId, at: DateTime<Utc>) -> Result<(), StoreError> {
        sqlx::query!(
            "UPDATE api_keys SET last_used_at = $2 WHERE id = $1",
            id.as_uuid(),
            at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

repository!(
    /// Sessions: handles that pin a key's requests to one persona.
    SessionRepo
);

struct SessionRow {
    api_key_id: Uuid,
    id: String,
    persona_id: Uuid,
    created_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl TryFrom<SessionRow> for Session {
    type Error = StoreError;

    fn try_from(row: SessionRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: parse::<SessionId>(("sessions", "id"), &row.id)?,
            api_key_id: ApiKeyId::from_uuid(row.api_key_id),
            persona_id: PersonaId::from_uuid(row.persona_id),
            created_at: row.created_at,
            last_used_at: row.last_used_at,
            expires_at: row.expires_at,
        })
    }
}

impl SessionRepo<'_> {
    /// Stores a new session.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the key already has a session with this
    /// id.
    pub async fn insert(&self, session: &Session) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO sessions (api_key_id, id, persona_id, created_at, last_used_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
            session.api_key_id.as_uuid(),
            session.id.as_str(),
            session.persona_id.as_uuid(),
            session.created_at,
            session.last_used_at,
            session.expires_at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// A key's session with this id, expired or not.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(
        &self,
        api_key_id: ApiKeyId,
        id: &SessionId,
    ) -> Result<Option<Session>, StoreError> {
        sqlx::query_as!(
            SessionRow,
            "SELECT api_key_id, id, persona_id, created_at, last_used_at, expires_at
             FROM sessions WHERE api_key_id = $1 AND id = $2",
            api_key_id.as_uuid(),
            id.as_str(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(Session::try_from)
        .transpose()
    }

    /// Writes back a session's last use and expiry. Returns whether the
    /// session exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn touch(&self, session: &Session) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE sessions SET last_used_at = $3, expires_at = $4
             WHERE api_key_id = $1 AND id = $2",
            session.api_key_id.as_uuid(),
            session.id.as_str(),
            session.last_used_at,
            session.expires_at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Ends a session. Returns whether it existed.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn delete(&self, api_key_id: ApiKeyId, id: &SessionId) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "DELETE FROM sessions WHERE api_key_id = $1 AND id = $2",
            api_key_id.as_uuid(),
            id.as_str(),
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Removes every session that has expired by `now`. Returns how many.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn delete_expired(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let done = sqlx::query!("DELETE FROM sessions WHERE expires_at <= $1", now)
            .execute(self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

repository!(
    /// The audit trail: every admin API call and CLI mutation (spec §19).
    AuditRepo
);

struct AuditRow {
    id: Uuid,
    at: DateTime<Utc>,
    actor: String,
    api_key_id: Option<Uuid>,
    action: String,
    target: Option<String>,
    outcome: String,
    details: serde_json::Value,
}

impl TryFrom<AuditRow> for AuditEvent {
    type Error = StoreError;

    fn try_from(row: AuditRow) -> Result<Self, StoreError> {
        const TABLE: &str = "audit_events";
        let actor = match (row.actor.as_str(), row.api_key_id) {
            ("api_key", Some(id)) => AuditActor::ApiKey(ApiKeyId::from_uuid(id)),
            // The key was deleted afterwards; the event stays, attributed to
            // the system rather than to nobody.
            ("api_key", None) | ("system", _) => AuditActor::System,
            ("cli", _) => AuditActor::Cli,
            (other, _) => {
                return Err(StoreError::Corrupt {
                    table: TABLE,
                    column: "actor",
                    reason: format!("unknown actor {other:?}"),
                });
            }
        };
        Ok(Self {
            id: AuditEventId::from_uuid(row.id),
            at: row.at,
            actor,
            action: row.action,
            target: row.target,
            outcome: parse::<AuditOutcome>((TABLE, "outcome"), &row.outcome)?,
            details: row.details,
        })
    }
}

impl AuditRepo<'_> {
    /// Appends an event.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn insert(&self, event: &AuditEvent) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO audit_events (id, at, actor, api_key_id, action, target, outcome, details)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            event.id.as_uuid(),
            event.at,
            event.actor.kind(),
            event.actor.api_key_id().map(|id| id.as_uuid()),
            event.action,
            event.target,
            event.outcome.as_str(),
            event.details,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// A page of events, newest first. Pass the id of the last event of the
    /// previous page as `before` to continue.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list(
        &self,
        before: Option<AuditEventId>,
        limit: u32,
    ) -> Result<Vec<AuditEvent>, StoreError> {
        sqlx::query_as!(
            AuditRow,
            "SELECT id, at, actor, api_key_id, action, target, outcome, details
             FROM audit_events
             WHERE $1::uuid IS NULL OR id < $1
             ORDER BY id DESC
             LIMIT $2",
            before.map(|id| id.as_uuid()),
            page_limit(limit),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(AuditEvent::try_from)
        .collect()
    }
}
