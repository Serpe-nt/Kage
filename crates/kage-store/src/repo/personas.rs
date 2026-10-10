//! The `personas` and `persona_domain_state` tables.

use chrono::{DateTime, NaiveDate, Utc};
use kage_core::domain::{CoolingReason, Persona, PersonaDomainState, PersonaState};
use kage_core::ids::{PersonaId, ProxyId};
use kage_core::outcome::Outcome;
use sqlx::PgConnection;
use uuid::Uuid;

use super::repository;
use crate::StoreError;
use crate::convert::{
    from_json, from_json_opt, parse, parse_opt, to_i32, to_json, to_json_opt, to_u32,
};

repository!(
    /// Personas: one long-lived synthetic user per proxy.
    PersonaRepo
);

struct PersonaRow {
    id: Uuid,
    proxy_id: Uuid,
    engine: String,
    claimed_os: String,
    browser_major: i32,
    seed: String,
    fingerprint: serde_json::Value,
    user_agent: String,
    hardware_concurrency: i32,
    wreq_profile: Option<String>,
    country: Option<String>,
    locale: String,
    languages: Vec<String>,
    timezone: String,
    geolocation: Option<serde_json::Value>,
    screen: serde_json::Value,
    tables_version: String,
    profile_dir: String,
    state: String,
    cooling_reason: Option<String>,
    warmup_visits: i32,
    warmed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    retired_at: Option<DateTime<Utc>>,
    retire_reason: Option<String>,
    profile_archived_at: Option<DateTime<Utc>>,
    profile_archive: Option<String>,
    profile_deleted_at: Option<DateTime<Utc>>,
}

impl TryFrom<PersonaRow> for Persona {
    type Error = StoreError;

    fn try_from(row: PersonaRow) -> Result<Self, StoreError> {
        const TABLE: &str = "personas";
        Ok(Self {
            id: PersonaId::from_uuid(row.id),
            proxy_id: ProxyId::from_uuid(row.proxy_id),
            engine: parse((TABLE, "engine"), &row.engine)?,
            claimed_os: parse((TABLE, "claimed_os"), &row.claimed_os)?,
            browser_major: to_u32((TABLE, "browser_major"), row.browser_major)?,
            seed: parse((TABLE, "seed"), &row.seed)?,
            fingerprint: row.fingerprint,
            user_agent: row.user_agent,
            hardware_concurrency: to_u32(
                (TABLE, "hardware_concurrency"),
                row.hardware_concurrency,
            )?,
            wreq_profile: row.wreq_profile,
            country: parse_opt((TABLE, "country"), row.country.as_deref())?,
            locale: row.locale,
            languages: row.languages,
            timezone: row.timezone,
            geolocation: from_json_opt((TABLE, "geolocation"), row.geolocation)?,
            screen: from_json((TABLE, "screen"), row.screen)?,
            tables_version: row.tables_version,
            profile_dir: row.profile_dir,
            state: parse((TABLE, "state"), &row.state)?,
            cooling_reason: parse_opt((TABLE, "cooling_reason"), row.cooling_reason.as_deref())?,
            warmup_visits: to_u32((TABLE, "warmup_visits"), row.warmup_visits)?,
            warmed_at: row.warmed_at,
            created_at: row.created_at,
            retired_at: row.retired_at,
            retire_reason: row.retire_reason,
            profile_archived_at: row.profile_archived_at,
            profile_archive: row.profile_archive,
            profile_deleted_at: row.profile_deleted_at,
        })
    }
}

