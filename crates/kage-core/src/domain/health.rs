//! Health events, self-test runs and per-domain circuit breakers
//! (spec §7.4, §8.3, §18).

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{Engine, Render};
use crate::alert::AlertSeverity;
use crate::ids::{HealthEventId, PersonaId, SelftestRunId};
use crate::macros::string_enum;

string_enum! {
    /// What a health event is about.
    pub enum SubjectKind {
        /// A proxy.
        Proxy => "proxy",
        /// A persona.
        Persona => "persona",
        /// The policy files.
        Policy => "policy",
        /// A browser sidecar.
        Sidecar => "sidecar",
        /// Kage as a whole.
        System => "system",
    }
}

/// Something that happened to the health of a proxy, a persona, the policy
/// files or the system. Operator alerts are stored as health events too.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthEvent {
    /// Identity.
    pub id: HealthEventId,
    /// When it happened.
    pub at: DateTime<Utc>,
    /// What kind of thing it is about.
    pub subject_kind: SubjectKind,
    /// Which one, when the subject has an id.
    pub subject_id: Option<Uuid>,
    /// What happened, as a stable `snake_case` name.
    pub kind: String,
    /// How urgent it is.
    pub severity: AlertSeverity,
    /// Structured context. Never contains secrets.
    pub details: serde_json::Value,
}

/// One run of a fingerprint self-test (spec §7.4).
#[derive(Debug, Clone, PartialEq)]
pub struct SelftestRun {
    /// Identity.
    pub id: SelftestRunId,
    /// The persona under test.
    pub persona_id: PersonaId,
    /// Its engine.
    pub engine: Engine,
    /// Which path was tested.
    pub render: Render,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run finished.
    pub finished_at: DateTime<Utc>,
    /// Whether every assertion held.
    pub passed: bool,
    /// Detection score for the run, when the test produces one.
    pub score: Option<f32>,
    /// What each detection page reported.
    pub findings: serde_json::Value,
}

string_enum! {
    /// Circuit breaker position.
    pub enum BreakerState {
        /// Requests flow.
        Closed => "closed",
        /// The domain is paused; requests fail fast.
        Open => "open",
    }
}

/// Circuit breaker for one domain (spec §8.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainBreaker {
    /// The domain.
    pub domain: String,
    /// Stored position; see [`DomainBreaker::is_open`] for the effective one.
    pub state: BreakerState,
    /// Blocks over all personas in the current one-hour window.
    pub block_count_1h: u32,
    /// Start of the current window.
    pub window_started_at: Option<DateTime<Utc>>,
    /// End of the pause, when open.
    pub paused_until: Option<DateTime<Utc>>,
    /// How often the breaker opened since the domain last answered well:
    /// every time doubles the next pause.
    pub trips: u32,
    /// When the one trial request after a pause was let through; `None`
    /// while none is under way.
    pub trial_started_at: Option<DateTime<Utc>>,
    /// Last change.
    pub updated_at: DateTime<Utc>,
}

impl DomainBreaker {
    /// A closed breaker with no blocks counted.
    pub fn closed(domain: impl Into<String>, now: DateTime<Utc>) -> Self {
        Self {
            domain: domain.into(),
            state: BreakerState::Closed,
            block_count_1h: 0,
            window_started_at: None,
            paused_until: None,
            trips: 0,
            trial_started_at: None,
            updated_at: now,
        }
    }

    /// Whether the pause of the domain still runs at `now`. An open breaker
    /// whose pause has run out is half open: one trial request decides
    /// whether it closes (`crate::schedule::breaker`).
    pub fn is_open(&self, now: DateTime<Utc>) -> bool {
        self.state == BreakerState::Open && self.paused_until.is_none_or(|until| until > now)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::{BreakerState, DomainBreaker, SubjectKind};
    use crate::macros::test_support::assert_string_enum;

    #[test]
    fn enum_names_are_stable() {
        assert_string_enum(
            SubjectKind::ALL,
            SubjectKind::as_str,
            &["proxy", "persona", "policy", "sidecar", "system"],
        );
        assert_string_enum(BreakerState::ALL, BreakerState::as_str, &["closed", "open"]);
    }

    #[test]
    fn an_open_breaker_closes_when_its_pause_ends() {
        let now = Utc::now();
        let mut breaker = DomainBreaker::closed("example.org", now);
        assert!(!breaker.is_open(now));

        breaker.state = BreakerState::Open;
        breaker.paused_until = Some(now + Duration::minutes(30));
        assert!(breaker.is_open(now));
        assert!(breaker.is_open(now + Duration::minutes(29)));
        assert!(!breaker.is_open(now + Duration::minutes(30)));

        // Open without a deadline stays open until someone closes it.
        breaker.paused_until = None;
        assert!(breaker.is_open(now + Duration::days(1)));
    }
}
