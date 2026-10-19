//! The `proxies` table.

use chrono::{DateTime, Utc};
use kage_core::domain::{HealthEvent, Proxy, ProxyProviderKind, ProxyReplacement, ProxyState};
use kage_core::ids::ProxyId;
use kage_core::secret::SealedSecret;
use sqlx::PgConnection;
use uuid::Uuid;

use super::events::insert_event;
use super::repository;
use crate::StoreError;
use crate::convert::{from_json_opt, parse, parse_opt, to_i32, to_json_opt, to_u16, to_u32};

repository!(
    /// Proxies: one static residential IP with credentials each.
    ProxyRepo
);

/// Key of the transaction-scoped advisory lock that serializes every
/// decision about the replacement budget. The bytes spell `kagerepl`.
const REPLACEMENT_LOCK: i64 = 0x6b61_6765_7265_706c;

struct ProxyRow {
    id: Uuid,
    provider: String,
    provider_ref: String,
    host: String,
    port: i32,
    username: Vec<u8>,
    password: Vec<u8>,
    scheme: String,
    country: Option<String>,
    city: Option<String>,
    asn: Option<String>,
    state: String,
    consecutive_failures: i32,
    backoff_count: i32,
    backoff_until: Option<DateTime<Utc>>,
    failing_since: Option<DateTime<Utc>>,
    replacement_requested_at: Option<DateTime<Utc>>,
    replacement_reason: Option<String>,
    replacement_ticket: Option<String>,
    last_health: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
    retired_at: Option<DateTime<Utc>>,
}

impl TryFrom<ProxyRow> for Proxy {
    type Error = StoreError;

    fn try_from(row: ProxyRow) -> Result<Self, StoreError> {
        // The schema keeps the request time and the reason together.
        let replacement = match (row.replacement_requested_at, row.replacement_reason) {
            (Some(requested_at), Some(reason)) => Some(ProxyReplacement {
                requested_at,
                reason: parse(("proxies", "replacement_reason"), &reason)?,
                ticket: row.replacement_ticket,
            }),
            _ => None,
        };
        Ok(Self {
            id: ProxyId::from_uuid(row.id),
            provider: parse(("proxies", "provider"), &row.provider)?,
            provider_ref: row.provider_ref,
            host: row.host,
            port: to_u16(("proxies", "port"), row.port)?,
            username: SealedSecret::new(row.username),
            password: SealedSecret::new(row.password),
            scheme: parse(("proxies", "scheme"), &row.scheme)?,
            country: parse_opt(("proxies", "country"), row.country.as_deref())?,
            city: row.city,
            asn: row.asn,
            state: parse(("proxies", "state"), &row.state)?,
            consecutive_failures: to_u32(
                ("proxies", "consecutive_failures"),
                row.consecutive_failures,
            )?,
            backoff_count: to_u32(("proxies", "backoff_count"), row.backoff_count)?,
            backoff_until: row.backoff_until,
            failing_since: row.failing_since,
            replacement,
            last_health: from_json_opt(("proxies", "last_health"), row.last_health)?,
            created_at: row.created_at,
            retired_at: row.retired_at,
        })
    }
}