impl PersonaRepo<'_> {
    /// Stores a new persona.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the proxy already has a persona that
    /// is not retired.
    pub async fn insert(&self, persona: &Persona) -> Result<(), StoreError> {
        const TABLE: &str = "personas";
        sqlx::query!(
            "INSERT INTO personas (id, proxy_id, engine, claimed_os, browser_major, seed,
                                   fingerprint, user_agent, hardware_concurrency, wreq_profile,
                                   country, locale, languages, timezone, geolocation, screen,
                                   tables_version, profile_dir, state, cooling_reason,
                                   warmup_visits, warmed_at, created_at, retired_at,
                                   retire_reason, profile_archived_at, profile_archive,
                                   profile_deleted_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                     $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28)",
            persona.id.as_uuid(),
            persona.proxy_id.as_uuid(),
            persona.engine.as_str(),
            persona.claimed_os.as_str(),
            to_i32((TABLE, "browser_major"), persona.browser_major)?,
            persona.seed.to_string(),
            persona.fingerprint,
            persona.user_agent,
            to_i32(
                (TABLE, "hardware_concurrency"),
                persona.hardware_concurrency
            )?,
            persona.wreq_profile,
            persona.country.map(|country| country.as_str().to_owned()),
            persona.locale,
            &persona.languages,
            persona.timezone,
            to_json_opt((TABLE, "geolocation"), persona.geolocation.as_ref())?,
            to_json((TABLE, "screen"), &persona.screen)?,
            persona.tables_version,
            persona.profile_dir,
            persona.state.as_str(),
            persona.cooling_reason.map(CoolingReason::as_str),
            to_i32((TABLE, "warmup_visits"), persona.warmup_visits)?,
            persona.warmed_at,
            persona.created_at,
            persona.retired_at,
            persona.retire_reason,
            persona.profile_archived_at,
            persona.profile_archive,
            persona.profile_deleted_at,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// The persona with this id.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(&self, id: PersonaId) -> Result<Option<Persona>, StoreError> {
        sqlx::query_as!(
            PersonaRow,
            "SELECT id, proxy_id, engine, claimed_os, browser_major, seed, fingerprint,
                    user_agent, hardware_concurrency, wreq_profile, country, locale, languages,
                    timezone, geolocation, screen, tables_version, profile_dir, state,
                    cooling_reason, warmup_visits, warmed_at, created_at, retired_at,
                    retire_reason, profile_archived_at, profile_archive, profile_deleted_at
             FROM personas WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(Persona::try_from)
        .transpose()
    }

    /// The persona that lives behind this proxy now: the one that is not
    /// retired. A proxy has at most one.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get_by_proxy(&self, proxy_id: ProxyId) -> Result<Option<Persona>, StoreError> {
        sqlx::query_as!(
            PersonaRow,
            "SELECT id, proxy_id, engine, claimed_os, browser_major, seed, fingerprint,
                    user_agent, hardware_concurrency, wreq_profile, country, locale, languages,
                    timezone, geolocation, screen, tables_version, profile_dir, state,
                    cooling_reason, warmup_visits, warmed_at, created_at, retired_at,
                    retire_reason, profile_archived_at, profile_archive, profile_deleted_at
             FROM personas WHERE proxy_id = $1 AND state <> 'retired'",
            proxy_id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(Persona::try_from)
        .transpose()
    }

    /// Every persona, or only those in `state`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list(&self, state: Option<PersonaState>) -> Result<Vec<Persona>, StoreError> {
        sqlx::query_as!(
            PersonaRow,
            "SELECT id, proxy_id, engine, claimed_os, browser_major, seed, fingerprint,
                    user_agent, hardware_concurrency, wreq_profile, country, locale, languages,
                    timezone, geolocation, screen, tables_version, profile_dir, state,
                    cooling_reason, warmup_visits, warmed_at, created_at, retired_at,
                    retire_reason, profile_archived_at, profile_archive, profile_deleted_at
             FROM personas
             WHERE $1::text IS NULL OR state = $1
             ORDER BY id",
            state.map(PersonaState::as_str),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Persona::try_from)
        .collect()
    }

    /// Every persona that is not retired, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_live(&self) -> Result<Vec<Persona>, StoreError> {
        sqlx::query_as!(
            PersonaRow,
            "SELECT id, proxy_id, engine, claimed_os, browser_major, seed, fingerprint,
                    user_agent, hardware_concurrency, wreq_profile, country, locale, languages,
                    timezone, geolocation, screen, tables_version, profile_dir, state,
                    cooling_reason, warmup_visits, warmed_at, created_at, retired_at,
                    retire_reason, profile_archived_at, profile_archive, profile_deleted_at
             FROM personas
             WHERE state <> 'retired'
             ORDER BY id",
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Persona::try_from)
        .collect()
    }

    /// Retired personas whose profile directory is still to be archived:
    /// retired at or before `retired_by`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_archive_due(
        &self,
        retired_by: DateTime<Utc>,
    ) -> Result<Vec<Persona>, StoreError> {
        sqlx::query_as!(
            PersonaRow,
            "SELECT id, proxy_id, engine, claimed_os, browser_major, seed, fingerprint,
                    user_agent, hardware_concurrency, wreq_profile, country, locale, languages,
                    timezone, geolocation, screen, tables_version, profile_dir, state,
                    cooling_reason, warmup_visits, warmed_at, created_at, retired_at,
                    retire_reason, profile_archived_at, profile_archive, profile_deleted_at
             FROM personas
             WHERE state = 'retired' AND profile_archived_at IS NULL AND retired_at <= $1
             ORDER BY id",
            retired_by,
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Persona::try_from)
        .collect()
    }

    /// Retired personas whose archive is still to be deleted: archived at or
    /// before `archived_by`, oldest first.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_delete_due(
        &self,
        archived_by: DateTime<Utc>,
    ) -> Result<Vec<Persona>, StoreError> {
        sqlx::query_as!(
            PersonaRow,
            "SELECT id, proxy_id, engine, claimed_os, browser_major, seed, fingerprint,
                    user_agent, hardware_concurrency, wreq_profile, country, locale, languages,
                    timezone, geolocation, screen, tables_version, profile_dir, state,
                    cooling_reason, warmup_visits, warmed_at, created_at, retired_at,
                    retire_reason, profile_archived_at, profile_archive, profile_deleted_at
             FROM personas
             WHERE profile_archived_at IS NOT NULL AND profile_deleted_at IS NULL
               AND profile_archived_at <= $1
             ORDER BY id",
            archived_by,
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(Persona::try_from)
        .collect()
    }

    /// Reads one persona, lets `change` alter it, and writes it back, all
    /// in one transaction that holds the row lock. Concurrent callers, in
    /// this process or another (the CLI next to the server), therefore
    /// apply their changes one after the other, each to what the one before
    /// left.
    ///
    /// What can change: the state and its reason, what follows the geo
    /// record of the address (country, locale, languages, time zone,
    /// position), what an engine upgrade moves (browser major, wreq
    /// profile, user agent), the warm-up counters, and the retirement and
    /// archive marks. The identity (engine, system, seed, fingerprint,
    /// screen, profile directory) is written once and never again.
    ///
    /// Returns `None` when the persona does not exist; `change` is not
    /// called then.
    ///
    /// # Errors
    ///
    /// Fails when a query fails or the row does not parse; nothing is
    /// written in that case.
    pub async fn modify<T>(
        &self,
        id: PersonaId,
        change: impl FnOnce(&mut Persona) -> T,
    ) -> Result<Option<T>, StoreError> {
        let mut transaction = self.pool.begin().await?;
        let Some(mut persona) = lock_row(&mut transaction, id).await? else {
            return Ok(None);
        };
        let result = change(&mut persona);
        write(&mut transaction, &persona).await?;
        transaction.commit().await?;
        Ok(Some(result))
    }
}

