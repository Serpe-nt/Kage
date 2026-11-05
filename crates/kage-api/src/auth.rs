//! Who is asking (spec §16.2), and the layers every request passes.
//!
//! From the outside in:
//!
//! 1. [`observe`]: the request gets its id (`X-Request-Id` on every
//!    answer, the `request_id` of every problem); a head over the limits
//!    of spec §19 (`431`, `414`) and a body that says it is too large
//!    (`413`) are refused; an error that a layer below answered in plain
//!    text becomes a problem; and the answer is counted (spec §20), by its
//!    route's pattern and a method HTTP defines.
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
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use kage_core::config::ServerConfig;
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
    /// A refusal is counted under `kage_api_refusals_total{code="scope_missing"}`
    /// with the refusals of the key's layer, though the scope is asked for by
    /// the handler (F182).
    ///
    /// # Errors
    ///
    /// `scope_missing`, naming the scope.
    pub(crate) fn require(&self, scope: Scope) -> ApiResult<()> {
        if self.has(scope) {
            return Ok(());
        }
        crate::metrics::refusal(ErrorCode::ScopeMissing.as_str());
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

/// The method as a metric's label and a span's field: one of the methods
/// HTTP defines, `other` for the rest. A client chooses the method, so a
/// label that took it as it came would grow by whatever it sent (F131).
pub(crate) fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::DELETE => "DELETE",
        Method::PATCH => "PATCH",
        Method::OPTIONS => "OPTIONS",
        Method::CONNECT => "CONNECT",
        Method::TRACE => "TRACE",
        _ => "other",
    }
}

/// The bytes of a request's headers as they came: every name and value,
/// with its colon, space and line end.
fn header_bytes(headers: &HeaderMap) -> usize {
    headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.len() + 4)
        .sum()
}

/// What a request is refused with before anything else looks at it: a head
/// over the limits of spec §19, a target too long, a body that says it is
/// too large (on every endpoint, whether it reads a body or not).
fn refused_at_the_door(request: &Request, server: &ServerConfig) -> Option<ApiError> {
    let limit = |value: u32| usize::try_from(value).unwrap_or(usize::MAX);
    let headers = request.headers();
    if headers.len() > limit(server.max_headers) {
        return Some(ApiError::new(
            ErrorCode::HeadersTooLarge,
            format!("the request has more than {} headers", server.max_headers),
        ));
    }
    if header_bytes(headers) > limit(server.max_header_bytes) {
        return Some(ApiError::new(
            ErrorCode::HeadersTooLarge,
            format!(
                "the request's headers are longer than {} bytes",
                server.max_header_bytes
            ),
        ));
    }
    let target = request
        .uri()
        .path_and_query()
        .map_or(0, |target| target.as_str().len());
    if target > limit(server.max_uri_bytes) {
        return Some(ApiError::new(
            ErrorCode::UriTooLong,
            format!(
                "the request target is longer than {} bytes",
                server.max_uri_bytes
            ),
        ));
    }
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    if declared.is_some_and(|length| length > server.max_body_bytes) {
        return Some(ApiError::new(
            ErrorCode::PayloadTooLarge,
            "the body of the request is larger than server.max_body_bytes",
        ));
    }
    None
}

/// The code of an answer that a layer below the API made in plain text:
/// axum's refusal of a path it cannot read (`%ff`), of a body, of a
/// method. `None` for a status no code of the API has.
const fn code_for(status: StatusCode) -> Option<ErrorCode> {
    Some(match status {
        StatusCode::BAD_REQUEST => ErrorCode::InvalidRequest,
        StatusCode::NOT_FOUND => ErrorCode::NotFound,
        StatusCode::METHOD_NOT_ALLOWED => ErrorCode::MethodNotAllowed,
        StatusCode::REQUEST_TIMEOUT => ErrorCode::RequestTimeout,
        StatusCode::PAYLOAD_TOO_LARGE => ErrorCode::PayloadTooLarge,
        StatusCode::URI_TOO_LONG => ErrorCode::UriTooLong,
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ErrorCode::UnsupportedMediaType,
        StatusCode::UNPROCESSABLE_ENTITY => ErrorCode::ValidationFailed,
        StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE => ErrorCode::HeadersTooLarge,
        StatusCode::SERVICE_UNAVAILABLE => ErrorCode::Unavailable,
        StatusCode::INTERNAL_SERVER_ERROR => ErrorCode::Internal,
        _ => return None,
    })
}

