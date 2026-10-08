//! API keys and the audit trail (spec §16.2, §19).

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::ids::{ApiKeyId, AuditEventId};
use crate::macros::string_enum;

string_enum! {
    /// What an API key may do (spec §16.2).
    pub enum Scope {
        /// Synchronous fetches, results, artifacts, policy preview.
        Fetch => "fetch",
        /// Batch and flow jobs.
        Jobs => "jobs",
        /// Crawl jobs.
        Crawl => "crawl",
        /// Sessions.
        Sessions => "sessions",
        /// Administration.
        Admin => "admin",
    }
}

/// Per-key limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyLimits {
    /// Requests per minute.
    pub requests_per_minute: u32,
    /// Requests in flight at the same time.
    pub max_concurrent: u32,
    /// Pages per day; `None` means no budget.
    pub daily_page_budget: Option<u32>,
}

/// A consumer's API key. Only the SHA-256 of the secret is stored; the
/// secret itself is shown once at creation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKey {
    /// Identity; the `<keyid>` part of the bearer token.
    pub id: ApiKeyId,
    /// A label for humans.
    pub name: String,
    /// SHA-256 of the secret.
    pub secret_hash: [u8; 32],
    /// What the key may do.
    pub scopes: BTreeSet<Scope>,
    /// Its limits.
    pub limits: KeyLimits,
    /// When the key was disabled, if it was.
    pub disabled_at: Option<DateTime<Utc>>,
    /// When the key was created.
    pub created_at: DateTime<Utc>,
    /// When the key last authenticated a request.
    pub last_used_at: Option<DateTime<Utc>>,
}

impl ApiKey {
    /// Whether the key may authenticate requests.
    pub const fn is_active(&self) -> bool {
        self.disabled_at.is_none()
    }

    /// Whether the key is active and carries `scope`. Scopes are independent:
    /// `admin` does not imply the others.
    pub fn allows(&self, scope: Scope) -> bool {
        self.is_active() && self.scopes.contains(&scope)
    }
}

/// Who performed an audited action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditActor {
    /// An API call authenticated with this key.
    ApiKey(ApiKeyId),
    /// An operator at the command line.
    Cli,
    /// Kage itself, for example the replacement loop.
    System,
}

impl AuditActor {
    /// The stable name of the actor kind.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ApiKey(_) => "api_key",
            Self::Cli => "cli",
            Self::System => "system",
        }
    }

    /// The key, when the actor is one.
    pub const fn api_key_id(&self) -> Option<ApiKeyId> {
        match self {
            Self::ApiKey(id) => Some(*id),
            Self::Cli | Self::System => None,
        }
    }
}

string_enum! {
    /// How an audited action ended.
    pub enum AuditOutcome {
        /// It was carried out.
        Success => "success",
        /// It was attempted and failed.
        Failure => "failure",
        /// It was refused.
        Denied => "denied",
    }
}

/// One entry in the audit trail: every admin API call and CLI mutation
/// writes one (spec §19).
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEvent {
    /// Identity.
    pub id: AuditEventId,
    /// When it happened.
    pub at: DateTime<Utc>,
    /// Who did it.
    pub actor: AuditActor,
    /// What was done, for example `persona.retire`.
    pub action: String,
    /// What it was done to, for example a persona id.
    pub target: Option<String>,
    /// How it ended.
    pub outcome: AuditOutcome,
    /// Extra context. Never contains secrets.
    pub details: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::Utc;

    use super::{ApiKey, AuditActor, AuditOutcome, KeyLimits, Scope};
    use crate::ids::ApiKeyId;
    use crate::macros::test_support::assert_string_enum;

    fn key(scopes: &[Scope]) -> ApiKey {
        ApiKey {
            id: ApiKeyId::new(),
            name: "project-a".to_owned(),
            secret_hash: [9; 32],
            scopes: scopes.iter().copied().collect::<BTreeSet<_>>(),
            limits: KeyLimits {
                requests_per_minute: 60,
                max_concurrent: 4,
                daily_page_budget: None,
            },
            disabled_at: None,
            created_at: Utc::now(),
            last_used_at: None,
        }
    }

    #[test]
    fn enum_names_are_the_spec_names() {
        assert_string_enum(
            Scope::ALL,
            Scope::as_str,
            &["fetch", "jobs", "crawl", "sessions", "admin"],
        );
        assert_string_enum(
            AuditOutcome::ALL,
            AuditOutcome::as_str,
            &["success", "failure", "denied"],
        );
    }

    #[test]
    fn scopes_are_independent() {
        let key = key(&[Scope::Fetch, Scope::Sessions]);

        assert!(key.allows(Scope::Fetch));
        assert!(key.allows(Scope::Sessions));
        assert!(!key.allows(Scope::Jobs));

        let admin = self::key(&[Scope::Admin]);
        assert!(admin.allows(Scope::Admin));
        assert!(!admin.allows(Scope::Fetch));
    }

    #[test]
    fn a_disabled_key_allows_nothing() {
        let mut key = key(&[Scope::Fetch]);
        assert!(key.is_active());

        key.disabled_at = Some(Utc::now());

        assert!(!key.is_active());
        assert!(!key.allows(Scope::Fetch));
    }

    #[test]
    fn actors_expose_kind_and_key() {
        let id = ApiKeyId::new();

        assert_eq!(AuditActor::ApiKey(id).kind(), "api_key");
        assert_eq!(AuditActor::ApiKey(id).api_key_id(), Some(id));
        assert_eq!(AuditActor::Cli.kind(), "cli");
        assert_eq!(AuditActor::Cli.api_key_id(), None);
        assert_eq!(AuditActor::System.kind(), "system");
        assert_eq!(AuditActor::System.api_key_id(), None);
    }
}
