//! Typed identifiers. Every id is a UUIDv7 (spec §5), so ids sort by creation
//! time and never collide across tables.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Mints a new id from the current time (UUIDv7).
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID, for example one read from the store.
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl From<Uuid> for $name {
            fn from(uuid: Uuid) -> Self {
                Self(uuid)
            }
        }

        impl From<$name> for Uuid {
            fn from(id: $name) -> Self {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(text: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(text).map(Self)
            }
        }
    };
}

define_id!(
    /// Identifies a [`Proxy`](crate::domain::Proxy).
    ProxyId
);
define_id!(
    /// Identifies a [`Persona`](crate::domain::Persona).
    PersonaId
);
define_id!(
    /// Identifies an [`ApiKey`](crate::domain::ApiKey). It is also the
    /// `<keyid>` part of a bearer token (spec §16.2).
    ApiKeyId
);
define_id!(
    /// Identifies one fetch request, whether synchronous or part of a job.
    RequestId
);
define_id!(
    /// Identifies an [`Attempt`](crate::domain::Attempt).
    AttemptId
);
define_id!(
    /// Identifies a stored [`FetchRecord`](crate::domain::FetchRecord).
    ResultId
);
define_id!(
    /// Identifies an [`Artifact`](crate::domain::Artifact).
    ArtifactId
);
define_id!(
    /// Identifies a [`Job`](crate::domain::Job).
    JobId
);
define_id!(
    /// Identifies a [`JobItem`](crate::domain::JobItem).
    JobItemId
);
define_id!(
    /// Identifies a [`FrontierEntry`](crate::domain::FrontierEntry).
    FrontierId
);
define_id!(
    /// Identifies an [`AuditEvent`](crate::domain::AuditEvent).
    AuditEventId
);
define_id!(
    /// Identifies a [`HealthEvent`](crate::domain::HealthEvent).
    HealthEventId
);
define_id!(
    /// Identifies a [`SelftestRun`](crate::domain::SelftestRun).
    SelftestRunId
);

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::{PersonaId, ProxyId};

    #[test]
    fn new_ids_are_uuid_v7_and_distinct() {
        let first = ProxyId::new();
        let second = ProxyId::default();

        assert_ne!(first, second);
        assert_eq!(first.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn ids_minted_later_sort_later() {
        let first = PersonaId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = PersonaId::new();

        assert!(first < second);
    }

    #[test]
    fn text_and_json_round_trip() {
        let id = ProxyId::new();

        let parsed: ProxyId = id.to_string().parse().unwrap();
        assert_eq!(parsed, id);

        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<ProxyId>(&json).unwrap(), id);

        assert!("not-a-uuid".parse::<ProxyId>().is_err());
    }

    #[test]
    fn converts_to_and_from_uuid() {
        let uuid = Uuid::now_v7();

        let id = ProxyId::from(uuid);
        assert_eq!(id, ProxyId::from_uuid(uuid));
        assert_eq!(Uuid::from(id), uuid);
    }
}
