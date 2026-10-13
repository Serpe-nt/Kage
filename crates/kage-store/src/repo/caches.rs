//! The `robots_cache`, `site_scouts` and `response_cache` tables (spec
//! §14, §15).

use chrono::{DateTime, Utc};
use kage_core::ids::{ApiKeyId, PersonaId, ResultId};

use super::repository;
use crate::StoreError;
use crate::convert::to_u16_opt;

/// What is known of the `robots.txt` of one origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotsCacheEntry {
    /// `scheme://host[:port]`.
    pub origin: String,
    /// The site the origin belongs to: its registrable domain.
    pub site: String,
    /// When the file was asked for.
    pub fetched_at: DateTime<Utc>,
    /// When it is to be asked for again, if a request needs it.
    pub expires_at: DateTime<Utc>,
    /// The status of the answer; `None` when there was none.
    pub status: Option<u16>,
    /// The file's content, when there was one.
    pub body: Option<String>,
    /// Whether the file could not be had: a server error, a timeout, a
    /// block.
    pub unreachable: bool,
}

repository!(
    /// What is known of the `robots.txt` of every origin that was asked.
    RobotsCacheRepo
);

impl RobotsCacheRepo<'_> {
    /// Stores an entry, replacing the one for the same origin.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn put(&self, entry: &RobotsCacheEntry) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO robots_cache (origin, site, fetched_at, expires_at, status, body, unreachable)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (origin) DO UPDATE
             SET site = EXCLUDED.site, fetched_at = EXCLUDED.fetched_at,
                 expires_at = EXCLUDED.expires_at, status = EXCLUDED.status,
                 body = EXCLUDED.body, unreachable = EXCLUDED.unreachable",
            entry.origin,
            entry.site,
            entry.fetched_at,
            entry.expires_at,
            entry.status.map(i32::from),
            entry.body,
            entry.unreachable,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The entry for an origin, however old: one that has expired says
    /// that the file is to be asked for again.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, origin: &str) -> Result<Option<RobotsCacheEntry>, StoreError> {
        let row = sqlx::query!(
            "SELECT origin, site, fetched_at, expires_at, status, body, unreachable
             FROM robots_cache WHERE origin = $1",
            origin,
        )
        .fetch_optional(self.pool)
        .await?;
        row.map(|row| {
            Ok(RobotsCacheEntry {
                origin: row.origin,
                site: row.site,
                fetched_at: row.fetched_at,
                expires_at: row.expires_at,
                status: to_u16_opt(("robots_cache", "status"), row.status)?,
                body: row.body,
                unreachable: row.unreachable,
            })
        })
        .transpose()
    }
}

repository!(
    /// The persona that asks for a site's `robots.txt` and sitemaps, one
    /// per site.
    SiteScoutRepo
);

impl SiteScoutRepo<'_> {
    /// The scout of a site, when it has one that is not retired.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn get(&self, site: &str) -> Result<Option<PersonaId>, StoreError> {
        let persona = sqlx::query_scalar!(
            "SELECT scout.persona_id FROM site_scouts scout
             JOIN personas persona ON persona.id = scout.persona_id
             WHERE scout.site = $1 AND persona.state <> 'retired'",
            site,
        )
        .fetch_optional(self.pool)
        .await?;
        Ok(persona.map(PersonaId::from_uuid))
    }

    /// Makes a persona the scout of a site, in place of whoever was.
    ///
    /// # Errors
    ///
    /// Fails when the query fails, or the persona does not exist.
    pub async fn set(
        &self,
        site: &str,
        persona: PersonaId,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO site_scouts (site, persona_id, since) VALUES ($1, $2, $3)
             ON CONFLICT (site) DO UPDATE
             SET persona_id = EXCLUDED.persona_id, since = EXCLUDED.since
             WHERE site_scouts.persona_id <> EXCLUDED.persona_id",
            site,
            persona.as_uuid(),
            now,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The personas that have asked a host of a site for a page and are
    /// not its scout: its readers. A host belongs to a site when it is the
    /// site's registrable domain or ends in a dot and it.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn readers(&self, site: &str) -> Result<Vec<PersonaId>, StoreError> {
        let readers = sqlx::query_scalar!(
            "SELECT DISTINCT state.persona_id FROM persona_domain_state state
             WHERE state.last_request_at IS NOT NULL
               AND (state.domain = $1 OR right(state.domain, length($1) + 1) = '.' || $1)
               AND NOT EXISTS (SELECT 1 FROM site_scouts scout
                               WHERE scout.site = $1 AND scout.persona_id = state.persona_id)
             ORDER BY state.persona_id",
            site,
        )
        .fetch_all(self.pool)
        .await?;
        Ok(readers.into_iter().map(PersonaId::from_uuid).collect())
    }
}

/// A cached response: a pointer from a cache key to a stored result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseCacheEntry {
    /// Hash of everything that makes two requests interchangeable (spec §15).
    pub cache_key: String,
    /// The normalized URL, for inspection.
    pub url_key: String,
    /// The stored result that is served.
    pub result_id: ResultId,
    /// The key the entry serves; `None` for a site whose policy shares
    /// its pages between keys.
    pub api_key_id: Option<ApiKeyId>,
    /// When the page was fetched; reported as `cached_at`.
    pub cached_at: DateTime<Utc>,
    /// When the entry stops being served.
    pub expires_at: DateTime<Utc>,
}

repository!(
    /// The opt-in response cache.
    ResponseCacheRepo
);

impl ResponseCacheRepo<'_> {
    /// Stores an entry, replacing the one with the same key.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn put(&self, entry: &ResponseCacheEntry) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO response_cache (cache_key, url_key, result_id, api_key_id, cached_at,
                                         expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (cache_key) DO UPDATE
             SET url_key = EXCLUDED.url_key, result_id = EXCLUDED.result_id,
                 api_key_id = EXCLUDED.api_key_id, cached_at = EXCLUDED.cached_at,
                 expires_at = EXCLUDED.expires_at",
            entry.cache_key,
            entry.url_key,
            entry.result_id.as_uuid(),
            entry.api_key_id.map(|key| key.as_uuid()),
            entry.cached_at,
            entry.expires_at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The entry for a key if it has not expired by `now`.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn get(
        &self,
        cache_key: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<ResponseCacheEntry>, StoreError> {
        let row = sqlx::query!(
            "SELECT cache_key, url_key, result_id, api_key_id, cached_at, expires_at
             FROM response_cache WHERE cache_key = $1 AND expires_at > $2",
            cache_key,
            now,
        )
        .fetch_optional(self.pool)
        .await?;
        Ok(row.map(|row| ResponseCacheEntry {
            cache_key: row.cache_key,
            url_key: row.url_key,
            result_id: ResultId::from_uuid(row.result_id),
            api_key_id: row.api_key_id.map(ApiKeyId::from_uuid),
            cached_at: row.cached_at,
            expires_at: row.expires_at,
        }))
    }

    /// Removes every entry that has expired by `now`. Returns how many.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn delete_expired(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let done = sqlx::query!("DELETE FROM response_cache WHERE expires_at <= $1", now)
            .execute(self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}
