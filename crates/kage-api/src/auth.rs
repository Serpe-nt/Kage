//! Who is asking (spec §16.2), and the layers every request passes.
//!
//! From the outside in:
//!
//! 1. [`observe`]: the request gets its id (`X-Request-Id` on every
//!    answer, the `request_id` of every problem), a request target that is
//!    too long is refused, and the answer is counted (spec §20).
//! 2. [`authenticate`]: the limit of the whole API, then the key: its
//!    token is read, its row found, the secret compared, and the key's own
//!    limits applied. Every refusal here is counted by its code.
//! 3. [`audit`], on the admin endpoints only: the `admin` scope is asked
//!    for, and whatever comes of the call is written to `audit_events`
//!    with the key, the action, its target and its outcome (spec §19).
//!
//! A handler then asks for the scope it needs ([`Caller::require`]).
//!
//! The layers find the API's state in the request's extensions, where the
//! router puts it, so that the routes can be registered, and the OpenAPI
//! document built, without any state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Extension;
use axum::extract::{FromRequestParts, MatchedPath, Request};
use axum::http::header::{AUTHORIZATION, HeaderName, HeaderValue};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use kage_core::domain::{ApiKey, AuditActor, AuditEvent, AuditOutcome, Scope};
use kage_core::ids::{AuditEventId, RequestId};
use serde_json::{Value, json};
use tracing::Instrument;

use crate::error::{ApiError, ApiResult, Carried, ErrorCode, respond};
use crate::extract::BodyDeadline;
use crate::keys::Presented;
use crate::state::ApiState;

/// The header every answer carries the request's id in.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// How long a key's `last_used_at` may lag: it is written at most this
/// often, not on every request.
const TOUCH_EVERY: Duration = Duration::from_secs(60);

/// The key a request came with, once it was checked.
#[derive(Debug, Clone)]
pub(crate) struct Caller {
    /// The key.
    pub(crate) key: Arc<ApiKey>,
    /// The request's id.
    pub(crate) request: RequestId,
}

impl Caller {
    /// Whether the key carries the scope.
    pub(crate) fn has(&self, scope: Scope) -> bool {
        self.key.allows(scope)
    }

    /// Asks for a scope. Scopes are independent: `admin` implies no other.
    ///
    /// # Errors
    ///
    /// `scope_missing`, naming the scope.
    pub(crate) fn require(&self, scope: Scope) -> ApiResult<()> {
        if self.has(scope) {
            return Ok(());
        }
        Err(ApiError::new(
            ErrorCode::ScopeMissing,
            format!("this endpoint needs the {scope} scope, which the key does not have"),
        )
        .with("scope", scope.as_str()))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(parts.extensions.get::<Self>().cloned().ok_or_else(|| {
            // A route that takes a caller and is not behind `authenticate`.
            ApiError::internal(&"a handler asked for the caller on a route without authentication")
        }))
    }
}

/// The route a request matched, for a metric's label: the pattern, never
/// the path a client sent, so that the label set stays small.
fn route_of(request: &Request) -> String {
    request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |path| path.as_str().to_owned())
}

/// The outermost layer; see the module documentation.
pub(crate) async fn observe(
    Extension(state): Extension<ApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let id = RequestId::new();
    let started = Instant::now();
    let method = request.method().clone();
    let route = route_of(&request);
    // The trace of the request (spec §20): its route as the pattern, never
    // the path a client sent, and the key once it is known.
    let span = tracing::info_span!(
        "api.request",
        otel.kind = "server",
        route = %route,
        method = %method,
        request = %id,
        key = tracing::field::Empty,
        status = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
    );
    request.extensions_mut().insert(id);
    request
        .extensions_mut()
        .insert(BodyDeadline(state.config().server.body_read_timeout.get()));

    let limit = usize::try_from(state.config().server.max_uri_bytes).unwrap_or(usize::MAX);
    let target = request
        .uri()
        .path_and_query()
        .map_or(0, |target| target.as_str().len());
    crate::metrics::in_flight(1.0);
    let mut response = if target > limit {
        crate::metrics::refusal(ErrorCode::UriTooLong.as_str());
        ApiError::new(
            ErrorCode::UriTooLong,
            format!("the request target is longer than {limit} bytes"),
        )
        .into_response()
    } else {
        next.run(request).instrument(span.clone()).await
    };
    crate::metrics::in_flight(-1.0);
    let status = response.status();
    span.record("status", status.as_u16());
    if status.is_server_error() {
        span.record("otel.status_code", "error");
    }

    // A problem learns the request's id here: the one place that knows it
    // for every route, the unmatched ones included.
    if let Some(Carried(error)) = response.extensions_mut().remove::<Carried>() {
        response = respond(&error.for_request(id));
    }
    if let Ok(value) = HeaderValue::from_str(&id.to_string()) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    crate::metrics::request(
        &route,
        method.as_str(),
        response.status().as_u16(),
        started.elapsed(),
    );
    response
}

