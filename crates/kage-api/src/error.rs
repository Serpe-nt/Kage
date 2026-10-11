//! Errors as RFC 9457 problem details with a stable `code` (spec §16.1).
//!
//! Every answer that is not a success is a document of the media type
//! `application/problem+json`:
//!
//! ```json
//! {
//!   "type": "urn:kage:problem:header_denied",
//!   "title": "A header cannot be set by a caller",
//!   "status": 422,
//!   "detail": "the header \"User-Agent\" cannot be set by a caller: it is the persona's identity",
//!   "code": "header_denied",
//!   "request_id": "0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e",
//!   "header": "User-Agent"
//! }
//! ```
//!
//! `code` is the contract: clients branch on it, and a code never changes
//! its meaning. `title` belongs to the code, `detail` to the occurrence.
//! Further members say what a client needs to act (`header`, `field`,
//! `retry_after_s`, `constraint`).
//!
//! A problem never carries a proxy's address, a persona's real id or a
//! sidecar's message: what went wrong inside is logged under the request's
//! id, and the caller gets the code.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use axum::Json;
use axum::http::header::{CONTENT_TYPE, HeaderValue, RETRY_AFTER, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use kage_core::ids::RequestId;
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

use crate::schema::FreeForm;

/// The media type of a problem document.
pub const PROBLEM_JSON: &str = "application/problem+json";

macro_rules! error_codes {
    ($( $(#[$meta:meta])* $variant:ident => ($name:literal, $status:ident, $title:literal), )+) => {
        /// The stable codes of the API's errors (spec §16.1). The name is
        /// what a problem document carries as `code`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, ToSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum ErrorCode {
            $( $(#[$meta])* $variant, )+
        }

        impl ErrorCode {
            /// Every code.
            pub const ALL: &'static [Self] = &[ $( Self::$variant ),+ ];

            /// The code as a problem document carries it.
            pub const fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $name, )+ }
            }

            /// The HTTP status the code is answered with.
            pub const fn status(self) -> StatusCode {
                match self { $( Self::$variant => StatusCode::$status, )+ }
            }

            /// What the code means, in a sentence that never changes.
            pub const fn title(self) -> &'static str {
                match self { $( Self::$variant => $title, )+ }
            }
        }
    };
}

