//! The `artifacts` table. The bytes themselves are handled by
//! [`ArtifactStore`](crate::ArtifactStore).

use chrono::{DateTime, Utc};
use kage_core::domain::{Artifact, ArtifactEncoding, ContentHash};
use kage_core::ids::{ArtifactId, RequestId};
use sqlx::PgConnection;
use uuid::Uuid;

use super::repository;
use crate::StoreError;
use crate::convert::{parse, to_i64, to_u64};

repository!(
    /// Artifact rows: one per stored blob reference.
    ArtifactRepo
);

struct ArtifactRow {
    id: Uuid,
    content_hash: String,
    kind: String,
    content_type: String,
    size_bytes: i64,
    stored_bytes: i64,
    encoding: String,
    request_id: Option<Uuid>,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
}

impl TryFrom<ArtifactRow> for Artifact {
    type Error = StoreError;

    fn try_from(row: ArtifactRow) -> Result<Self, StoreError> {
        const TABLE: &str = "artifacts";
        Ok(Self {
            id: ArtifactId::from_uuid(row.id),
            content_hash: parse((TABLE, "content_hash"), &row.content_hash)?,
            kind: parse((TABLE, "kind"), &row.kind)?,
            content_type: row.content_type,
            size_bytes: to_u64((TABLE, "size_bytes"), row.size_bytes)?,
            stored_bytes: to_u64((TABLE, "stored_bytes"), row.stored_bytes)?,
            encoding: parse((TABLE, "encoding"), &row.encoding)?,
            request_id: row.request_id.map(RequestId::from_uuid),
            created_at: row.created_at,
            expires_at: row.expires_at,
        })
    }
}

/// A blob on disk: what the content hashes to and how it is stored.
pub(crate) type BlobKey = (ContentHash, ArtifactEncoding);

impl ArtifactRepo<'_> {
    /// The artifact with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: ArtifactId) -> Result<Option<Artifact>, StoreError> {
        sqlx::query_as!(
            ArtifactRow,
            "SELECT id, content_hash, kind, content_type, size_bytes, stored_bytes, encoding,
                    request_id, created_at, expires_at
             FROM artifacts WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(Artifact::try_from)
        .transpose()
    }

    /// Every artifact a request produced, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_for_request(
        &self,
        request_id: RequestId,
    ) -> Result<Vec<Artifact>, StoreError> {
        sqlx::query_as!(
            ArtifactRow,
            "SELECT id, content_hash, kind, content_type, size_bytes, stored_bytes, encoding,
                    request_id, created_at, expires_at
             FROM artifacts WHERE request_id = $1 ORDER BY id",
            request_id.as_uuid(),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Artifact::try_from)
        .collect()
    }

    /// Inserts a row on `connection`, so that it can share a transaction
    /// with the blob lock.
    pub(crate) async fn insert_on(
        connection: &mut PgConnection,
        artifact: &Artifact,
    ) -> Result<(), StoreError> {
        const TABLE: &str = "artifacts";
        sqlx::query!(
            "INSERT INTO artifacts (id, content_hash, kind, content_type, size_bytes, stored_bytes,
                                    encoding, request_id, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            artifact.id.as_uuid(),
            artifact.content_hash.as_str(),
            artifact.kind.as_str(),
            artifact.content_type,
            to_i64((TABLE, "size_bytes"), artifact.size_bytes)?,
            to_i64((TABLE, "stored_bytes"), artifact.stored_bytes)?,
            artifact.encoding.as_str(),
            artifact.request_id.map(|id| id.as_uuid()),
            artifact.created_at,
            artifact.expires_at,
        )
        .execute(connection)
        .await?;
        Ok(())
    }

    /// Deletes up to `limit` rows that have expired by `now` and returns the
    /// blob each one pointed at.
    pub(crate) async fn delete_expired(
        &self,
        now: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<BlobKey>, StoreError> {
        let rows = sqlx::query!(
            "DELETE FROM artifacts
             WHERE id IN (SELECT id FROM artifacts WHERE expires_at <= $1
                          ORDER BY expires_at LIMIT $2)
             RETURNING content_hash, encoding",
            now,
            limit,
        )
        .fetch_all(self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    parse(("artifacts", "content_hash"), &row.content_hash)?,
                    parse(("artifacts", "encoding"), &row.encoding)?,
                ))
            })
            .collect()
    }

    /// Takes the transaction-scoped lock for one blob. Writers of a blob and
    /// the retention job both take it, so a blob is never removed while a
    /// row that points at it is being inserted.
    pub(crate) async fn lock_blob(
        connection: &mut PgConnection,
        hash: &ContentHash,
    ) -> Result<(), StoreError> {
        // The function returns `void`, which the checked macros cannot decode.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(hash.as_str())
            .execute(connection)
            .await?;
        Ok(())
    }

    /// Whether any row still points at this blob.
    pub(crate) async fn blob_in_use_on(
        connection: &mut PgConnection,
        hash: &ContentHash,
        encoding: ArtifactEncoding,
    ) -> Result<bool, StoreError> {
        let row = sqlx::query!(
            r#"SELECT EXISTS (SELECT 1 FROM artifacts WHERE content_hash = $1 AND encoding = $2)
                   AS "in_use!""#,
            hash.as_str(),
            encoding.as_str(),
        )
        .fetch_one(connection)
        .await?;
        Ok(row.in_use)
    }
}
