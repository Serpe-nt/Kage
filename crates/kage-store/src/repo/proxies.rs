//! The `proxies` table.

use chrono::{DateTime, Utc};
use kage_core::domain::{Proxy, ProxyState};
use kage_core::ids::ProxyId;
use kage_core::secret::SealedSecret;
use uuid::Uuid;

use super::repository;
use crate::StoreError;
use crate::convert::{from_json_opt, parse, parse_opt, to_i32, to_json_opt, to_u16, to_u32};

repository!(
    /// Proxies: one static residential IP with credentials each.
    ProxyRepo
);

struct ProxyRow {
    id: Uuid,
    provider: String,
    provider_ref: String,
    host: String,
    port: i32,
    username: String,
    password: Vec<u8>,
    scheme: String,
    country: Option<String>,
    city: Option<String>,
    asn: Option<String>,
    state: String,
    consecutive_failures: i32,
    backoff_until: Option<DateTime<Utc>>,
    last_health: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
    retired_at: Option<DateTime<Utc>>,
}

impl TryFrom<ProxyRow> for Proxy {
    type Error = StoreError;

    fn try_from(row: ProxyRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: ProxyId::from_uuid(row.id),
            provider: parse(("proxies", "provider"), &row.provider)?,
            provider_ref: row.provider_ref,
            host: row.host,
            port: to_u16(("proxies", "port"), row.port)?,
            username: row.username,
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
            backoff_until: row.backoff_until,
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
    /// [`StoreError::Conflict`] when the provider already has a proxy with
    /// this `provider_ref`.
    pub async fn insert(&self, proxy: &Proxy) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO proxies (id, provider, provider_ref, host, port, username, password,
                                  scheme, country, city, asn, state, consecutive_failures,
                                  backoff_until, last_health, created_at, retired_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
            proxy.id.as_uuid(),
            proxy.provider.as_str(),
            proxy.provider_ref,
            proxy.host,
            i32::from(proxy.port),
            proxy.username,
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
            proxy.backoff_until,
            to_json_opt(("proxies", "last_health"), proxy.last_health.as_ref())?,
            proxy.created_at,
            proxy.retired_at,
        )
        .execute(self.pool)
        .await?;
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
                    city, asn, state, consecutive_failures, backoff_until, last_health,
                    created_at, retired_at
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
                    city, asn, state, consecutive_failures, backoff_until, last_health,
                    created_at, retired_at
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

    /// Writes back everything about a proxy that changes during its life:
    /// credentials, location, pool state and health. Returns whether the
    /// proxy exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn update(&self, proxy: &Proxy) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE proxies
             SET host = $2, port = $3, username = $4, password = $5, country = $6, city = $7,
                 asn = $8, state = $9, consecutive_failures = $10, backoff_until = $11,
                 last_health = $12, retired_at = $13
             WHERE id = $1",
            proxy.id.as_uuid(),
            proxy.host,
            i32::from(proxy.port),
            proxy.username,
            proxy.password.as_bytes(),
            proxy.country.map(|country| country.as_str().to_owned()),
            proxy.city,
            proxy.asn,
            proxy.state.as_str(),
            to_i32(
                ("proxies", "consecutive_failures"),
                proxy.consecutive_failures
            )?,
            proxy.backoff_until,
            to_json_opt(("proxies", "last_health"), proxy.last_health.as_ref())?,
            proxy.retired_at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
}