/// Reads a persona and locks its row until the transaction ends.
async fn lock_row(
    connection: &mut PgConnection,
    id: PersonaId,
) -> Result<Option<Persona>, StoreError> {
    sqlx::query_as!(
        PersonaRow,
        "SELECT id, proxy_id, engine, claimed_os, browser_major, seed, fingerprint, user_agent,
                hardware_concurrency, wreq_profile, country, locale, languages, timezone,
                geolocation, screen, tables_version, profile_dir, state, cooling_reason,
                warmup_visits, warmed_at, created_at, retired_at, retire_reason,
                profile_archived_at, profile_archive, profile_deleted_at
         FROM personas WHERE id = $1 FOR UPDATE",
        id.as_uuid(),
    )
    .fetch_optional(connection)
    .await?
    .map(Persona::try_from)
    .transpose()
}

/// Writes every column of a persona that may change during its life.
async fn write(connection: &mut PgConnection, persona: &Persona) -> Result<(), StoreError> {
    const TABLE: &str = "personas";
    sqlx::query!(
        "UPDATE personas
         SET browser_major = $2, user_agent = $3, wreq_profile = $4, country = $5, locale = $6,
             languages = $7, timezone = $8, geolocation = $9, state = $10, cooling_reason = $11,
             warmup_visits = $12, warmed_at = $13, retired_at = $14, retire_reason = $15,
             profile_archived_at = $16, profile_archive = $17, profile_deleted_at = $18
         WHERE id = $1",
        persona.id.as_uuid(),
        to_i32((TABLE, "browser_major"), persona.browser_major)?,
        persona.user_agent,
        persona.wreq_profile,
        persona.country.map(|country| country.as_str().to_owned()),
        persona.locale,
        &persona.languages,
        persona.timezone,
        to_json_opt((TABLE, "geolocation"), persona.geolocation.as_ref())?,
        persona.state.as_str(),
        persona.cooling_reason.map(CoolingReason::as_str),
        to_i32((TABLE, "warmup_visits"), persona.warmup_visits)?,
        persona.warmed_at,
        persona.retired_at,
        persona.retire_reason,
        persona.profile_archived_at,
        persona.profile_archive,
        persona.profile_deleted_at,
    )
    .execute(connection)
    .await?;
    Ok(())
}

