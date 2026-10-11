//! Sessions (spec §5, §16.3): handles that pin a key's requests to one
//! persona.
//!
//! A session is opened here and nowhere else. `POST /v1/sessions` answers
//! `201` when the session is new and `200` when the id names one that is
//! open, so a caller always knows when the persona behind an id is a new
//! one: a session that ended (ten idle minutes, six hours in all, or its
//! persona retired) is not reopened by a fetch that still names it.
//!
//! A session never moves. Hints (`persona.countries`, `persona.engines`)
//! and a `domain` whose policy must allow the persona decide which persona
//! a *new* session is pinned to; against an open session they are
//! conditions, and one the persona fails is `session_persona_conflict`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use kage_core::domain::{CountryCode, Engine, Persona, Scope, Session, SessionId};
use kage_core::policy::{EffectivePolicy, normalize_host};
use kage_persona::SessionError;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult, ErrorCode, Problem};
use crate::extract::Body;
use crate::fetch::request::PersonaHints;
use crate::fetch::result::{PersonaView, Viewer};
use crate::state::{ApiState, EngineParts};

/// What a new session is to be.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionRequest {
    /// The id: 1 to 64 characters of `[A-Za-z0-9_.-]`, unique for this
    /// key. Left out: the server makes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(
        max_length = 64,
        pattern = "^[A-Za-z0-9_.-]{1,64}$",
        example = "proj-a-42"
    )]
    pub(crate) id: Option<String>,
    /// Which personas the session may be pinned to. Here both lists are
    /// conditions: the persona is in one of the countries and on one of
    /// the engines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) persona: Option<PersonaHints>,
    /// A host the session is for: the persona is one its policy allows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "example.org")]
    pub(crate) domain: Option<String>,
}

/// A session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct SessionView {
    /// The id; `session_id` of a fetch.
    pub(crate) id: String,
    /// The persona every request of the session is made by.
    pub(crate) persona: PersonaView,
    /// When the session was opened.
    pub(crate) created_at: DateTime<Utc>,
    /// When it last served a request.
    pub(crate) last_used_at: DateTime<Utc>,
    /// When it ends unless it is used before: ten idle minutes, and six
    /// hours after it was opened at the latest (`sessions.idle_ttl`,
    /// `sessions.max_lifetime`).
    pub(crate) expires_at: DateTime<Utc>,
}

fn view(session: &Session, persona: &Persona, viewer: &Viewer<'_>) -> SessionView {
    SessionView {
        id: session.id.to_string(),
        persona: PersonaView {
            id: viewer.name(persona.id),
            engine: persona.engine.into(),
            claimed_os: persona.claimed_os.into(),
            country: persona.country.map(|country| country.to_string()),
        },
        created_at: session.created_at,
        last_used_at: session.last_used_at,
        expires_at: session.expires_at,
    }
}

fn viewer<'a>(state: &'a ApiState, caller: &Caller) -> Viewer<'a> {
    Viewer {
        key: caller.key.id,
        admin: caller.has(Scope::Admin),
        handles: state.handles(),
    }
}

fn engine(state: &ApiState) -> ApiResult<&EngineParts> {
    state.engine().ok_or_else(|| {
        ApiError::new(
            ErrorCode::Unavailable,
            "no proxy pool is configured, so there is no persona to pin a session to",
        )
    })
}

fn session_id(text: &str) -> ApiResult<SessionId> {
    SessionId::new(text)
        .map_err(|error| ApiError::new(ErrorCode::InvalidSessionId, error.to_string()))
}

/// What a session's persona must be.
#[derive(Debug, Clone, Default)]
struct Wanted {
    countries: Vec<CountryCode>,
    engines: Vec<Engine>,
    policy: Option<EffectivePolicy>,
}

impl Wanted {
    fn of(request: &SessionRequest, state: &ApiState) -> ApiResult<Self> {
        let hints = request.persona.clone().unwrap_or_default();
        let mut countries = Vec::with_capacity(hints.countries.len());
        for country in &hints.countries {
            countries.push(country.parse::<CountryCode>().map_err(|_| {
                ApiError::field(
                    "persona.countries",
                    format!("{country:?} is not a two-letter country code"),
                )
            })?);
        }
        let policy = match &request.domain {
            None => None,
            Some(domain) => {
                let host = normalize_host(domain)
                    .ok_or_else(|| ApiError::field("domain", "a host name, without a scheme"))?;
                Some(state.policies().current().domains.resolve(&host))
            }
        };
        Ok(Self {
            countries,
            engines: hints.engines.iter().copied().map(Engine::from).collect(),
            policy,
        })
    }

    /// The first condition a persona fails.
    fn failed_by(&self, persona: &Persona) -> Option<&'static str> {
        if !self.countries.is_empty()
            && !persona
                .country
                .is_some_and(|country| self.countries.contains(&country))
        {
            return Some("persona.countries");
        }
        if !self.engines.is_empty() && !self.engines.contains(&persona.engine) {
            return Some("persona.engines");
        }
        if let Some(policy) = &self.policy {
            if !policy.allows_engine(persona.engine) {
                return Some("policy.engines");
            }
            if !policy.allows_country(persona.country) {
                return Some("policy.countries");
            }
        }
        None
    }
}

