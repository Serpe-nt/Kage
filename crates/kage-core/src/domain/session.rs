//! Sessions: caller-visible handles that pin requests to one persona
//! (spec §5).

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::ids::{ApiKeyId, PersonaId};

/// Longest session id a caller may supply.
const MAX_SESSION_ID_LEN: usize = 64;

/// A session id: 1 to 64 characters from `[A-Za-z0-9_.-]`, supplied by the
/// caller or minted by the server.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionId(String);

/// A session id that breaks the length or character rules.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("session id must be 1 to 64 characters from [A-Za-z0-9_.-]")]
pub struct InvalidSessionId;

impl SessionId {
    /// Validates a caller-supplied id.
    ///
    /// # Errors
    ///
    /// Fails when the id is empty, longer than 64 characters or contains a
    /// character outside `[A-Za-z0-9_.-]`.
    pub fn new(text: impl Into<String>) -> Result<Self, InvalidSessionId> {
        let text = text.into();
        let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
        if text.is_empty() || text.len() > MAX_SESSION_ID_LEN || !text.chars().all(allowed) {
            return Err(InvalidSessionId);
        }
        Ok(Self(text))
    }

    /// Mints a server-side id.
    pub fn mint() -> Self {
        Self(format!("s-{}", Uuid::now_v7().simple()))
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SessionId {
    type Err = InvalidSessionId;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::new(text)
    }
}

impl Serialize for SessionId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::new(text).map_err(serde::de::Error::custom)
    }
}

/// How long sessions live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    /// A session ends after this long without use.
    pub idle_ttl: Duration,
    /// A session ends this long after creation, however busy it is.
    pub max_lifetime: Duration,
}

impl Default for SessionLimits {
    /// Spec §5: idle TTL 10 minutes, maximum lifetime 6 hours.
    fn default() -> Self {
        Self {
            idle_ttl: Duration::from_mins(10),
            max_lifetime: Duration::from_hours(6),
        }
    }
}

/// A handle that pins subsequent requests to one persona for a TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The id, unique per API key.
    pub id: SessionId,
    /// The key that owns the session.
    pub api_key_id: ApiKeyId,
    /// The pinned persona.
    pub persona_id: PersonaId,
    /// When the session was opened.
    pub created_at: DateTime<Utc>,
    /// When the session last served a request.
    pub last_used_at: DateTime<Utc>,
    /// When the session ends unless it is used again.
    pub expires_at: DateTime<Utc>,
}

impl Session {
    /// Opens a session at `now`.
    pub fn open(
        id: SessionId,
        api_key_id: ApiKeyId,
        persona_id: PersonaId,
        now: DateTime<Utc>,
        limits: SessionLimits,
    ) -> Self {
        let mut session = Self {
            id,
            api_key_id,
            persona_id,
            created_at: now,
            last_used_at: now,
            expires_at: now,
        };
        session.touch(now, limits);
        session
    }

    /// Records a use at `now` and pushes the expiry out by the idle TTL,
    /// never past the maximum lifetime.
    pub fn touch(&mut self, now: DateTime<Utc>, limits: SessionLimits) {
        self.last_used_at = now;
        self.expires_at = (now + limits.idle_ttl).min(self.created_at + limits.max_lifetime);
    }

    /// Whether the session has ended at `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::Utc;

    use super::{InvalidSessionId, Session, SessionId, SessionLimits};
    use crate::ids::{ApiKeyId, PersonaId};

    #[test]
    fn caller_ids_follow_the_charset_and_length_rules() {
        assert_eq!(SessionId::new("proj-a-42").unwrap().as_str(), "proj-a-42");
        assert!(SessionId::new("A.b_c-9").is_ok());
        assert!(SessionId::new("x".repeat(64)).is_ok());

        for bad in ["", "has space", "slash/no", "ünï", "semi;colon"] {
            assert_eq!(SessionId::new(bad), Err(InvalidSessionId), "{bad:?}");
        }
        assert_eq!(SessionId::new("x".repeat(65)), Err(InvalidSessionId));
    }

    #[test]
    fn minted_ids_are_valid_and_distinct() {
        let first = SessionId::mint();
        let second = SessionId::mint();

        assert_ne!(first, second);
        assert!(SessionId::new(first.as_str()).is_ok());
        assert!(first.as_str().starts_with("s-"));
    }

    #[test]
    fn text_and_json_round_trip() {
        let id: SessionId = "crawl.7".parse().unwrap();

        assert_eq!(id.to_string(), "crawl.7");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"crawl.7\"");
        assert_eq!(
            serde_json::from_str::<SessionId>("\"crawl.7\"").unwrap(),
            id
        );
        assert!(serde_json::from_str::<SessionId>("\"no way\"").is_err());
    }

    #[test]
    fn default_limits_match_the_spec() {
        let limits = SessionLimits::default();

        assert_eq!(limits.idle_ttl, Duration::from_secs(600));
        assert_eq!(limits.max_lifetime, Duration::from_secs(21_600));
    }

    #[test]
    fn use_extends_the_session_up_to_its_maximum_lifetime() {
        let limits = SessionLimits::default();
        let start = Utc::now();
        let mut session = Session::open(
            SessionId::mint(),
            ApiKeyId::new(),
            PersonaId::new(),
            start,
            limits,
        );

        assert_eq!(session.expires_at, start + limits.idle_ttl);
        assert!(!session.is_expired(start));
        assert!(session.is_expired(start + limits.idle_ttl));

        // Used five minutes in: ten more minutes from then.
        let later = start + Duration::from_secs(300);
        session.touch(later, limits);
        assert_eq!(session.last_used_at, later);
        assert_eq!(session.expires_at, later + limits.idle_ttl);

        // Used just before the maximum lifetime: capped at created + 6 h.
        let near_end = start + limits.max_lifetime - Duration::from_secs(60);
        session.touch(near_end, limits);
        assert_eq!(session.expires_at, start + limits.max_lifetime);
        assert!(session.is_expired(start + limits.max_lifetime));
    }
}
