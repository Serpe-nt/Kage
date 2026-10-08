//! The `robots_cache` and `response_cache` tables (spec §14, §15).

use chrono::{DateTime, Utc};
use kage_core::ids::ResultId;

use super::repository;
use crate::StoreError;
use crate::convert::to_u16_opt;

/// A cached robots.txt for one origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobotsCacheEntry {
    /// `scheme://host[:port]`.
    pub origin: String,
    /// When robots.txt was fetched.
    pub fetched_at: DateTime<Utc>,
    /// When the entry stops being used.
    pub expires_at: DateTime<Utc>,
    /// Status of the fetch; `None` when it failed without a response.
    pub status: Option<u16>,
    /// The file's content, when there was one.
    pub body: Option<String>,
}

repository!(
    /// The robots.txt cache, one entry per origin.
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
            "INSERT INTO robots_cache (origin, fetched_at, expires_at, status, body)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (origin) DO UPDATE
             SET fetched_at = EXCLUDED.fetched_at, expires_at = EXCLUDED.expires_at,
                 status = EXCLUDED.status, body = EXCLUDED.body",
            entry.origin,
            entry.fetched_at,
            entry.expires_at,
            entry.status.map(i32::from),
            entry.body,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The entry for an origin if it has not expired by `now`.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(
        &self,
        origin: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<RobotsCacheEntry>, StoreError> {
        let row = sqlx::query!(
            "SELECT origin, fetched_at, expires_at, status, body
             FROM robots_cache WHERE origin = $1 AND expires_at > $2",
            origin,
            now,
        )
        .fetch_optional(self.pool)
        .await?;
        row.map(|row| {
            Ok(RobotsCacheEntry {
                origin: row.origin,
                fetched_at: row.fetched_at,
                expires_at: row.expires_at,
                status: to_u16_opt(("robots_cache", "status"), row.status)?,
                body: row.body,
            })
        })
        .transpose()
    }

    /// Removes every entry that has expired by `now`. Returns how many.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn delete_expired(&self, now: DateTime<Utc>) -> Result<u64, StoreError> {
        let done = sqlx::query!("DELETE FROM robots_cache WHERE expires_at <= $1", now)
            .execute(self.pool)
            .await?;
        Ok(done.rows_affected())
    }
}

/// A cached response: a pointer from a cache key to a stored result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseCacheEntry {
    /// Hash of everything that makes two requests interchangeable (spec §15).
    pub cache_key: String,
    /// The normalized URL, for inspection and purging.
    pub url_key: String,
    /// The stored result that is served.
    pub result_id: ResultId,
    /// When the entry was stored; reported as `cached_at`.
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
            "INSERT INTO response_cache (cache_key, url_key, result_id, cached_at, expires_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (cache_key) DO UPDATE
             SET url_key = EXCLUDED.url_key, result_id = EXCLUDED.result_id,
                 cached_at = EXCLUDED.cached_at, expires_at = EXCLUDED.expires_at",
            entry.cache_key,
            entry.url_key,
            entry.result_id.as_uuid(),
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
            "SELECT cache_key, url_key, result_id, cached_at, expires_at
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
