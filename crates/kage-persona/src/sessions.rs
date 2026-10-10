//! Sessions (spec §5): handles that pin a caller's requests to one persona.
//!
//! A session id is the caller's or minted by the server, and unique per
//! API key: two keys may both call a session `checkout` and never meet. A
//! session is pinned to one persona when it is opened and never moves; it
//! ends after `sessions.idle_ttl` without use, `sessions.max_lifetime`
//! after it was opened, or at once when its persona is retired. An id
//! whose session has ended names a new session, on whichever persona is
//! chosen then.
//!
//! A new session goes to a persona that is ready (active and warmed up),
//! the one with the fewest open sessions; among equals the session id
//! decides, so that the first persona of the list is not everyone's. A
//! persona that cools gets no new sessions and its open ones go on
//! (spec §6.3).

use std::sync::Arc;

use kage_core::domain::{Persona, Session, SessionId, SessionLimits};
use kage_core::ids::{ApiKeyId, PersonaId};
use kage_core::seed::Seed;
use kage_proxy::Clock;
use kage_store::{Store, StoreError};

/// Why a session could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// No persona is ready and allowed for a new session.
    #[error("no persona is ready for a new session")]
    NoPersona,
}

/// A session with the persona it is pinned to.
#[derive(Debug, Clone, PartialEq)]
pub struct Pinned {
    /// The session, with its use at this moment recorded.
    pub session: Session,
    /// The persona it is pinned to.
    pub persona: Persona,
    /// Whether the session was opened by this call.
    pub created: bool,
}

/// The sessions service.
#[derive(Debug, Clone)]
pub struct Sessions {
    store: Store,
    clock: Arc<dyn Clock>,
    limits: SessionLimits,
}

impl Sessions {
    /// A service over `store` with the given limits.
    pub fn new(store: Store, clock: Arc<dyn Clock>, limits: SessionLimits) -> Self {
        Self {
            store,
            clock,
            limits,
        }
    }

    /// The limits sessions live by.
    pub const fn limits(&self) -> SessionLimits {
        self.limits
    }

    /// A key's open session with this id, with a use recorded: its idle
    /// time starts again. `None` when there is none or it has ended.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn resolve(
        &self,
        key: ApiKeyId,
        id: &SessionId,
    ) -> Result<Option<Pinned>, StoreError> {
        let now = self.clock.now();
        let Some(mut session) = self.store.sessions().get(key, id).await? else {
            return Ok(None);
        };
        if session.is_expired(now) {
            self.store.sessions().delete(key, id).await?;
            return Ok(None);
        }
        // A session never outlives its persona: retiring one deletes its
        // sessions. One that is found here anyway has ended.
        let persona = self.store.personas().get(session.persona_id).await?;
        let Some(persona) = persona.filter(Persona::is_live) else {
            self.store.sessions().delete(key, id).await?;
            return Ok(None);
        };
        session.touch(now, self.limits);
        if !self.store.sessions().touch(&session).await? {
            return Ok(None);
        }
        Ok(Some(Pinned {
            session,
            persona,
            created: false,
        }))
    }

    /// The key's session with this id, opened when there is none: pinned
    /// to the ready persona with the fewest open sessions among those
    /// `allowed` lets through. Without an id the server mints one.
    ///
    /// # Errors
    ///
    /// [`SessionError::NoPersona`] when no persona is ready and allowed;
    /// fails when the store fails.
    pub async fn open(
        &self,
        key: ApiKeyId,
        id: Option<SessionId>,
        allowed: impl Fn(&Persona) -> bool,
    ) -> Result<Pinned, SessionError> {
        if let Some(id) = &id
            && let Some(pinned) = self.resolve(key, id).await?
        {
            return Ok(pinned);
        }
        let id = id.unwrap_or_else(SessionId::mint);
        let now = self.clock.now();
        let mut chosen: Option<(u64, u64, Persona)> = None;
        for persona in self.store.personas().list_live().await? {
            if !persona.is_ready() || !allowed(&persona) {
                continue;
            }
            let load = self
                .store
                .sessions()
                .count_for_persona(persona.id, now)
                .await?;
            let rank = (load, tie_break(key, &id, persona.id));
            if chosen
                .as_ref()
                .is_none_or(|(load, tie, _)| rank < (*load, *tie))
            {
                chosen = Some((rank.0, rank.1, persona));
            }
        }
        let Some((_, _, persona)) = chosen else {
            return Err(SessionError::NoPersona);
        };
        let session = Session::open(id.clone(), key, persona.id, now, self.limits);
        match self.store.sessions().insert(&session).await {
            Ok(()) => Ok(Pinned {
                session,
                persona,
                created: true,
            }),
            // The same id was opened at the same moment: that session it
            // is.
            Err(StoreError::Conflict { .. }) => {
                self.resolve(key, &id).await?.ok_or(SessionError::NoPersona)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Ends a session. Returns whether it existed.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn end(&self, key: ApiKeyId, id: &SessionId) -> Result<bool, StoreError> {
        self.store.sessions().delete(key, id).await
    }

    /// Removes every session that has ended. Returns how many.
    ///
    /// # Errors
    ///
    /// Fails when the store fails.
    pub async fn sweep(&self) -> Result<u64, StoreError> {
        self.store.sessions().delete_expired(self.clock.now()).await
    }
}

/// The order among personas with the same load: a number that depends on
/// the session and the persona only, so that it can be replayed.
fn tie_break(key: ApiKeyId, id: &SessionId, persona: PersonaId) -> u64 {
    Seed::derive(
        "session-pin",
        &[
            key.as_uuid().as_bytes(),
            id.as_str().as_bytes(),
            persona.as_uuid().as_bytes(),
        ],
    )
    .rng()
    .next_u64()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use kage_core::domain::SessionId;
    use kage_core::ids::{ApiKeyId, PersonaId};

    use super::tie_break;

    #[test]
    fn equal_personas_are_ordered_by_the_session_and_not_by_the_list() {
        let key = ApiKeyId::new();
        let personas: Vec<PersonaId> = (0..8).map(|_| PersonaId::new()).collect();
        let first_of = |id: &SessionId| {
            personas
                .iter()
                .copied()
                .min_by_key(|persona| tie_break(key, id, *persona))
                .unwrap()
        };
        let a = SessionId::new("checkout").unwrap();
        // The same session always gets the same order.
        assert_eq!(first_of(&a), first_of(&a));
        // Many sessions spread over the personas.
        let winners: BTreeSet<PersonaId> = (0..64)
            .map(|n| first_of(&SessionId::new(format!("s-{n}")).unwrap()))
            .collect();
        assert!(winners.len() >= 6, "{} personas won", winners.len());
        // Another key's session of the same name is another session.
        let other = ApiKeyId::new();
        assert_ne!(
            tie_break(key, &a, personas[0]),
            tie_break(other, &a, personas[0])
        );
    }
}