impl ProxyRepo<'_> {
    /// Stores a new proxy.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the provider already has a live proxy
    /// with this `provider_ref`.
    pub async fn insert(&self, proxy: &Proxy) -> Result<(), StoreError> {
        self.insert_noting(proxy, &[]).await
    }

    /// Stores a new proxy and, in the same transaction, the health events
    /// that record its arrival, so that nobody reads the proxy without them
    /// (follow-up F65).
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the provider already has a live proxy
    /// with this `provider_ref`; nothing is written then.
    pub async fn insert_noting(
        &self,
        proxy: &Proxy,
        events: &[HealthEvent],
    ) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await?;
        insert_row(&mut transaction, proxy).await?;
        for event in events {
            insert_event(&mut transaction, event).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// The proxy with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: ProxyId) -> Result<Option<Proxy>, StoreError> {
        sqlx::query_as!(
            ProxyRow,
            "SELECT id, provider, provider_ref, host, port, username, password, scheme, country,
                    city, asn, state, consecutive_failures, backoff_count, backoff_until,
                    failing_since, replacement_requested_at, replacement_reason,
                    replacement_ticket, last_health, created_at, retired_at
             FROM proxies WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(Proxy::try_from)
        .transpose()
    }

    /// Every proxy, or only those in `state`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list(&self, state: Option<ProxyState>) -> Result<Vec<Proxy>, StoreError> {
        sqlx::query_as!(
            ProxyRow,
            "SELECT id, provider, provider_ref, host, port, username, password, scheme, country,
                    city, asn, state, consecutive_failures, backoff_count, backoff_until,
                    failing_since, replacement_requested_at, replacement_reason,
                    replacement_ticket, last_health, created_at, retired_at
             FROM proxies
             WHERE $1::text IS NULL OR state = $1
             ORDER BY id",
            state.map(ProxyState::as_str),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Proxy::try_from)
        .collect()
    }

    /// Every proxy that still belongs to the pool (any state but `retired`),
    /// oldest first, optionally of one provider only.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_live(
        &self,
        provider: Option<ProxyProviderKind>,
    ) -> Result<Vec<Proxy>, StoreError> {
        sqlx::query_as!(
            ProxyRow,
            "SELECT id, provider, provider_ref, host, port, username, password, scheme, country,
                    city, asn, state, consecutive_failures, backoff_count, backoff_until,
                    failing_since, replacement_requested_at, replacement_reason,
                    replacement_ticket, last_health, created_at, retired_at
             FROM proxies
             WHERE state <> 'retired' AND ($1::text IS NULL OR provider = $1)
             ORDER BY id",
            provider.map(ProxyProviderKind::as_str),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Proxy::try_from)
        .collect()
    }

    /// Writes back everything about a proxy that changes during its life:
    /// credentials, location, pool state and health. Returns whether the
    /// proxy exists.
    ///
    /// Two writers that both read, change and `update` can overwrite each
    /// other; use [`ProxyRepo::modify`] for a change that depends on what
    /// is stored.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn update(&self, proxy: &Proxy) -> Result<bool, StoreError> {
        let mut connection = self.pool.acquire().await?;
        write(&mut connection, proxy).await
    }

    /// Reads one proxy, lets `change` alter it, and writes it back, all in
    /// one transaction that holds the row lock. Concurrent callers, in this
    /// process or another (the CLI next to the server), therefore apply
    /// their changes one after the other, each to what the one before left.
    ///
    /// Returns `None` when the proxy does not exist; `change` is not called
    /// then.
    ///
    /// # Errors
    ///
    /// Fails when a query fails or the row does not parse; nothing is
    /// written in that case.
    pub async fn modify<T>(
        &self,
        id: ProxyId,
        change: impl FnOnce(&mut Proxy) -> T,
    ) -> Result<Option<T>, StoreError> {
        self.modify_noting(id, |proxy| (change(proxy), Vec::new()))
            .await
    }

    /// Like [`ProxyRepo::modify`], and `change` also names the health
    /// events that record what it did: they are written in the same
    /// transaction as the proxy, so that nobody reads a proxy in a state
    /// whose event is not there yet (follow-up F65).
    ///
    /// Returns `None` when the proxy does not exist; `change` is not called
    /// then.
    ///
    /// # Errors
    ///
    /// Fails when a query fails or the row does not parse; nothing is
    /// written in that case.
    pub async fn modify_noting<T>(
        &self,
        id: ProxyId,
        change: impl FnOnce(&mut Proxy) -> (T, Vec<HealthEvent>),
    ) -> Result<Option<T>, StoreError> {
        let mut transaction = self.pool.begin().await?;
        let Some(mut proxy) = lock_row(&mut transaction, id).await? else {
            return Ok(None);
        };
        let (result, events) = change(&mut proxy);
        write(&mut transaction, &proxy).await?;
        for event in &events {
            insert_event(&mut transaction, event).await?;
        }
        transaction.commit().await?;
        Ok(Some(result))
    }

    /// Like [`ProxyRepo::modify_noting`], for the one decision that spends
    /// the replacement budget: `change` also receives the number of
    /// replacements requested since `since`, and every caller of this
    /// method is serialized by one advisory lock. Two callers can therefore
    /// never both see the last free slot. The events `change` names are
    /// written with the decision.
    ///
    /// # Errors
    ///
    /// Fails when a query fails or the row does not parse; nothing is
    /// written in that case.
    pub async fn modify_counting_replacements<T>(
        &self,
        id: ProxyId,
        since: DateTime<Utc>,
        change: impl FnOnce(&mut Proxy, u32) -> (T, Vec<HealthEvent>),
    ) -> Result<Option<T>, StoreError> {
        let mut transaction = self.pool.begin().await?;
        // The function returns `void`, which the checked macros cannot decode.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(REPLACEMENT_LOCK)
            .execute(&mut *transaction)
            .await?;
        let used = count_replacements(&mut transaction, since).await?;
        let Some(mut proxy) = lock_row(&mut transaction, id).await? else {
            return Ok(None);
        };
        let (result, events) = change(&mut proxy, used);
        write(&mut transaction, &proxy).await?;
        for event in &events {
            insert_event(&mut transaction, event).await?;
        }
        transaction.commit().await?;
        Ok(Some(result))
    }

    /// How many replacements were requested at or after `since`. Retired
    /// proxies count: a completed replacement is a retired row.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn replacements_since(&self, since: DateTime<Utc>) -> Result<u32, StoreError> {
        let mut connection = self.pool.acquire().await?;
        count_replacements(&mut connection, since).await
    }

    /// Whether any proxy, retired ones included, is recorded as having let
    /// the host's own address through. This is the pool's `egress_unsafe`
    /// flag: while it is set nothing may be fetched.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn any_leak(&self) -> Result<bool, StoreError> {
        let row = sqlx::query!(
            r#"SELECT EXISTS (
                   SELECT 1 FROM proxies WHERE last_health @> '{"leak_detected": true}'::jsonb
               ) AS "leaking!""#,
        )
        .fetch_one(self.pool)
        .await?;
        Ok(row.leaking)
    }
}