repository!(
    /// What each persona has done on each domain: cooldowns and counters.
    PersonaDomainStateRepo
);

struct DomainStateRow {
    persona_id: Uuid,
    domain: String,
    cooldown_until: Option<DateTime<Utc>>,
    requests_today: i32,
    counter_day: Option<NaiveDate>,
    last_request_at: Option<DateTime<Utc>>,
    last_outcome: Option<String>,
    block_count_24h: i32,
    block_window_started_at: Option<DateTime<Utc>>,
}

impl TryFrom<DomainStateRow> for PersonaDomainState {
    type Error = StoreError;

    fn try_from(row: DomainStateRow) -> Result<Self, StoreError> {
        const TABLE: &str = "persona_domain_state";
        Ok(Self {
            persona_id: PersonaId::from_uuid(row.persona_id),
            domain: row.domain,
            cooldown_until: row.cooldown_until,
            requests_today: to_u32((TABLE, "requests_today"), row.requests_today)?,
            counter_day: row.counter_day,
            last_request_at: row.last_request_at,
            last_outcome: parse_opt::<Outcome>(
                (TABLE, "last_outcome"),
                row.last_outcome.as_deref(),
            )?,
            block_count_24h: to_u32((TABLE, "block_count_24h"), row.block_count_24h)?,
            block_window_started_at: row.block_window_started_at,
        })
    }
}

