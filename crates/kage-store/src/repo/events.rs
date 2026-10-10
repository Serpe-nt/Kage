//! The `health_events`, `selftest_runs` and `domain_breakers` tables.

use chrono::{DateTime, Utc};
use kage_core::alert::Alert;
use kage_core::domain::{DomainBreaker, HealthEvent, Render, SelftestRun, SubjectKind};
use kage_core::ids::{HealthEventId, PersonaId, SelftestRunId};
use uuid::Uuid;

use super::{page_limit, repository};
use crate::StoreError;
use crate::convert::{parse, to_i32, to_u32};

repository!(
    /// Health events, including stored operator alerts.
    HealthEventRepo
);

struct HealthEventRow {
    id: Uuid,
    at: DateTime<Utc>,
    subject_kind: String,
    subject_id: Option<Uuid>,
    kind: String,
    severity: String,
    details: serde_json::Value,
}

impl TryFrom<HealthEventRow> for HealthEvent {
    type Error = StoreError;

    fn try_from(row: HealthEventRow) -> Result<Self, StoreError> {
        const TABLE: &str = "health_events";
        Ok(Self {
            id: HealthEventId::from_uuid(row.id),
            at: row.at,
            subject_kind: parse((TABLE, "subject_kind"), &row.subject_kind)?,
            subject_id: row.subject_id,
            kind: row.kind,
            severity: parse_severity(&row.severity)?,
            details: row.details,
        })
    }
}

fn parse_severity(text: &str) -> Result<kage_core::alert::AlertSeverity, StoreError> {
    use kage_core::alert::AlertSeverity;

    [
        AlertSeverity::Info,
        AlertSeverity::Warning,
        AlertSeverity::Critical,
    ]
    .into_iter()
    .find(|severity| severity.as_str() == text)
    .ok_or_else(|| StoreError::Corrupt {
        table: "health_events",
        column: "severity",
        reason: format!("unknown severity {text:?}"),
    })
}

impl HealthEventRepo<'_> {
    /// Appends an event.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn insert(&self, event: &HealthEvent) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO health_events (id, at, subject_kind, subject_id, kind, severity, details)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            event.id.as_uuid(),
            event.at,
            event.subject_kind.as_str(),
            event.subject_id,
            event.kind,
            event.severity.as_str(),
            event.details,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Stores an operator alert as a health event about `subject_kind`, and
    /// about the one subject the alert names, if it names one. The alert's
    /// message is kept inside the details.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn record_alert(
        &self,
        subject_kind: SubjectKind,
        alert: &Alert,
    ) -> Result<HealthEvent, StoreError> {
        let event = HealthEvent {
            id: HealthEventId::new(),
            at: alert.at,
            subject_kind,
            subject_id: alert.subject,
            kind: alert.kind.as_str().to_owned(),
            severity: alert.severity,
            details: serde_json::json!({
                "message": alert.message,
                "details": alert.details,
            }),
        };
        self.insert(&event).await?;
        Ok(event)
    }

    /// The most recent events, newest first, optionally about one kind of
    /// subject.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn recent(
        &self,
        subject_kind: Option<SubjectKind>,
        limit: u32,
    ) -> Result<Vec<HealthEvent>, StoreError> {
        sqlx::query_as!(
            HealthEventRow,
            "SELECT id, at, subject_kind, subject_id, kind, severity, details
             FROM health_events
             WHERE $1::text IS NULL OR subject_kind = $1
             ORDER BY id DESC
             LIMIT $2",
            subject_kind.map(SubjectKind::as_str),
            page_limit(limit),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(HealthEvent::try_from)
        .collect()
    }
}

repository!(
    /// Fingerprint self-test runs.
    SelftestRepo
);

struct SelftestRow {
    id: Uuid,
    persona_id: Uuid,
    engine: String,
    render: String,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    passed: bool,
    score: Option<f32>,
    findings: serde_json::Value,
}

impl TryFrom<SelftestRow> for SelftestRun {
    type Error = StoreError;

    fn try_from(row: SelftestRow) -> Result<Self, StoreError> {
        const TABLE: &str = "selftest_runs";
        Ok(Self {
            id: SelftestRunId::from_uuid(row.id),
            persona_id: PersonaId::from_uuid(row.persona_id),
            engine: parse((TABLE, "engine"), &row.engine)?,
            render: parse((TABLE, "render"), &row.render)?,
            started_at: row.started_at,
            finished_at: row.finished_at,
            passed: row.passed,
            score: row.score,
            findings: row.findings,
        })
    }
}