/// Checks the key a request came with: the value of its `Authorization`
/// header. (The header and not the request: a request's body cannot be
/// shared between threads, so nothing here may hold on to one.)
async fn identify(state: &ApiState, authorization: Option<HeaderValue>) -> ApiResult<ApiKey> {
    state.limits().admit_any()?;
    let header = authorization.ok_or_else(|| {
        ApiError::new(
            ErrorCode::MissingKey,
            "send the key as Authorization: Bearer kage_<keyid>_<secret>",
        )
    })?;
    // One answer for a token that does not parse, a key that does not
    // exist and a secret that is wrong: none of them says which.
    let invalid = || ApiError::new(ErrorCode::InvalidKey, "the API key is not valid");
    let presented = header
        .to_str()
        .ok()
        .and_then(Presented::from_authorization)
        .ok_or_else(invalid)?;
    let key = state
        .store()
        .api_keys()
        .get(presented.id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(invalid)?;
    if !presented.opens(&key) {
        return Err(invalid());
    }
    // Only who holds the secret learns that the key was disabled.
    if !key.is_active() {
        return Err(ApiError::new(
            ErrorCode::KeyDisabled,
            "the API key was disabled",
        ));
    }
    Ok(key)
}

/// The second layer; see the module documentation.
pub(crate) async fn authenticate(
    Extension(state): Extension<ApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let authorization = request.headers().get(AUTHORIZATION).cloned();
    let key = match identify(&state, authorization).await {
        Ok(key) => key,
        Err(error) => {
            crate::metrics::refusal(error.code().as_str());
            return error.into_response();
        }
    };
    tracing::Span::current().record("key", tracing::field::display(key.id));
    let admitted = match state.limits().admit(&key) {
        Ok(admitted) => admitted,
        Err(error) => {
            crate::metrics::refusal(error.code().as_str());
            return error.into_response();
        }
    };
    let now = state.clock().now();
    let overdue = key
        .last_used_at
        .is_none_or(|at| (now - at).to_std().unwrap_or(Duration::ZERO) >= TOUCH_EVERY);
    if overdue && let Err(error) = state.store().api_keys().touch(key.id, now).await {
        tracing::warn!(key = %key.id, %error, "a key's last use could not be recorded");
    }
    let id = request
        .extensions()
        .get::<RequestId>()
        .copied()
        .unwrap_or_default();
    request.extensions_mut().insert(Caller {
        key: Arc::new(key),
        request: id,
    });
    let response = next.run(request).await;
    // The key's request is no longer in flight once its answer has a head.
    drop(admitted);
    response
}

/// What an admin handler adds to its audit event: what it acted on and
/// what came of it. Never a secret.
#[derive(Debug, Clone, Default)]
pub(crate) struct AuditNote {
    /// What the action was done to, for example a persona's id.
    pub(crate) target: Option<String>,
    /// What came of it.
    pub(crate) details: Value,
}

impl AuditNote {
    // Ids are `Copy`; a reference would be noise at every call.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn about(target: impl ToString) -> Self {
        Self {
            target: Some(target.to_string()),
            details: Value::Null,
        }
    }

    #[must_use]
    pub(crate) fn with(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

/// The name of an admin action in the audit trail: `admin.` and what the
/// route does, in the vocabulary the command line already writes
/// (`persona.retire`, `proxy.replace`, `key.create`).
pub(crate) fn action_of(method: &Method, route: &str) -> String {
    let rest = route.strip_prefix("/v1/admin/").unwrap_or(route);
    let name = match (method.as_str(), rest) {
        ("GET", "personas") => "persona.list",
        ("GET", "personas/{id}") => "persona.show",
        ("POST", "personas/{id}/retire") => "persona.retire",
        ("POST", "personas/{id}/regenerate") => "persona.regenerate",
        ("POST", "personas/{id}/selftest") => "persona.selftest",
        ("GET", "proxies") => "proxy.list",
        ("POST", "proxies/{id}/replace") => "proxy.replace",
        ("POST", "proxies/{id}/health") => "proxy.health",
        ("GET", "pool") => "pool.show",
        ("POST", "policies/reload") => "policy.reload",
        ("POST", "sites/{site}/scout") => "site.scout",
        ("GET", "keys") => "key.list",
        ("POST", "keys") => "key.create",
        ("DELETE", "keys/{id}") => "key.revoke",
        ("POST", "keys/{id}/rotate") => "key.rotate",
        ("POST", "purge") => "data.purge",
        ("GET", "audit") => "audit.list",
        _ => return format!("admin.{} {route}", method.as_str().to_ascii_lowercase()),
    };
    format!("admin.{name}")
}

/// What a request names in the place of its route's `{id}` or `{site}`:
/// the target of an admin call whose handler left none, because it refused
/// or failed. A trail that says who tried to retire something should say
/// what.
pub(crate) fn target_of(route: &str, path: &str) -> Option<String> {
    route
        .split('/')
        .zip(path.split('/'))
        .find(|(pattern, _)| matches!(*pattern, "{id}" | "{site}"))
        .map(|(_, named)| named.to_owned())
        .filter(|named| !named.is_empty() && named.len() <= 64)
}

/// The third layer, on the admin endpoints; see the module documentation.
pub(crate) async fn audit(
    Extension(state): Extension<ApiState>,
    caller: Caller,
    request: Request,
    next: Next,
) -> Response {
    let method = request.method().clone();
    let route = route_of(&request);
    let path = request.uri().path().to_owned();
    let mut response = match caller.require(Scope::Admin) {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    };
    let note = response
        .extensions_mut()
        .remove::<AuditNote>()
        .unwrap_or_default();
    let status = response.status();
    let outcome = if status.is_success() || status.is_redirection() {
        AuditOutcome::Success
    } else if status == StatusCode::FORBIDDEN {
        AuditOutcome::Denied
    } else {
        AuditOutcome::Failure
    };
    let code = response
        .extensions()
        .get::<Carried>()
        .map(|Carried(error)| error.code().as_str());
    let event = AuditEvent {
        id: AuditEventId::new(),
        at: state.clock().now(),
        actor: AuditActor::ApiKey(caller.key.id),
        action: action_of(&method, &route),
        target: note.target.or_else(|| target_of(&route, &path)),
        outcome,
        details: json!({
            "request_id": caller.request,
            "path": path,
            "status": status.as_u16(),
            "code": code,
            "result": note.details,
        }),
    };
    if let Err(error) = state.store().audit_events().insert(&event).await {
        // An admin action that leaves no trace is not answered as done.
        tracing::error!(%error, action = event.action, "an audit event could not be written");
        return ApiError::internal(&error).into_response();
    }
    response
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use axum::http::Method;
    use chrono::Utc;
    use kage_core::domain::{ApiKey, KeyLimits, Scope};
    use kage_core::ids::{ApiKeyId, RequestId};

    use super::{AuditNote, Caller, action_of, target_of};
    use crate::error::ErrorCode;

    fn caller(scopes: &[Scope], disabled: bool) -> Caller {
        Caller {
            key: Arc::new(ApiKey {
                id: ApiKeyId::new(),
                name: "test".to_owned(),
                secret_hash: [0; 32],
                scopes: scopes.iter().copied().collect::<BTreeSet<_>>(),
                limits: KeyLimits {
                    requests_per_minute: 60,
                    max_concurrent: 4,
                    daily_page_budget: None,
                },
                disabled_at: disabled.then(Utc::now),
                created_at: Utc::now(),
                last_used_at: None,
            }),
            request: RequestId::new(),
        }
    }

    #[test]
    fn a_scope_is_asked_for_by_name_and_admin_implies_no_other() {
        let consumer = caller(&[Scope::Fetch, Scope::Sessions], false);
        assert!(consumer.require(Scope::Fetch).is_ok());
        assert!(consumer.require(Scope::Sessions).is_ok());
        let refused = consumer.require(Scope::Admin).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::ScopeMissing);
        assert_eq!(refused.problem().extensions["scope"], "admin");

        let admin = caller(&[Scope::Admin], false);
        assert!(admin.require(Scope::Admin).is_ok());
        assert_eq!(
            admin.require(Scope::Fetch).unwrap_err().code(),
            ErrorCode::ScopeMissing
        );
        // A disabled key has no scope at all.
        assert!(!caller(&[Scope::Fetch], true).has(Scope::Fetch));
    }

    #[test]
    fn admin_actions_are_named_as_the_command_line_names_them() {
        for (method, route, action) in [
            (Method::GET, "/v1/admin/personas", "admin.persona.list"),
            (
                Method::POST,
                "/v1/admin/personas/{id}/retire",
                "admin.persona.retire",
            ),
            (
                Method::POST,
                "/v1/admin/personas/{id}/selftest",
                "admin.persona.selftest",
            ),
            (
                Method::POST,
                "/v1/admin/proxies/{id}/replace",
                "admin.proxy.replace",
            ),
            (
                Method::POST,
                "/v1/admin/proxies/{id}/health",
                "admin.proxy.health",
            ),
            (Method::GET, "/v1/admin/pool", "admin.pool.show"),
            (
                Method::POST,
                "/v1/admin/policies/reload",
                "admin.policy.reload",
            ),
            (
                Method::POST,
                "/v1/admin/sites/{site}/scout",
                "admin.site.scout",
            ),
            (Method::POST, "/v1/admin/keys", "admin.key.create"),
            (Method::DELETE, "/v1/admin/keys/{id}", "admin.key.revoke"),
            (
                Method::POST,
                "/v1/admin/keys/{id}/rotate",
                "admin.key.rotate",
            ),
            (Method::GET, "/v1/admin/audit", "admin.audit.list"),
        ] {
            assert_eq!(action_of(&method, route), action);
        }
        // A route nobody named yet still leaves a readable trace.
        assert_eq!(
            action_of(&Method::PUT, "/v1/admin/later"),
            "admin.put /v1/admin/later"
        );
    }

    #[test]
    fn a_call_that_failed_still_names_what_it_was_aimed_at() {
        assert_eq!(
            target_of(
                "/v1/admin/personas/{id}/retire",
                "/v1/admin/personas/0190a0b0-0000-7000-8000-000000000001/retire"
            )
            .as_deref(),
            Some("0190a0b0-0000-7000-8000-000000000001")
        );
        assert_eq!(
            target_of("/v1/admin/keys/{id}", "/v1/admin/keys/nothing").as_deref(),
            Some("nothing")
        );
        assert_eq!(
            target_of(
                "/v1/admin/sites/{site}/scout",
                "/v1/admin/sites/www.example.org/scout"
            )
            .as_deref(),
            Some("www.example.org")
        );
        // A route without an id has no target; what is no id is not kept.
        assert_eq!(target_of("/v1/admin/pool", "/v1/admin/pool"), None);
        assert_eq!(target_of("unmatched", "/v1/admin/keys/x"), None);
        assert_eq!(
            target_of(
                "/v1/admin/keys/{id}",
                &format!("/v1/admin/keys/{}", "x".repeat(65))
            ),
            None
        );
    }

    #[test]
    fn a_note_names_the_target_and_what_came_of_it() {
        let note = AuditNote::about("0199-persona").with(serde_json::json!({ "passed": true }));

        assert_eq!(note.target.as_deref(), Some("0199-persona"));
        assert_eq!(note.details["passed"], true);
        assert!(AuditNote::default().target.is_none());
    }
}