impl PersonaDomainStateRepo<'_> {
    /// The state of one persona on one domain; `None` when it has never
    /// visited.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get(
        &self,
        persona_id: PersonaId,
        domain: &str,
    ) -> Result<Option<PersonaDomainState>, StoreError> {
        sqlx::query_as!(
            DomainStateRow,
            "SELECT persona_id, domain, cooldown_until, requests_today, counter_day,
                    last_request_at, last_outcome, block_count_24h, block_window_started_at
             FROM persona_domain_state WHERE persona_id = $1 AND domain = $2",
            persona_id.as_uuid(),
            domain,
        )
        .fetch_optional(self.pool)
        .await?
        .map(PersonaDomainState::try_from)
        .transpose()
    }

    /// The state of every persona that has visited a domain.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_for_domain(
        &self,
        domain: &str,
    ) -> Result<Vec<PersonaDomainState>, StoreError> {
        sqlx::query_as!(
            DomainStateRow,
            "SELECT persona_id, domain, cooldown_until, requests_today, counter_day,
                    last_request_at, last_outcome, block_count_24h, block_window_started_at
             FROM persona_domain_state WHERE domain = $1 ORDER BY persona_id",
            domain,
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(PersonaDomainState::try_from)
        .collect()
    }

    /// Every domain one persona has visited, by domain.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a row does not parse.
    pub async fn list_for_persona(
        &self,
        persona_id: PersonaId,
    ) -> Result<Vec<PersonaDomainState>, StoreError> {
        sqlx::query_as!(
            DomainStateRow,
            "SELECT persona_id, domain, cooldown_until, requests_today, counter_day,
                    last_request_at, last_outcome, block_count_24h, block_window_started_at
             FROM persona_domain_state WHERE persona_id = $1 ORDER BY domain",
            persona_id.as_uuid(),
        )
        .fetch_all(self.pool)
        .await?
        .into_iter()
        .map(PersonaDomainState::try_from)
        .collect()
    }

    /// Stores the state, replacing what was there for this persona and
    /// domain.
    ///
    /// Two writers that both read, change and `upsert` can overwrite each
    /// other; use [`PersonaDomainStateRepo::modify`] for a change that
    /// depends on what is stored, such as counting a request.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or a counter is out of range.
    pub async fn upsert(&self, state: &PersonaDomainState) -> Result<(), StoreError> {
        let mut connection = self.pool.acquire().await?;
        write_state(&mut connection, state).await
    }

    /// Reads the state of one persona on one domain (a fresh one when it
    /// has never visited), lets `change` alter it, and writes it back, in
    /// one transaction that holds the row lock: two requests counted at
    /// the same moment are both counted.
    ///
    /// # Errors
    ///
    /// Fails when a query fails, the row does not parse or a counter is out
    /// of range (a database error when the persona does not exist); nothing
    /// is written in that case.
    pub async fn modify<T>(
        &self,
        persona_id: PersonaId,
        domain: &str,
        change: impl FnOnce(&mut PersonaDomainState) -> T,
    ) -> Result<T, StoreError> {
        let mut transaction = self.pool.begin().await?;
        // The row must exist before it can be locked.
        sqlx::query!(
            "INSERT INTO persona_domain_state (persona_id, domain) VALUES ($1, $2)
             ON CONFLICT (persona_id, domain) DO NOTHING",
            persona_id.as_uuid(),
            domain,
        )
        .execute(&mut *transaction)
        .await?;
        let mut state: PersonaDomainState = sqlx::query_as!(
            DomainStateRow,
            "SELECT persona_id, domain, cooldown_until, requests_today, counter_day,
                    last_request_at, last_outcome, block_count_24h, block_window_started_at
             FROM persona_domain_state WHERE persona_id = $1 AND domain = $2 FOR UPDATE",
            persona_id.as_uuid(),
            domain,
        )
        .fetch_one(&mut *transaction)
        .await?
        .try_into()?;
        let result = change(&mut state);
        write_state(&mut transaction, &state).await?;
        transaction.commit().await?;
        Ok(result)
    }
}

async fn write_state(
    connection: &mut PgConnection,
    state: &PersonaDomainState,
) -> Result<(), StoreError> {
    const TABLE: &str = "persona_domain_state";
    sqlx::query!(
        "INSERT INTO persona_domain_state (persona_id, domain, cooldown_until, requests_today,
                                           counter_day, last_request_at, last_outcome,
                                           block_count_24h, block_window_started_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (persona_id, domain) DO UPDATE
         SET cooldown_until = EXCLUDED.cooldown_until,
             requests_today = EXCLUDED.requests_today,
             counter_day = EXCLUDED.counter_day,
             last_request_at = EXCLUDED.last_request_at,
             last_outcome = EXCLUDED.last_outcome,
             block_count_24h = EXCLUDED.block_count_24h,
             block_window_started_at = EXCLUDED.block_window_started_at",
        state.persona_id.as_uuid(),
        state.domain,
        state.cooldown_until,
        to_i32((TABLE, "requests_today"), state.requests_today)?,
        state.counter_day,
        state.last_request_at,
        state.last_outcome.map(Outcome::as_str),
        to_i32((TABLE, "block_count_24h"), state.block_count_24h)?,
        state.block_window_started_at,
    )
    .execute(connection)
    .await?;
    Ok(())
}