/// Reads a proxy and locks its row until the transaction ends.
async fn lock_row(connection: &mut PgConnection, id: ProxyId) -> Result<Option<Proxy>, StoreError> {
    sqlx::query_as!(
        ProxyRow,
        "SELECT id, provider, provider_ref, host, port, username, password, scheme, country,
                city, asn, state, consecutive_failures, backoff_count, backoff_until,
                failing_since, replacement_requested_at, replacement_reason,
                replacement_ticket, last_health, created_at, retired_at
         FROM proxies WHERE id = $1 FOR UPDATE",
        id.as_uuid(),
    )
    .fetch_optional(connection)
    .await?
    .map(Proxy::try_from)
    .transpose()
}

/// Writes every mutable column of a proxy; returns whether the row exists.
async fn write(connection: &mut PgConnection, proxy: &Proxy) -> Result<bool, StoreError> {
    let replacement = proxy.replacement.as_ref();
    let done = sqlx::query!(
        "UPDATE proxies
         SET host = $2, port = $3, username = $4, password = $5, country = $6, city = $7,
             asn = $8, state = $9, consecutive_failures = $10, backoff_count = $11,
             backoff_until = $12, failing_since = $13, replacement_requested_at = $14,
             replacement_reason = $15, replacement_ticket = $16, last_health = $17,
             retired_at = $18
         WHERE id = $1",
        proxy.id.as_uuid(),
        proxy.host,
        i32::from(proxy.port),
        proxy.username.as_bytes(),
        proxy.password.as_bytes(),
        proxy.country.map(|country| country.as_str().to_owned()),
        proxy.city,
        proxy.asn,
        proxy.state.as_str(),
        to_i32(
            ("proxies", "consecutive_failures"),
            proxy.consecutive_failures
        )?,
        to_i32(("proxies", "backoff_count"), proxy.backoff_count)?,
        proxy.backoff_until,
        proxy.failing_since,
        replacement.map(|replacement| replacement.requested_at),
        replacement.map(|replacement| replacement.reason.as_str()),
        replacement.and_then(|replacement| replacement.ticket.as_deref()),
        to_json_opt(("proxies", "last_health"), proxy.last_health.as_ref())?,
        proxy.retired_at,
    )
    .execute(connection)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Writes a new proxy row.
async fn insert_row(connection: &mut PgConnection, proxy: &Proxy) -> Result<(), StoreError> {
    let replacement = proxy.replacement.as_ref();
    sqlx::query!(
        "INSERT INTO proxies (id, provider, provider_ref, host, port, username, password,
                                  scheme, country, city, asn, state, consecutive_failures,
                                  backoff_count, backoff_until, failing_since,
                                  replacement_requested_at, replacement_reason,
                                  replacement_ticket, last_health, created_at, retired_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                     $18, $19, $20, $21, $22)",
        proxy.id.as_uuid(),
        proxy.provider.as_str(),
        proxy.provider_ref,
        proxy.host,
        i32::from(proxy.port),
        proxy.username.as_bytes(),
        proxy.password.as_bytes(),
        proxy.scheme.as_str(),
        proxy.country.map(|country| country.as_str().to_owned()),
        proxy.city,
        proxy.asn,
        proxy.state.as_str(),
        to_i32(
            ("proxies", "consecutive_failures"),
            proxy.consecutive_failures
        )?,
        to_i32(("proxies", "backoff_count"), proxy.backoff_count)?,
        proxy.backoff_until,
        proxy.failing_since,
        replacement.map(|replacement| replacement.requested_at),
        replacement.map(|replacement| replacement.reason.as_str()),
        replacement.and_then(|replacement| replacement.ticket.as_deref()),
        to_json_opt(("proxies", "last_health"), proxy.last_health.as_ref())?,
        proxy.created_at,
        proxy.retired_at,
    )
    .execute(connection)
    .await?;
    Ok(())
}

async fn count_replacements(
    connection: &mut PgConnection,
    since: DateTime<Utc>,
) -> Result<u32, StoreError> {
    let row = sqlx::query!(
        r#"SELECT count(*) AS "requested!" FROM proxies WHERE replacement_requested_at >= $1"#,
        since,
    )
    .fetch_one(connection)
    .await?;
    // A count is never negative; more than u32::MAX rows cannot be counted
    // against a budget that is a u32.
    Ok(u32::try_from(row.requested).unwrap_or(u32::MAX))
}