error_codes! {
    /// The request carries no `Authorization: Bearer` header.
    MissingKey => ("missing_key", UNAUTHORIZED, "The request carries no API key"),
    /// The key is malformed, unknown, or its secret is wrong.
    InvalidKey => ("invalid_key", UNAUTHORIZED, "The API key is not valid"),
    /// The key is valid and was disabled.
    KeyDisabled => ("key_disabled", UNAUTHORIZED, "The API key is disabled"),
    /// The key lacks the scope the endpoint needs.
    ScopeMissing => ("scope_missing", FORBIDDEN, "The API key lacks a scope this endpoint needs"),
    /// Too many requests per minute, for the key or for the whole API.
    RateLimited => ("rate_limited", TOO_MANY_REQUESTS, "Too many requests"),
    /// The key has as many requests in flight as it may have.
    ConcurrencyLimited => ("concurrency_limited", TOO_MANY_REQUESTS, "Too many requests in flight for this key"),
    /// The key's daily page budget is used up.
    BudgetExhausted => ("budget_exhausted", TOO_MANY_REQUESTS, "The key's daily page budget is used up"),
    /// The body is not the JSON the endpoint takes.
    InvalidRequest => ("invalid_request", BAD_REQUEST, "The request is not what this endpoint takes"),
    /// A field is out of range or contradicts another.
    ValidationFailed => ("validation_failed", UNPROCESSABLE_ENTITY, "A field of the request is not acceptable"),
    /// The URL is not an http or https URL, or is too long.
    InvalidUrl => ("invalid_url", UNPROCESSABLE_ENTITY, "The URL cannot be fetched"),
    /// The SSRF guard refuses the URL.
    SsrfDenied => ("ssrf_denied", UNPROCESSABLE_ENTITY, "The URL points where Kage does not go"),
    /// A header is one a caller may not set.
    HeaderDenied => ("header_denied", UNPROCESSABLE_ENTITY, "A header cannot be set by a caller"),
    /// A header's name or value cannot be sent, or it is given twice.
    HeaderInvalid => ("header_invalid", UNPROCESSABLE_ENTITY, "A header cannot be sent as written"),
    /// A header has no known place in a `fetch()` or `XMLHttpRequest`.
    HeaderNotPlaceable => ("header_not_placeable", UNPROCESSABLE_ENTITY, "No capture of a real browser says where this header goes in such a request"),
    /// `include` asks for something the render mode cannot give.
    RenderIncludeConflict => ("render_include_conflict", UNPROCESSABLE_ENTITY, "The render mode cannot produce what include asks for"),
    /// A `fetch()` or `XMLHttpRequest` was asked for without `render: http`.
    RenderKindConflict => ("render_kind_conflict", UNPROCESSABLE_ENTITY, "Only the http path sends a fetch() or an XMLHttpRequest"),
    /// A method, a body or caller headers were given without `render: http`.
    RenderNeedsHttp => ("render_needs_http", UNPROCESSABLE_ENTITY, "A browser render is a plain navigation: a method, a body and headers need render http"),
    /// A `fetch()` or `XMLHttpRequest` was asked for without its page.
    RequestKindNeedsReferer => ("request_kind_needs_referer", UNPROCESSABLE_ENTITY, "A fetch() or XMLHttpRequest needs the page that makes it as its Referer"),
    /// A browser would preflight the request, and preflights are not emulated.
    PreflightRequired => ("preflight_required", UNPROCESSABLE_ENTITY, "A browser would send a preflight before this request"),
    /// `timeout_ms` leaves a browser no time to behave like a person.
    TimeoutTooShort => ("timeout_too_short", UNPROCESSABLE_ENTITY, "The timeout is shorter than an attempt needs"),
    /// The request asks for something a later milestone brings.
    NotAvailableYet => ("not_available_yet", UNPROCESSABLE_ENTITY, "This part of the API does not exist yet"),
    /// The pacing profile is not defined, or an inline profile is invalid.
    InvalidPacing => ("invalid_pacing", UNPROCESSABLE_ENTITY, "The pacing asked for cannot be used"),
    /// The session id is not 1 to 64 characters of `[A-Za-z0-9_.-]`.
    InvalidSessionId => ("invalid_session_id", UNPROCESSABLE_ENTITY, "The session id is not well formed"),
    /// The key has no open session with this id.
    SessionNotFound => ("session_not_found", NOT_FOUND, "The key has no open session with this id"),
    /// The session's persona is not one the request or the policy allows.
    SessionPersonaConflict => ("session_persona_conflict", CONFLICT, "The session's persona is not one this request may use"),
    /// No ready persona matches what a new session asks for.
    NoMatchingPersona => ("no_matching_persona", CONFLICT, "No ready persona matches"),
    /// Nothing is served under this path.
    NotFound => ("not_found", NOT_FOUND, "Nothing is here"),
    /// The method is not one the path takes.
    MethodNotAllowed => ("method_not_allowed", METHOD_NOT_ALLOWED, "The method is not allowed here"),
    /// No such result, or it is another key's.
    ResultNotFound => ("result_not_found", NOT_FOUND, "No such result"),
    /// No such artifact, or it is another key's, or it has expired.
    ArtifactNotFound => ("artifact_not_found", NOT_FOUND, "No such artifact"),
    /// No such persona.
    PersonaNotFound => ("persona_not_found", NOT_FOUND, "No such persona"),
    /// No such proxy.
    ProxyNotFound => ("proxy_not_found", NOT_FOUND, "No such proxy"),
    /// No such API key.
    KeyNotFound => ("key_not_found", NOT_FOUND, "No such API key"),
    /// The thing is not in a state the action applies to.
    Conflict => ("conflict", CONFLICT, "The action does not apply to the current state"),
    /// The request body is larger than the server takes.
    PayloadTooLarge => ("payload_too_large", PAYLOAD_TOO_LARGE, "The request body is too large"),
    /// The request target is longer than the server takes.
    UriTooLong => ("uri_too_long", URI_TOO_LONG, "The request target is too long"),
    /// The body is not `application/json`.
    UnsupportedMediaType => ("unsupported_media_type", UNSUPPORTED_MEDIA_TYPE, "The body must be application/json"),
    /// The client took too long to send the request, or the server to answer.
    RequestTimeout => ("request_timeout", REQUEST_TIMEOUT, "The request took too long"),
    /// Nothing can be fetched: no proxy pool is configured, or the server stops.
    Unavailable => ("unavailable", SERVICE_UNAVAILABLE, "The service cannot do this now"),
    /// Something failed inside Kage; the log has it under the request id.
    Internal => ("internal", INTERNAL_SERVER_ERROR, "Something failed inside Kage"),
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A problem document (RFC 9457).
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct Problem {
    /// Identifies the kind of problem: `urn:kage:problem:<code>`.
    #[serde(rename = "type")]
    pub kind: String,
    /// What the code means; the same for every occurrence.
    pub title: String,
    /// The HTTP status.
    pub status: u16,
    /// What happened this time.
    pub detail: String,
    /// The stable code clients branch on.
    pub code: ErrorCode,
    /// The id of the request, as in `X-Request-Id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = Uuid)]
    pub request_id: Option<RequestId>,
    /// What a client needs to act on the problem: for example `header`,
    /// `field`, `retry_after_s`, `constraint`.
    #[serde(flatten)]
    #[schema(value_type = FreeForm)]
    pub extensions: BTreeMap<String, Value>,
}

/// An error of the API: a code, what happened, and what the answer adds.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    code: ErrorCode,
    detail: String,
    extensions: BTreeMap<String, Value>,
    retry_after: Option<Duration>,
    request_id: Option<RequestId>,
}

