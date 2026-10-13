//! `POST /v1/fetch`: one page, fetched while the caller waits (spec §16.3).
//!
//! 1. The request is checked ([`request::plan`]) and the SSRF guard looks
//!    at its URL, with a lookup from this host only when
//!    `security.resolve_on_host` is on.
//! 2. A session, when one is named, must be open, and its persona must be
//!    one the request and the domain's policy allow. A fetch never opens a
//!    session and never moves one: a persona that changed under an id
//!    would show a site one visitor changing address in the middle of a
//!    visit.
//! 3. What the request says about `robots.txt`, the cache and extraction
//!    is settled against the policy of its site ([`terms`]).
//! 4. The page counts against the key's daily budget, unless the response
//!    cache holds it: a page that is not fetched is no page of the budget.
//! 5. The pipeline fetches, or answers from the cache. A block is a result
//!    (`200` with its outcome), not an error.
//! 6. A fetch that outlasts `scheduler.sync_wall_time` runs on as a job:
//!    the answer is `202` with the request's id, under which the result
//!    will be found, and the id of the job that carries it on. From then
//!    on it waits where a job waits and survives a stop of the server.

pub(crate) mod request;
pub(crate) mod result;
pub(crate) mod terms;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{HeaderValue, LOCATION, RETRY_AFTER};
use axum::response::{IntoResponse, Response};
use kage_core::domain::{Persona, RenderMode, Scope, SessionId};
use kage_core::policy::EffectivePolicy;
use kage_persona::identity::http_spec;
use kage_scheduler::{SchedulerError, Synchronous};

use self::request::{FetchRequest, Planned, plan, url_refused};
use self::result::{FetchAccepted, Viewer, from_report, from_stored};
use self::terms::{CacheUse, settle};
use crate::auth::Caller;
use crate::error::{ApiError, ApiResult, ErrorCode, Problem};
use crate::extract::Body;
use crate::resolve::guard;
use crate::state::{ApiState, EngineParts};

/// How long a client that got a `202` is told to wait before it asks for
/// the result.
const POLL_AFTER_SECONDS: u64 = 5;

/// What a session's persona fails, when it is not one a request may use.
pub(crate) fn conflict_of(
    persona: &Persona,
    planned: &Planned,
    policy: &EffectivePolicy,
) -> Option<&'static str> {
    if !planned.countries.is_empty()
        && !persona
            .country
            .is_some_and(|country| planned.countries.contains(&country))
    {
        return Some("request.persona.countries");
    }
    if !planned.engines.is_empty() && !planned.engines.contains(&persona.engine) {
        return Some("request.persona.engines");
    }
    if !policy.allows_engine(persona.engine) {
        return Some("policy.engines");
    }
    if !policy.allows_country(persona.country) {
        return Some("policy.countries");
    }
    // An http render by a persona whose browser has no http profile.
    if planned.order.render == RenderMode::Http && http_spec(persona).is_err() {
        return Some("render");
    }
    None
}

/// The persona of the session a request names.
pub(crate) async fn pinned(
    state: &ApiState,
    engine: &EngineParts,
    caller: &Caller,
    session: &SessionId,
    planned: &Planned,
) -> ApiResult<Persona> {
    caller.require(Scope::Sessions)?;
    let found = engine
        .personas
        .sessions()
        .resolve(caller.key.id, session)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let Some(found) = found else {
        return Err(ApiError::new(
            ErrorCode::SessionNotFound,
            "the key has no open session with this id: it was never opened, or it ended. A \
             fetch opens none; POST /v1/sessions does, and says when the persona behind an id \
             is a new one",
        )
        .with("session_id", session.as_str()));
    };
    let host = planned.order.url.host_str().unwrap_or_default();
    let policy = state.policies().current().domains.resolve(host);
    if let Some(constraint) = conflict_of(&found.persona, planned, &policy) {
        let viewer = Viewer {
            key: caller.key.id,
            admin: caller.has(Scope::Admin),
            handles: state.handles(),
        };
        return Err(ApiError::new(
            ErrorCode::SessionPersonaConflict,
            format!(
                "the session's persona does not satisfy {constraint}; nothing was fetched. A \
                 session never moves to another persona: drop the constraint, or open another \
                 session"
            ),
        )
        .with("session_id", session.as_str())
        .with("constraint", constraint)
        .with("engine", found.persona.engine.as_str())
        .with(
            "country",
            found.persona.country.map(|country| country.to_string()),
        )
        .with("persona", viewer.name(found.persona.id)));
    }
    Ok(found.persona)
}

