//! The `personas` and `persona_domain_state` tables.

use chrono::{DateTime, NaiveDate, Utc};
use kage_core::domain::{Persona, PersonaDomainState, PersonaState};
use kage_core::ids::{PersonaId, ProxyId};
use kage_core::outcome::Outcome;
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
    fingerprint: serde_json::Value,
    wreq_profile: Option<String>,
    locale: String,
    languages: Vec<String>,
    timezone: String,
    geolocation: Option<serde_json::Value>,
    screen: serde_json::Value,
    profile_dir: String,
    state: String,
    created_at: DateTime<Utc>,
    retired_at: Option<DateTime<Utc>>,
}

impl TryFrom<PersonaRow> for Persona {
    type Error = StoreError;

    fn try_from(row: PersonaRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: PersonaId::from_uuid(row.id),
            proxy_id: ProxyId::from_uuid(row.proxy_id),
            engine: parse(("personas", "engine"), &row.engine)?,
            claimed_os: parse(("personas", "claimed_os"), &row.claimed_os)?,
            browser_major: to_u32(("personas", "browser_major"), row.browser_major)?,
            fingerprint: row.fingerprint,
            wreq_profile: row.wreq_profile,
            locale: row.locale,
            languages: row.languages,
            timezone: row.timezone,
            geolocation: from_json_opt(("personas", "geolocation"), row.geolocation)?,
            screen: from_json(("personas", "screen"), row.screen)?,
            profile_dir: row.profile_dir,
            state: parse(("personas", "state"), &row.state)?,
            created_at: row.created_at,
            retired_at: row.retired_at,
        })
    }
}

impl PersonaRepo<'_> {
    /// Stores a new persona.
    ///
    /// # Errors
    ///
    /// [`StoreError::Conflict`] when the proxy already has a persona.
    pub async fn insert(&self, persona: &Persona) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO personas (id, proxy_id, engine, claimed_os, browser_major, fingerprint,
                                   wreq_profile, locale, languages, timezone, geolocation, screen,
                                   profile_dir, state, created_at, retired_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
            persona.id.as_uuid(),
            persona.proxy_id.as_uuid(),
            persona.engine.as_str(),
            persona.claimed_os.as_str(),
            to_i32(("personas", "browser_major"), persona.browser_major)?,
            persona.fingerprint,
            persona.wreq_profile,
            persona.locale,
            &persona.languages,
            persona.timezone,
            to_json_opt(("personas", "geolocation"), persona.geolocation.as_ref())?,
            to_json(("personas", "screen"), &persona.screen)?,
            persona.profile_dir,
            persona.state.as_str(),
            persona.created_at,
            persona.retired_at,
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
            "SELECT id, proxy_id, engine, claimed_os, browser_major, fingerprint, wreq_profile,
                    locale, languages, timezone, geolocation, screen, profile_dir, state,
                    created_at, retired_at
             FROM personas WHERE id = $1",
            id.as_uuid(),
        )
        .fetch_optional(self.pool)
        .await?
        .map(Persona::try_from)
        .transpose()
    }

    /// The persona that lives behind this proxy.
    ///
    /// # Errors
    ///
    /// Fails when the query fails or the row does not parse.
    pub async fn get_by_proxy(&self, proxy_id: ProxyId) -> Result<Option<Persona>, StoreError> {
        sqlx::query_as!(
            PersonaRow,
            "SELECT id, proxy_id, engine, claimed_os, browser_major, fingerprint, wreq_profile,
                    locale, languages, timezone, geolocation, screen, profile_dir, state,
                    created_at, retired_at
             FROM personas WHERE proxy_id = $1",
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
            "SELECT id, proxy_id, engine, claimed_os, browser_major, fingerprint, wreq_profile,
                    locale, languages, timezone, geolocation, screen, profile_dir, state,
                    created_at, retired_at
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

    /// Moves a persona to another lifecycle state. Returns whether the
    /// persona exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn set_state(
        &self,
        id: PersonaId,
        state: PersonaState,
        retired_at: Option<DateTime<Utc>>,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE personas SET state = $2, retired_at = $3 WHERE id = $1",
            id.as_uuid(),
            state.as_str(),
            retired_at,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// Records an engine upgrade: the browser major and the wreq profile
    /// always change together (spec §6.2). Returns whether the persona exists.
    ///
    /// # Errors
    ///
    /// Fails when the query fails.
    pub async fn set_browser(
        &self,
        id: PersonaId,
        browser_major: u32,
        wreq_profile: Option<&str>,
    ) -> Result<bool, StoreError> {
        let done = sqlx::query!(
            "UPDATE personas SET browser_major = $2, wreq_profile = $3 WHERE id = $1",
            id.as_uuid(),
            to_i32(("personas", "browser_major"), browser_major)?,
            wreq_profile,
        )
        .execute(self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }
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
                    last_request_at, last_outcome, block_count_24h
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
                    last_request_at, last_outcome, block_count_24h
             FROM persona_domain_state WHERE domain = $1 ORDER BY persona_id",
            domain,
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
    /// # Errors
    ///
    /// Fails when the query fails or a counter is out of range.
    pub async fn upsert(&self, state: &PersonaDomainState) -> Result<(), StoreError> {
        const TABLE: &str = "persona_domain_state";
        sqlx::query!(
            "INSERT INTO persona_domain_state (persona_id, domain, cooldown_until, requests_today,
                                               counter_day, last_request_at, last_outcome,
                                               block_count_24h)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (persona_id, domain) DO UPDATE
             SET cooldown_until = EXCLUDED.cooldown_until,
                 requests_today = EXCLUDED.requests_today,
                 counter_day = EXCLUDED.counter_day,
                 last_request_at = EXCLUDED.last_request_at,
                 last_outcome = EXCLUDED.last_outcome,
                 block_count_24h = EXCLUDED.block_count_24h",
            state.persona_id.as_uuid(),
            state.domain,
            state.cooldown_until,
            to_i32((TABLE, "requests_today"), state.requests_today)?,
            state.counter_day,
            state.last_request_at,
            state.last_outcome.map(Outcome::as_str),
            to_i32((TABLE, "block_count_24h"), state.block_count_24h)?,
        )
        .execute(self.pool)
        .await?;
        Ok(())
    }
}