impl ApiError {
    /// An error with this code; `detail` says what happened this time.
    pub fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
            extensions: BTreeMap::new(),
            retry_after: None,
            request_id: None,
        }
    }

    /// A field of the request that is not acceptable.
    pub fn field(field: &str, detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::ValidationFailed, detail).with("field", field)
    }

    /// Something failed inside. The cause is logged, never returned: it
    /// may name an address, a persona or a sidecar.
    pub fn internal(cause: &dyn fmt::Display) -> Self {
        tracing::error!(error = %cause, "an API request failed inside");
        Self::new(
            ErrorCode::Internal,
            "the request failed inside Kage; the log has the cause under the request id",
        )
    }

    /// The same error with one more member in its problem document.
    #[must_use]
    pub fn with(mut self, name: &str, value: impl Into<Value>) -> Self {
        self.extensions.insert(name.to_owned(), value.into());
        self
    }

    /// The same error with a `Retry-After` header, and `retry_after_s` in
    /// the document: whole seconds, rounded up, at least one.
    #[must_use]
    pub fn retry_after(mut self, wait: Duration) -> Self {
        let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
        let seconds = seconds.max(1);
        self.retry_after = Some(Duration::from_secs(seconds));
        self.with("retry_after_s", seconds)
    }

    /// The same error for the request with this id.
    #[must_use]
    pub const fn for_request(mut self, id: RequestId) -> Self {
        self.request_id = Some(id);
        self
    }

    /// The code.
    pub const fn code(&self) -> ErrorCode {
        self.code
    }

    /// What happened.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The problem document of the error.
    pub fn problem(&self) -> Problem {
        Problem {
            kind: format!("urn:kage:problem:{}", self.code),
            title: self.code.title().to_owned(),
            status: self.code.status().as_u16(),
            detail: self.detail.clone(),
            code: self.code,
            request_id: self.request_id,
            extensions: self.extensions.clone(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ApiError {}

/// The error a response was made from, kept in the response's extensions
/// so that the request-id layer can add the id to the document and the
/// audit layer can read the code.
#[derive(Debug, Clone)]
pub(crate) struct Carried(pub(crate) ApiError);

/// The response of an error: the problem document with its headers.
pub(crate) fn respond(error: &ApiError) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    if let Some(wait) = error.retry_after {
        headers.insert(RETRY_AFTER, HeaderValue::from(wait.as_secs()));
    }
    if error.code.status() == StatusCode::UNAUTHORIZED {
        headers.insert(
            WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"kage\""),
        );
    }
    let mut response = (error.code.status(), headers, Json(error.problem())).into_response();
    // `Json` sets its own media type; a problem has another.
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    response.extensions_mut().insert(Carried(error.clone()));
    response
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        respond(&self)
    }
}