impl SelftestRepo<'_> {
    /// Stores a finished run.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn insert(&self, run: &SelftestRun) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO selftest_runs (id, persona_id, engine, render, started_at, finished_at,
                                        passed, score, findings)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            run.id.as_uuid(),
            run.persona_id.as_uuid(),
            run.engine.as_str(),
            run.render.as_str(),
            run.started_at,
            run.finished_at,
            run.passed,
            run.score,
            run.findings,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// A persona's most recent run on one path.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn latest(
        &self,
        persona_id: PersonaId,
        render: Render,
    ) -> Result<Option<SelftestRun>, StoreError> {
        sqlx::query_as!(
            SelftestRow,
            "SELECT id, persona_id, engine, render, started_at, finished_at, passed, score, findings
             FROM selftest_runs
             WHERE persona_id = $1 AND render = $2
             ORDER BY started_at DESC, id DESC
             LIMIT 1",
            persona_id.as_uuid(),
            render.as_str(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(SelftestRun::try_from)
        .transpose()
    }

    /// A persona's most recent runs on both paths, newest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn recent(
        &self,
        persona_id: PersonaId,
        limit: u32,
    ) -> Result<Vec<SelftestRun>, StoreError> {
        sqlx::query_as!(
            SelftestRow,
            "SELECT id, persona_id, engine, render, started_at, finished_at, passed, score, findings
             FROM selftest_runs
             WHERE persona_id = $1
             ORDER BY started_at DESC, id DESC
             LIMIT $2",
            persona_id.as_uuid(),
            page_limit(limit),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(SelftestRun::try_from)
        .collect()
    }

    /// How many runs a persona has on one path: the number of its next
    /// scheduled run, which decides that run's jitter.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn count(&self, persona_id: PersonaId, render: Render) -> Result<u64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"SELECT count(*) AS "count!" FROM selftest_runs WHERE persona_id = $1 AND render = $2"#,
            persona_id.as_uuid(),
            render.as_str(),
        )
        .fetch_one(self.pool)
        .await?;
        Ok(u64::try_from(count).unwrap_or(0))
    }
}

repository!(
    /// Per-domain circuit breakers.
    BreakerRepo
);

struct BreakerRow {
    domain: String,
    state: String,
    block_count_1h: i32,
    window_started_at: Option<DateTime<Utc>>,
    paused_until: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<BreakerRow> for DomainBreaker {
    type Error = StoreError;

    fn try_from(row: BreakerRow) -> Result<Self, StoreError> {
        const TABLE: &str = "domain_breakers";
        Ok(Self {
            domain: row.domain,
            state: parse((TABLE, "state"), &row.state)?,
            block_count_1h: to_u32((TABLE, "block_count_1h"), row.block_count_1h)?,
            window_started_at: row.window_started_at,
            paused_until: row.paused_until,
            updated_at: row.updated_at,
        })
    }
}

impl BreakerRepo<'_> {
    /// The breaker for a domain; `None` when the domain has never had a
    /// block counted.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, domain: &str) -> Result<Option<DomainBreaker>, StoreError> {
        sqlx::query_as!(
            BreakerRow,
            "SELECT domain, state, block_count_1h, window_started_at, paused_until, updated_at
             FROM domain_breakers WHERE domain = $1",
            domain,
        )
        .fetch_optional(self.pool)
        .await?
        .map(DomainBreaker::try_from)
        .transpose()
    }

    /// Stores a breaker, replacing the one for the same domain.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the counter is out of range.
    pub async fn upsert(&self, breaker: &DomainBreaker) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO domain_breakers (domain, state, block_count_1h, window_started_at,
                                          paused_until, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (domain) DO UPDATE
             SET state = EXCLUDED.state, block_count_1h = EXCLUDED.block_count_1h,
                 window_started_at = EXCLUDED.window_started_at,
                 paused_until = EXCLUDED.paused_until, updated_at = EXCLUDED.updated_at",
            breaker.domain,
            breaker.state.as_str(),
            to_i32(
                ("domain_breakers", "block_count_1h"),
                breaker.block_count_1h
            )?,
            breaker.window_started_at,
            breaker.paused_until,
            breaker.updated_at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Every breaker stored as open, by domain. Use
    /// [`DomainBreaker::is_open`] to tell whether its pause still runs.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_open(&self) -> Result<Vec<DomainBreaker>, StoreError> {
        sqlx::query_as!(
            BreakerRow,
            "SELECT domain, state, block_count_1h, window_started_at, paused_until, updated_at
             FROM domain_breakers WHERE state = 'open' ORDER BY domain",
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(DomainBreaker::try_from)
        .collect()
    }
}
