//! The `http_cookies` table: the cookie jar of the http path, one jar per
//! proxy.

use chrono::{DateTime, Utc};
use kage_core::domain::HttpCookie;
use kage_core::ids::ProxyId;

use super::repository;
use crate::StoreError;
use crate::convert::parse_opt;

repository!(
    /// Cookie jars of the http path.
    HttpCookieRepo
);

struct CookieRow {
    domain: String,
    host_only: bool,
    path: String,
    name: String,
    value: String,
    secure: bool,
    http_only: bool,
    same_site: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    last_used_at: DateTime<Utc>,
}

impl TryFrom<CookieRow> for HttpCookie {
    type Error = StoreError;

    fn try_from(row: CookieRow) -> Result<Self, StoreError> {
        Ok(Self {
            domain: row.domain,
            host_only: row.host_only,
            path: row.path,
            name: row.name,
            value: row.value,
            secure: row.secure,
            http_only: row.http_only,
            same_site: parse_opt(("http_cookies", "same_site"), row.same_site.as_deref())?,
            expires_at: row.expires_at,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
        })
    }
}

impl HttpCookieRepo<'_> {
    /// The jar of a proxy, oldest cookie first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list(&self, proxy: ProxyId) -> Result<Vec<HttpCookie>, StoreError> {
        sqlx::query_as!(
            CookieRow,
            "SELECT domain, host_only, path, name, value, secure, http_only, same_site,
                    expires_at, created_at, last_used_at
             FROM http_cookies
             WHERE proxy_id = $1
             ORDER BY ordinal",
            proxy.as_uuid(),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(HttpCookie::try_from)
        .collect()
    }

    /// Replaces the jar of a proxy with `cookies`, which are stored in the
    /// order given. One transaction: a reader sees the old jar or the new
    /// one, never half of each.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when two of the cookies are the same cookie
    /// (equal domain, host-only flag, path and name); a database error when
    /// the proxy does not exist. Nothing is written in either case.
    pub async fn replace(&self, proxy: ProxyId, cookies: &[HttpCookie]) -> Result<(), StoreError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query!(
            "DELETE FROM http_cookies WHERE proxy_id = $1",
            proxy.as_uuid()
        )
        .execute(&mut *transaction)
        .await?;
        for (ordinal, cookie) in cookies.iter().enumerate() {
            sqlx::query!(
                "INSERT INTO http_cookies (proxy_id, domain, host_only, path, name, value, secure,
                                           http_only, same_site, expires_at, created_at,
                                           last_used_at, ordinal)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                proxy.as_uuid(),
                cookie.domain,
                cookie.host_only,
                cookie.path,
                cookie.name,
                cookie.value,
                cookie.secure,
                cookie.http_only,
                cookie
                    .same_site
                    .map(|same_site| same_site.as_str().to_owned()),
                cookie.expires_at,
                cookie.created_at,
                cookie.last_used_at,
                i32::try_from(ordinal).map_err(|_| StoreError::OutOfRange {
                    table: "http_cookies",
                    column: "ordinal",
                })?,
            )
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Deletes the whole jar of a proxy and returns how many cookies it
    /// held. A retired persona's cookies must not outlive it.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn delete_for_proxy(&self, proxy: ProxyId) -> Result<u64, StoreError> {
        let done = sqlx::query!(
            "DELETE FROM http_cookies WHERE proxy_id = $1",
            proxy.as_uuid()
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// Deletes the cookies a proxy holds for one domain and its subdomains,
    /// and returns how many there were.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn delete_for_domain(&self, proxy: ProxyId, domain: &str) -> Result<u64, StoreError> {
        let domain = domain.trim_start_matches('.').to_ascii_lowercase();
        let done = sqlx::query!(
            "DELETE FROM http_cookies
             WHERE proxy_id = $1 AND (domain = $2 OR right(domain, length($2) + 1) = '.' || $2)",
            proxy.as_uuid(),
            domain,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected())
    }

    /// How many cookies a proxy holds.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn count(&self, proxy: ProxyId) -> Result<u64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM http_cookies WHERE proxy_id = $1"#,
            proxy.as_uuid()
        )
        .fetch_one(self.pool)
        .await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }
}