/// Most bytes of a plain-text refusal that are read to say what it was.
const PLAIN_REFUSAL_BYTES: usize = 4096;

/// An error answer that is no problem document, as one: every error of the
/// API is a problem with a stable code (spec §16.1), whatever made it.
async fn as_problem(response: Response) -> Response {
    let status = response.status();
    let Some(code) = code_for(status) else {
        return response;
    };
    let plain = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|value| value.starts_with("text/plain"));
    if !plain || response.extensions().get::<Carried>().is_some() {
        return response;
    }
    let text = axum::body::to_bytes(response.into_body(), PLAIN_REFUSAL_BYTES)
        .await
        .ok()
        .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| code.title().to_owned());
    if code == ErrorCode::Internal {
        return ApiError::internal(&text).into_response();
    }
    ApiError::new(code, text).into_response()
}

/// The outermost layer; see the module documentation.
pub(crate) async fn observe(
    Extension(state): Extension<ApiState>,
    mut request: Request,
    next: Next,
) -> Response {
    let id = RequestId::new();
    let started = Instant::now();
    let method = method_label(request.method());
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

    crate::metrics::in_flight(1.0);
    let refused = refused_at_the_door(&request, &state.config().server);
    let mut response = if let Some(error) = refused {
        crate::metrics::refusal(error.code().as_str());
        error.into_response()
    } else {
        let response = next.run(request).instrument(span.clone()).await;
        as_problem(response).await
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
        method,
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
    // What a stopping server waits for before it closes its listener.
    let counted = state.enter();
    let response = next.run(request).await;
    drop(counted);
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
    let written = record(
        &state,
        &caller,
        action_of(&method, &route),
        note.target.or_else(|| target_of(&route, &path)),
        outcome,
        json!({
            "request_id": caller.request,
            "path": path,
            "status": status.as_u16(),
            "code": code,
            "result": note.details,
        }),
    )
    .await;
    match written {
        Ok(()) => response,
        // An admin action that leaves no trace is not answered as done.
        Err(error) => error.into_response(),
    }
}

/// Writes one event of the audit trail for the caller's key.
///
/// # Errors
///
/// `internal` when the event cannot be written: an action that leaves no
/// trace is not answered as done.
pub(crate) async fn record(
    state: &ApiState,
    caller: &Caller,
    action: String,
    target: Option<String>,
    outcome: AuditOutcome,
    details: Value,
) -> ApiResult<()> {
    let event = AuditEvent {
        id: AuditEventId::new(),
        at: state.clock().now(),
        actor: AuditActor::ApiKey(caller.key.id),
        action,
        target,
        outcome,
        details,
    };
    state
        .store()
        .audit_events()
        .insert(&event)
        .await
        .map_err(|error| {
            tracing::error!(%error, action = event.action, "an audit event could not be written");
            ApiError::internal(&error)
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use axum::http::Method;
    use chrono::Utc;
    use kage_core::domain::{ApiKey, KeyLimits, Scope};
    use kage_core::ids::{ApiKeyId, RequestId};

    use super::{AuditNote, Caller, action_of, method_label, target_of};
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
    fn the_method_label_is_one_http_defines_or_other() {
        // F131 (secrecy-04): a client chooses the method; the label must
        // not grow with what it sends.
        for (method, label) in [
            (Method::GET, "GET"),
            (Method::POST, "POST"),
            (Method::DELETE, "DELETE"),
            (Method::OPTIONS, "OPTIONS"),
            (Method::from_bytes(b"QAS04X1").unwrap(), "other"),
            (Method::from_bytes(&[b'L'; 4000]).unwrap(), "other"),
        ] {
            assert_eq!(method_label(&method), label);
        }
    }

    #[test]
    fn a_note_names_the_target_and_what_came_of_it() {
        let note = AuditNote::about("0199-persona").with(serde_json::json!({ "passed": true }));

        assert_eq!(note.target.as_deref(), Some("0199-persona"));
        assert_eq!(note.details["passed"], true);
        assert!(AuditNote::default().target.is_none());
    }
}
