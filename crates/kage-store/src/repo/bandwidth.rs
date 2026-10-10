//! The `bandwidth_usage` table: bytes through the forwarder per hour,
//! persona and domain (decision D5).

use chrono::{DateTime, Utc};
use kage_core::domain::BandwidthUsage;
use kage_core::ids::PersonaId;
use uuid::Uuid;

use super::{page_limit, repository};
use crate::StoreError;
use crate::convert::{to_i64, to_u64};

const TABLE: &str = "bandwidth_usage";

repository!(
    /// Bytes through the forwarder.
    BandwidthRepo
);

/// Bytes in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Traffic {
    /// Bytes sent towards the proxy.
    pub bytes_up: u64,
    /// Bytes received from the proxy.
    pub bytes_down: u64,
}

impl Traffic {
    /// Both directions together.
    pub const fn total(&self) -> u64 {
        self.bytes_up.saturating_add(self.bytes_down)
    }
}

fn traffic(up: i64, down: i64) -> Result<Traffic, StoreError> {
    Ok(Traffic {
        bytes_up: to_u64((TABLE, "bytes_up"), up)?,
        bytes_down: to_u64((TABLE, "bytes_down"), down)?,
    })
}

impl BandwidthRepo<'_> {
    /// Adds bytes to what is stored for their hour, persona and domain.
    /// Everything is added in one transaction, or nothing.
    ///
    /// # Errors
    ///
    /// Fails when a query fails (the persona does not exist, the hour is
    /// not the start of one) or a count is out of range.
    pub async fn add(&self, usages: &[BandwidthUsage]) -> Result<(), StoreError> {
        if usages.is_empty() {
            return Ok(());
        }
        let mut transaction = self.pool.begin().await?;
        for usage in usages {
            sqlx::query!(
                "INSERT INTO bandwidth_usage (hour, persona_id, domain, bytes_up, bytes_down)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (hour, persona_id, domain) DO UPDATE
                 SET bytes_up = bandwidth_usage.bytes_up + EXCLUDED.bytes_up,
                     bytes_down = bandwidth_usage.bytes_down + EXCLUDED.bytes_down",
                usage.hour,
                usage.persona_id.as_uuid(),
                usage.domain,
                to_i64((TABLE, "bytes_up"), usage.bytes_up)?,
                to_i64((TABLE, "bytes_down"), usage.bytes_down)?,
            )
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    /// Everything counted from `since` on.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn total_since(&self, since: DateTime<Utc>) -> Result<Traffic, StoreError> {
        let row = sqlx::query!(
            r#"SELECT COALESCE(SUM(bytes_up), 0)::bigint AS "up!",
                      COALESCE(SUM(bytes_down), 0)::bigint AS "down!"
               FROM bandwidth_usage WHERE hour >= $1"#,
            since,
        )
        .fetch_one(self.pool)
        .await?;
        traffic(row.up, row.down)
    }

    /// What each persona exchanged from `since` on, the heaviest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn by_persona_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<(PersonaId, Traffic)>, StoreError> {
        struct Row {
            persona_id: Uuid,
            up: i64,
            down: i64,
        }
        sqlx::query_as!(
            Row,
            r#"SELECT persona_id,
                      SUM(bytes_up)::bigint AS "up!",
                      SUM(bytes_down)::bigint AS "down!"
               FROM bandwidth_usage WHERE hour >= $1
               GROUP BY persona_id
               ORDER BY SUM(bytes_up) + SUM(bytes_down) DESC, persona_id"#,
            since,
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(|row| {
            Ok((
                PersonaId::from_uuid(row.persona_id),
                traffic(row.up, row.down)?,
            ))
        })
        .collect()
    }

    /// What was exchanged per domain from `since` on, the heaviest first,
    /// at most `limit` domains.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn by_domain_since(
        &self,
        since: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<(String, Traffic)>, StoreError> {
        struct Row {
            domain: String,
            up: i64,
            down: i64,
        }
        sqlx::query_as!(
            Row,
            r#"SELECT domain,
                      SUM(bytes_up)::bigint AS "up!",
                      SUM(bytes_down)::bigint AS "down!"
               FROM bandwidth_usage WHERE hour >= $1
               GROUP BY domain
               ORDER BY SUM(bytes_up) + SUM(bytes_down) DESC, domain
               LIMIT $2"#,
            since,
            page_limit(limit),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(|row| Ok((row.domain, traffic(row.up, row.down)?)))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Traffic;

    #[test]
    fn the_total_is_both_directions() {
        let traffic = Traffic {
            bytes_up: 5,
            bytes_down: 7,
        };

        assert_eq!(traffic.total(), 12);
        assert_eq!(Traffic::default().total(), 0);
        let huge = Traffic {
            bytes_up: u64::MAX,
            bytes_down: 1,
        };
        assert_eq!(huge.total(), u64::MAX);
    }
}