/// Open a session, or return the open one.
///
/// `201` for a new session, `200` for one that is open under the id. The
/// persona is the ready one with the fewest open sessions among those the
/// hints and the domain's policy allow.
#[utoipa::path(
    post,
    path = "/v1/sessions",
    tag = "sessions",
    operation_id = "open_session",
    request_body = SessionRequest,
    responses(
        (status = 201, description = "A new session; its persona is new to this id.", body = SessionView),
        (status = 200, description = "The session was open already.", body = SessionView),
        (status = 409, description = "`no_matching_persona`, or `session_persona_conflict` for an open session whose persona the request excludes.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The id or a hint is not well formed.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["sessions"]))
)]
pub(crate) async fn open(
    State(state): State<ApiState>,
    caller: Caller,
    Body(request): Body<SessionRequest>,
) -> ApiResult<Response> {
    caller.require(Scope::Sessions)?;
    let engine = engine(&state)?;
    let id = request.id.as_deref().map(session_id).transpose()?;
    let wanted = Wanted::of(&request, &state)?;

    let pinned = engine
        .personas
        .sessions()
        .open(caller.key.id, id, |persona| {
            wanted.failed_by(persona).is_none()
        })
        .await
        .map_err(|error| match error {
            SessionError::NoPersona => ApiError::new(
                ErrorCode::NoMatchingPersona,
                "no persona that is ready matches the hints and the domain's policy",
            ),
            SessionError::Store(error) => ApiError::internal(&error),
        })?;
    // A session that was open is the persona it has; it is not moved to
    // one the request would rather have.
    if !pinned.created
        && let Some(constraint) = wanted.failed_by(&pinned.persona)
    {
        return Err(ApiError::new(
            ErrorCode::SessionPersonaConflict,
            format!(
                "a session is open under this id and its persona does not satisfy {constraint}; \
                 end it first, or use another id"
            ),
        )
        .with("session_id", pinned.session.id.as_str())
        .with("constraint", constraint));
    }
    let status = if pinned.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let body = view(&pinned.session, &pinned.persona, &viewer(&state, &caller));
    Ok((status, Json(body)).into_response())
}

/// Look at a session. Looking is not using: the session's idle time does
/// not start again.
#[utoipa::path(
    get,
    path = "/v1/sessions/{id}",
    tag = "sessions",
    operation_id = "get_session",
    params(("id" = String, Path, description = "The session's id.")),
    responses(
        (status = 200, description = "The session.", body = SessionView),
        (status = 404, description = "`session_not_found`: never opened, or ended.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["sessions"]))
)]
pub(crate) async fn show(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
) -> ApiResult<Json<SessionView>> {
    caller.require(Scope::Sessions)?;
    let id = session_id(&id)?;
    let not_found = || {
        ApiError::new(
            ErrorCode::SessionNotFound,
            "the key has no open session with this id",
        )
    };
    let session = state
        .store()
        .sessions()
        .get(caller.key.id, &id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .filter(|session| !session.is_expired(state.clock().now()))
        .ok_or_else(not_found)?;
    let persona = state
        .store()
        .personas()
        .get(session.persona_id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .filter(Persona::is_live)
        .ok_or_else(not_found)?;
    Ok(Json(view(&session, &persona, &viewer(&state, &caller))))
}

/// End a session. Its persona goes on; the id is free again.
#[utoipa::path(
    delete,
    path = "/v1/sessions/{id}",
    tag = "sessions",
    operation_id = "end_session",
    params(("id" = String, Path, description = "The session's id.")),
    responses(
        (status = 204, description = "The session ended."),
        (status = 404, description = "`session_not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["sessions"]))
)]
pub(crate) async fn end(
    State(state): State<ApiState>,
    caller: Caller,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    caller.require(Scope::Sessions)?;
    let id = session_id(&id)?;
    let existed = state
        .store()
        .sessions()
        .delete(caller.key.id, &id)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    if existed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            ErrorCode::SessionNotFound,
            "the key has no session with this id",
        ))
    }
}

#[cfg(test)]
mod tests {
    use kage_core::domain::Engine;
    use kage_core::policy::DomainPolicies;

    use super::Wanted;
    use crate::test_support::persona;

    #[test]
    fn hints_and_a_domains_policy_are_conditions_on_a_sessions_persona() {
        let german_chrome = persona(Engine::Chromium, 155);
        let german_firefox = persona(Engine::Firefox, 152);
        let wanted = |countries: &[&str], engines: &[Engine], policy: Option<&str>| Wanted {
            countries: countries
                .iter()
                .map(|country| country.parse().unwrap())
                .collect(),
            engines: engines.to_vec(),
            policy: policy.map(|yaml| {
                DomainPolicies::from_yaml(yaml)
                    .unwrap()
                    .resolve("example.org")
            }),
        };

        assert_eq!(wanted(&[], &[], None).failed_by(&german_chrome), None);
        assert_eq!(
            wanted(&["AT", "DE"], &[Engine::Chromium], None).failed_by(&german_chrome),
            None
        );
        assert_eq!(
            wanted(&["FR"], &[], None).failed_by(&german_chrome),
            Some("persona.countries")
        );
        assert_eq!(
            wanted(&["DE"], &[Engine::Firefox], None).failed_by(&german_chrome),
            Some("persona.engines")
        );
        assert_eq!(
            wanted(&[], &[Engine::Firefox], None).failed_by(&german_firefox),
            None
        );
        let chrome_only = Some("defaults:\n  engines: [chromium]\n");
        assert_eq!(
            wanted(&[], &[], chrome_only).failed_by(&german_firefox),
            Some("policy.engines")
        );
        assert_eq!(
            wanted(&[], &[], chrome_only).failed_by(&german_chrome),
            None
        );
        let italy = Some("domains:\n  example.org:\n    countries: [IT]\n");
        assert_eq!(
            wanted(&[], &[], italy).failed_by(&german_chrome),
            Some("policy.countries")
        );
    }
}