/// What a handler returns.
pub(crate) type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::time::Duration;

    use axum::http::StatusCode;
    use axum::http::header::{CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE};
    use axum::response::IntoResponse;
    use kage_core::ids::RequestId;

    use super::{ApiError, ErrorCode, PROBLEM_JSON};

    #[test]
    fn codes_are_unique_snake_case_names_with_an_error_status() {
        let names: BTreeSet<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
        assert_eq!(names.len(), ErrorCode::ALL.len());
        for code in ErrorCode::ALL {
            let name = code.as_str();
            assert!(
                name.bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{name}"
            );
            assert!(
                code.status().is_client_error() || code.status().is_server_error(),
                "{name}"
            );
            assert!(
                !code.title().is_empty() && !code.title().ends_with('.'),
                "{name}"
            );
            // The name is what serde writes.
            assert_eq!(serde_json::to_value(code).unwrap(), serde_json::json!(name));
            assert_eq!(code.to_string(), name);
        }
        // A few that clients are known to branch on never move.
        assert_eq!(ErrorCode::RateLimited.as_str(), "rate_limited");
        assert_eq!(
            ErrorCode::RateLimited.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            ErrorCode::RenderIncludeConflict.as_str(),
            "render_include_conflict"
        );
        assert_eq!(
            ErrorCode::SessionPersonaConflict.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(ErrorCode::SessionNotFound.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn a_problem_document_carries_the_code_and_what_a_client_needs() {
        let id = RequestId::new();
        let error = ApiError::new(ErrorCode::HeaderDenied, "the header cannot be set")
            .with("header", "User-Agent")
            .for_request(id);

        let json = serde_json::to_value(error.problem()).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "type": "urn:kage:problem:header_denied",
                "title": "A header cannot be set by a caller",
                "status": 422,
                "detail": "the header cannot be set",
                "code": "header_denied",
                "request_id": id.to_string(),
                "header": "User-Agent",
            })
        );
        assert_eq!(error.code(), ErrorCode::HeaderDenied);
        assert_eq!(error.detail(), "the header cannot be set");
        assert_eq!(error.to_string(), "header_denied: the header cannot be set");

        let field = ApiError::field("timeout_ms", "must be at most 300000");
        assert_eq!(field.code(), ErrorCode::ValidationFailed);
        assert_eq!(field.problem().extensions["field"], "timeout_ms");
        // Without a request id the member is left out.
        assert!(
            serde_json::to_value(field.problem())
                .unwrap()
                .get("request_id")
                .is_none()
        );
    }

    #[test]
    fn the_response_is_a_problem_with_its_headers() {
        let limited = ApiError::new(ErrorCode::RateLimited, "slow down")
            .retry_after(Duration::from_millis(1500))
            .into_response();
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(limited.headers()[CONTENT_TYPE], PROBLEM_JSON);
        // Whole seconds, rounded up.
        assert_eq!(limited.headers()[RETRY_AFTER], "2");
        assert!(limited.headers().get(WWW_AUTHENTICATE).is_none());

        let soon = ApiError::new(ErrorCode::RateLimited, "slow down").retry_after(Duration::ZERO);
        assert_eq!(soon.problem().extensions["retry_after_s"], 1);

        let unauthorized = ApiError::new(ErrorCode::MissingKey, "no key").into_response();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized.headers()[WWW_AUTHENTICATE],
            "Bearer realm=\"kage\""
        );
        assert!(unauthorized.headers().get(RETRY_AFTER).is_none());
    }

    #[test]
    fn what_failed_inside_is_never_told() {
        let error = ApiError::internal(&"connect to 10.1.2.3:5432 refused for persona 0199");

        assert_eq!(error.code(), ErrorCode::Internal);
        let text = serde_json::to_string(&error.problem()).unwrap();
        assert!(
            !text.contains("10.1.2.3") && !text.contains("0199"),
            "{text}"
        );
    }
}