/// What a failure of the pipeline itself is answered with. A fetch that
/// could not get its page is not one of these: it has a result.
fn failed(error: &SchedulerError) -> ApiError {
    match error {
        SchedulerError::Pacing(pacing) => {
            ApiError::new(ErrorCode::InvalidPacing, pacing.to_string()).with("field", "pacing")
        }
        SchedulerError::NoHost(_) => {
            ApiError::new(ErrorCode::InvalidUrl, "the URL has no host").with("field", "url")
        }
        SchedulerError::ShuttingDown => ApiError::new(
            ErrorCode::Unavailable,
            "the server is stopping; the request was not fetched",
        ),
        other => ApiError::internal(other),
    }
}

/// Fetch one page.
///
/// The answer is `200` with the result whenever a fetch was made, whatever
/// the site did: a block is `outcome`, with `block` saying who and how. A
/// request that cannot be made as asked is a problem document with a
/// stable `code`. A fetch that takes longer than the server's wall time
/// (180 s unless configured) is answered `202` and carried on as a job
/// (`job_id`); `GET /v1/results/{id}` has its result when it ends, also
/// after a restart of the server.
#[utoipa::path(
    post,
    path = "/v1/fetch",
    tag = "fetch",
    operation_id = "fetch",
    request_body = FetchRequest,
    responses(
        (status = 200, description = "The fetch ended; `outcome` says how.", body = result::FetchResult),
        (status = 202, description = "The fetch outlasted the wall time and runs on.", body = FetchAccepted,
            headers(("Location" = String, description = "Where the result will be."),
                    ("Retry-After" = u64, description = "Seconds to wait before asking for it."))),
        (status = 400, description = "The body is not JSON.", body = Problem, content_type = "application/problem+json"),
        (status = 401, description = "No key, or not a valid one.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The key lacks the `fetch` scope, or `sessions` for `session_id`.", body = Problem, content_type = "application/problem+json"),
        (status = 404, description = "`session_not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`session_persona_conflict`.", body = Problem, content_type = "application/problem+json"),
        (status = 422, description = "The request cannot be made as asked; `code` says why.", body = Problem, content_type = "application/problem+json"),
        (status = 429, description = "A limit of the key or of the API; `Retry-After` says when.", body = Problem, content_type = "application/problem+json"),
        (status = 503, description = "Nothing can be fetched: no proxy pool, or the server stops.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["fetch"]))
)]
pub(crate) async fn fetch(
    State(state): State<ApiState>,
    caller: Caller,
    Body(request): Body<FetchRequest>,
) -> ApiResult<Response> {
    caller.require(Scope::Fetch)?;
    let Some(engine) = state.engine() else {
        return Err(ApiError::new(
            ErrorCode::Unavailable,
            "no proxy pool is configured, so nothing can be fetched: Kage never connects to a \
             target directly",
        ));
    };
    let policies = state.policies().current();
    let mut planned = plan(&request, &policies.pacing)?;
    settle(
        &mut planned,
        &request,
        state.config(),
        &policies,
        caller.key.id,
        CacheUse::ReadAndFill,
    )?;
    guard(
        &planned.order.url,
        state.config().security.ssrf(),
        state.resolver(),
    )
    .await
    .map_err(|denial| url_refused(&denial))?;

    let session = planned.session.clone();
    if let Some(session) = &session {
        let persona = pinned(&state, engine, &caller, session, &planned).await?;
        planned.order.pin = Some(persona.id);
    }

    // A page the cache holds is no visit and no page of the budget.
    let held = engine
        .pipeline
        .has_cached(&planned.order)
        .await
        .map_err(|error| failed(&error))?;
    if !held {
        let store = state.store().clone();
        let key = caller.key.id;
        state
            .limits()
            .spend_page(&caller.key, state.clock().now(), async move |since| {
                store
                    .requests()
                    .count_for_key_since(key, since)
                    .await
                    .map_err(|error| ApiError::internal(&error))
            })
            .await?;
    }

    let mut order = planned.order;
    order.api_key = Some(caller.key.id);
    // The request as it was accepted, for the record. It holds no secret:
    // a caller cannot send one (cookies and credentials are refused).
    order.spec = serde_json::to_value(&request).map_err(|error| ApiError::internal(&error))?;
    let spec = order.spec.clone();
    let site = order
        .url
        .host_str()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let viewer = Viewer {
        key: caller.key.id,
        admin: caller.has(Scope::Admin),
        handles: state.handles(),
    };
    match engine
        .pipeline
        .fetch_sync_as(caller.request, order)
        .await
        .map_err(|error| failed(&error))?
    {
        Synchronous::Finished(report) => {
            let session = session.as_ref().map(SessionId::as_str);
            let result = match report.cached.as_deref() {
                // Served from the cache: the stored result of the visit
                // that filled it, with the bodies it kept.
                Some(record) => {
                    let bodies = crate::results::bodies(&state, record, &planned.include).await?;
                    from_stored(record, &[], bodies, &planned.include, session, &viewer)
                }
                None => from_report(&report, &planned.include, session, &viewer),
            };
            Ok(Json(result).into_response())
        }
        Synchronous::Overflowed(running) => {
            // It is a job's from here on: it waits where a job waits, and
            // is taken up again after a stop of the server.
            let request = running.request;
            let job = crate::jobs::worker::carry_on(&state, &caller.key, running, spec, site)
                .await
                .map_err(|error| ApiError::internal(&error))?;
            let accepted = FetchAccepted::carried(request, job);
            let mut response = (StatusCode::ACCEPTED, Json(&accepted)).into_response();
            if let Ok(location) = HeaderValue::from_str(&accepted.result) {
                response.headers_mut().insert(LOCATION, location);
            }
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(POLL_AFTER_SECONDS));
            Ok(response)
        }
    }
}

#[cfg(test)]
mod tests {
    use kage_core::domain::Engine;
    use kage_core::pacing::PacingProfiles;
    use kage_core::policy::DomainPolicies;
    use kage_scheduler::SchedulerError;
    use serde_json::json;

    use super::request::{FetchRequest, plan};
    use super::{conflict_of, failed};
    use crate::error::ErrorCode;
    use crate::test_support::persona;

    const HUMAN: &str = "
human:
  gap_ms: { dist: lognormal, median: 4500, p95: 15000, min: 1500 }
  burst_pages: { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 60000, p95: 240000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
";

    fn planned(body: serde_json::Value) -> super::Planned {
        let request: FetchRequest = serde_json::from_value(body).unwrap();
        plan(&request, &PacingProfiles::from_yaml(HUMAN).unwrap()).unwrap()
    }

    #[test]
    fn a_sessions_persona_must_be_one_the_request_and_the_policy_allow() {
        // A chromium persona on linux in Germany, without an http profile.
        let german = persona(Engine::Chromium, 155);
        let open = DomainPolicies::default().resolve("example.org");
        let url = "https://example.org/";

        // Nothing asked: it fits; hints it satisfies: it fits.
        assert_eq!(
            conflict_of(&german, &planned(json!({ "url": url })), &open),
            None
        );
        assert_eq!(
            conflict_of(
                &german,
                &planned(json!({
                    "url": url,
                    "persona": { "countries": ["AT", "DE"], "engines": ["chromium"] },
                })),
                &open
            ),
            None
        );
        // What the request asks for and the persona is not.
        assert_eq!(
            conflict_of(
                &german,
                &planned(json!({ "url": url, "persona": { "countries": ["FR"] } })),
                &open
            ),
            Some("request.persona.countries")
        );
        assert_eq!(
            conflict_of(
                &german,
                &planned(json!({ "url": url, "persona": { "engines": ["firefox"] } })),
                &open
            ),
            Some("request.persona.engines")
        );
        // What the domain's policy excludes.
        let firefox_only = DomainPolicies::from_yaml("defaults:\n  engines: [firefox]\n")
            .unwrap()
            .resolve("example.org");
        assert_eq!(
            conflict_of(&german, &planned(json!({ "url": url })), &firefox_only),
            Some("policy.engines")
        );
        let french = DomainPolicies::from_yaml("defaults:\n  countries: [FR]\n")
            .unwrap()
            .resolve("example.org");
        assert_eq!(
            conflict_of(&german, &planned(json!({ "url": url })), &french),
            Some("policy.countries")
        );
        // A browser-only persona cannot make an http render.
        assert_eq!(
            conflict_of(
                &german,
                &planned(json!({ "url": url, "render": "http" })),
                &open
            ),
            Some("render")
        );
    }

    #[test]
    fn a_failure_of_the_pipeline_is_told_without_its_cause() {
        assert_eq!(
            failed(&SchedulerError::NoHost("data:x".to_owned())).code(),
            ErrorCode::InvalidUrl
        );
        assert_eq!(
            failed(&SchedulerError::ShuttingDown).code(),
            ErrorCode::Unavailable
        );
    }
}
