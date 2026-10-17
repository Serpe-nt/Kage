//! The public API (spec §16): the axum router, its handlers, the OpenAPI
//! document, API keys and their limits, and the admin endpoints.
//!
//! - [`router`] builds the router and [`openapi`] the document that
//!   describes it; both come from one registration, so a route that is
//!   served is documented and the other way round.
//! - [`serve`] is the listener: the limits on a request's head and the
//!   deadline for reading it, connections counted, a graceful stop.
//! - [`keys`] mints and checks API keys; [`handle`] makes the handles a
//!   key that is not an admin's sees in place of persona ids; [`resolve`]
//!   is the opt-in host-side lookup of the SSRF guard; [`metrics`] is the
//!   Prometheus exporter behind `/metrics`; [`error`] holds the stable
//!   codes of the problem documents.
//!
//! # What is served
//!
//! | Path | Scope | |
//! |---|---|---|
//! | `GET /healthz`, `GET /readyz`, `GET /openapi.json` | none | liveness, readiness, the document |
//! | `POST /v1/fetch` | `fetch` (and `sessions` with `session_id`) | one page |
//! | `GET /v1/results/{id}`, `GET /v1/artifacts/{id}` | `fetch` | a stored result, what was kept of a page |
//! | `GET /v1/policies/domains/{domain}` | `fetch` | the policy in force for a host |
//! | `POST /v1/sessions`, `GET`/`DELETE /v1/sessions/{id}` | `sessions` | sessions |
//! | `POST /v1/jobs`, `GET /v1/jobs/{id}`, `GET /v1/jobs/{id}/items`, `POST /v1/jobs/{id}/cancel` | `jobs` | batches and flows worked in the background |
//! | `/v1/admin/...` | `admin` | personas, proxies, the pool, policies, keys, the audit trail |
//! | `GET /metrics`, `GET /docs` | `admin` | Prometheus metrics, Swagger UI |
//!
//! [`run_worker`] works the jobs, and a [`WebhookSender`] delivers their
//! events; `kage serve` runs both beside the listener. No CORS header is
//! sent unless `server.cors_allowed_origins` names an origin.

#![forbid(unsafe_code)]

mod admin;
mod auth;
pub mod error;
mod extract;
mod fetch;
pub mod handle;
pub mod jobs;
pub mod keys;
mod limits;
pub mod metrics;
mod ops;
mod policies;
pub mod resolve;
mod results;
mod schema;
pub mod serve;
mod sessions;
mod state;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, HeaderValue};
use axum::http::{HeaderName, Method};
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json, Router};
use kage_core::domain::Scope;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use utoipa_swagger_ui::SwaggerUi;

pub use auth::REQUEST_ID_HEADER;
pub use error::{ApiError, ErrorCode, Problem};
pub use jobs::webhook::{DirectTransport, Transport, WebhookSender};
pub use jobs::worker::run_worker;
pub use ops::{Liveness, LivenessStatus};
pub use state::{ApiParts, ApiState, EngineParts, StateError};

use crate::auth::Caller;

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// A moment as the store keeps it: to the microsecond. What the API says
/// of a thing it just made is then what it says after reading it back.
pub(crate) fn stored(at: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    use chrono::SubsecRound as _;
    at.trunc_subsecs(6)
}

/// Path of the liveness endpoint (spec §16.1).
pub const HEALTHZ_PATH: &str = "/healthz";

/// Path of the readiness endpoint.
pub const READYZ_PATH: &str = "/readyz";

/// Path of the OpenAPI document.
pub const OPENAPI_PATH: &str = "/openapi.json";

/// Path of Swagger UI.
pub const DOCS_PATH: &str = "/docs";

/// Path of the metrics.
pub const METRICS_PATH: &str = "/metrics";

/// The name of the security scheme in the OpenAPI document.
const SECURITY_SCHEME: &str = "api_key";

/// Adds what the route registrations do not carry: how a request
/// authenticates.
struct Security;

impl Modify for Security {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            SECURITY_SCHEME,
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("kage_<keyid>_<secret>")
                    .description(Some(
                        "An API key, made with `kage key create` or `POST /v1/admin/keys` and \
                         shown once. The scopes an operation lists are the ones the key needs; \
                         scopes are independent, `admin` implies no other.",
                    ))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Kage API",
        description = "Self-hosted page fetching API. Every request but `/healthz`, `/readyz` \
                       and `/openapi.json` carries an API key as `Authorization: Bearer \
                       kage_<keyid>_<secret>`. Errors are RFC 9457 problem documents \
                       (`application/problem+json`) with a stable `code`; every answer carries \
                       `X-Request-Id`. A fetch that was made answers `200` whatever the site \
                       did: a block is the result's `outcome`, not an HTTP error. See \
                       docs/01-spec.md §16 and docs/runbooks/api.md.",
        license(name = "MIT OR Apache-2.0")
    ),
    tags(
        (name = "fetch", description = "Fetch a page, read its result and what was kept of it."),
        (name = "sessions", description = "Pin requests to one persona."),
        (name = "jobs", description = "Batches and flows worked in the background, and the webhooks that report them."),
        (name = "admin", description = "Personas, proxies, the pool, policies, keys and the audit trail. Every call is audited."),
        (name = "ops", description = "Liveness, readiness, metrics."),
    ),
    components(schemas(Problem, ErrorCode)),
    modifiers(&Security)
)]
struct ApiDoc;

/// Asks for the `admin` scope: in front of Swagger UI.
async fn admins_only(caller: Caller, request: Request, next: Next) -> Response {
    scoped(Scope::Admin, &caller, request, next).await
}

/// Asks for the `fetch` scope before the request's body is read.
async fn fetchers_only(caller: Caller, request: Request, next: Next) -> Response {
    scoped(Scope::Fetch, &caller, request, next).await
}

/// Asks for the `jobs` scope before the request's body is read.
async fn job_holders_only(caller: Caller, request: Request, next: Next) -> Response {
    scoped(Scope::Jobs, &caller, request, next).await
}

/// Asks for the `sessions` scope before the request's body is read.
async fn session_holders_only(caller: Caller, request: Request, next: Next) -> Response {
    scoped(Scope::Sessions, &caller, request, next).await
}

/// Lets a request through when its key has the scope. The scope is asked
/// for in front of the handler, so before the body is parsed: a key
/// without it learns nothing of what the endpoint would have made of its
/// request.
async fn scoped(scope: Scope, caller: &Caller, request: Request, next: Next) -> Response {
    match caller.require(scope) {
        Ok(()) => next.run(request).await,
        Err(error) => error.into_response(),
    }
}

/// Ends a request that is not a fetch after `server.request_timeout`.
async fn deadline(Extension(state): Extension<ApiState>, request: Request, next: Next) -> Response {
    let limit = state.config().server.request_timeout.get();
    match tokio::time::timeout(limit, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::new(
            ErrorCode::RequestTimeout,
            format!("the request was not answered within {limit:?}"),
        )
        .into_response(),
    }
}

/// What a handler that panicked is answered with: a problem, and the
/// connection lives on.
fn panicked(_: Box<dyn std::any::Any + Send + 'static>) -> Response {
    ApiError::internal(&"a handler panicked").into_response()
}

/// The routes that need a key, with their documentation.
fn keyed() -> OpenApiRouter<ApiState> {
    // Every admin call is audited, whatever comes of it.
    let admin_quick = OpenApiRouter::new()
        .routes(routes!(admin::list_personas))
        .routes(routes!(admin::show_persona))
        .routes(routes!(admin::retire_persona))
        .routes(routes!(admin::regenerate_persona))
        .routes(routes!(admin::list_proxies))
        .routes(routes!(admin::replace_proxy))
        .routes(routes!(admin::pool))
        .routes(routes!(admin::reload_policies))
        .routes(routes!(admin::replace_site_scout))
        .routes(routes!(admin::list_keys, admin::create_key))
        .routes(routes!(admin::revoke_key))
        .routes(routes!(admin::rotate_key))
        .routes(routes!(admin::purge))
        .routes(routes!(admin::list_audit))
        .layer(from_fn(deadline));
    // A probe and a self-test take as long as the network takes.
    let admin_slow = OpenApiRouter::new()
        .routes(routes!(admin::selftest_persona))
        .routes(routes!(admin::check_proxy));
    let admin = admin_quick.merge(admin_slow).layer(from_fn(auth::audit));

    let reading = OpenApiRouter::new()
        .routes(routes!(results::result))
        .routes(routes!(results::artifact))
        .routes(routes!(policies::preview))
        .layer(from_fn(fetchers_only));
    let sessions = OpenApiRouter::new()
        .routes(routes!(sessions::open))
        .routes(routes!(sessions::show, sessions::end))
        .layer(from_fn(session_holders_only));
    let metrics = OpenApiRouter::new()
        .routes(routes!(ops::metrics))
        .layer(from_fn(admins_only));
    // Accepting a job checks every item of it, which is quick; working it
    // is the worker's.
    let jobs = OpenApiRouter::new()
        .routes(routes!(jobs::create))
        .routes(routes!(jobs::show))
        .routes(routes!(jobs::items))
        .routes(routes!(jobs::cancel))
        .layer(from_fn(job_holders_only));
    let quick = reading
        .merge(sessions)
        .merge(jobs)
        .merge(metrics)
        .layer(from_fn(deadline));
    // A fetch has its own limit, `scheduler.sync_wall_time`.
    let fetch = OpenApiRouter::new()
        .routes(routes!(fetch::fetch))
        .layer(from_fn(fetchers_only));

    quick
        .merge(fetch)
        .merge(admin)
        .layer(from_fn(auth::authenticate))
}

/// Single registration point: every route added here is served and
/// documented. It needs no state: the layers find theirs in the request.
fn api() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .routes(routes!(ops::healthz))
        .routes(routes!(ops::readyz))
        .merge(keyed())
}

/// The document as `/openapi.json` serves it.
async fn document(State(document): State<Arc<utoipa::openapi::OpenApi>>) -> Response {
    let mut response = Json(document.as_ref()).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// CORS as `server.cors_allowed_origins` asks for it; `None` when no
/// origin is named, which sends no CORS header at all.
fn cors(origins: &[String]) -> Option<CorsLayer> {
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect();
    if allowed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(allowed))
            .allow_methods([Method::GET, Method::POST, Method::DELETE])
            .allow_headers([AUTHORIZATION, CONTENT_TYPE])
            .expose_headers([
                REQUEST_ID_HEADER,
                HeaderName::from_static("retry-after"),
                HeaderName::from_static("location"),
            ]),
    )
}

/// Builds the HTTP router with every route of the API.
pub fn router(state: ApiState) -> Router {
    let (documented, openapi) = api().split_for_parts();
    let openapi = Arc::new(openapi);
    let settings = &state.config().server;
    let limit = usize::try_from(settings.max_body_bytes).unwrap_or(usize::MAX);
    let cors = cors(&settings.cors_allowed_origins);
    let docs = settings.docs;

    let mut router = documented
        .route(
            OPENAPI_PATH,
            axum::routing::get(document).with_state(Arc::clone(&openapi)),
        )
        .with_state(state.clone());
    if docs {
        // Swagger UI and the copy of the document it loads, for admins.
        let ui: Router = SwaggerUi::new(DOCS_PATH)
            .url("/docs/openapi.json", openapi.as_ref().clone())
            .into();
        router = router.merge(
            ui.layer(from_fn(admins_only))
                .layer(from_fn(auth::authenticate)),
        );
    }
    let router = router
        .fallback(ops::not_found)
        .method_not_allowed_fallback(ops::method_not_allowed)
        .layer(axum::extract::DefaultBodyLimit::max(limit))
        .layer(CatchPanicLayer::custom(panicked))
        .layer(from_fn(auth::observe))
        // Outermost but for CORS: every layer above finds the state here.
        .layer(Extension(state));
    match cors {
        Some(cors) => router.layer(cors),
        None => router,
    }
}

/// The router of the listener of `server.metrics_bind`: `/metrics` and
/// `/healthz` without a key, for a Prometheus on the same private network.
pub fn metrics_router(state: ApiState) -> Router {
    Router::new()
        .route(
            METRICS_PATH,
            axum::routing::get(ops::metrics_unauthenticated),
        )
        .route(HEALTHZ_PATH, axum::routing::get(ops::healthz))
        .fallback(ops::not_found)
        .with_state(state)
}

/// Builds the OpenAPI document that describes [`router`]. It needs no
/// state: the document is the same whatever is configured.
pub fn openapi() -> utoipa::openapi::OpenApi {
    let (_, openapi) = api().split_for_parts();
    openapi
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::{CRATE_NAME, HEALTHZ_PATH, cors, openapi};
    use crate::error::ErrorCode;

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-api");
    }

    fn document() -> Value {
        serde_json::to_value(openapi()).unwrap()
    }

    #[test]
    fn the_document_is_openapi_3_1_and_describes_every_endpoint_of_the_spec() {
        let document = document();

        assert_eq!(document["info"]["title"], "Kage API");
        assert!(
            document["openapi"].as_str().unwrap().starts_with("3.1"),
            "spec §16.1 requires OpenAPI 3.1, got {}",
            document["openapi"]
        );
        let served: BTreeSet<String> = document["paths"]
            .as_object()
            .unwrap()
            .iter()
            .flat_map(|(path, item)| {
                item.as_object()
                    .unwrap()
                    .keys()
                    .map(move |method| format!("{} {path}", method.to_ascii_uppercase()))
            })
            .collect();
        let expected: BTreeSet<String> = [
            "GET /healthz",
            "GET /readyz",
            "GET /metrics",
            "POST /v1/fetch",
            "GET /v1/results/{id}",
            "GET /v1/artifacts/{id}",
            "GET /v1/policies/domains/{domain}",
            "POST /v1/sessions",
            "GET /v1/sessions/{id}",
            "DELETE /v1/sessions/{id}",
            "POST /v1/jobs",
            "GET /v1/jobs/{id}",
            "GET /v1/jobs/{id}/items",
            "POST /v1/jobs/{id}/cancel",
            "POST /v1/admin/purge",
            "GET /v1/admin/personas",
            "GET /v1/admin/personas/{id}",
            "POST /v1/admin/personas/{id}/retire",
            "POST /v1/admin/personas/{id}/regenerate",
            "POST /v1/admin/personas/{id}/selftest",
            "GET /v1/admin/proxies",
            "POST /v1/admin/proxies/{id}/replace",
            "POST /v1/admin/proxies/{id}/health",
            "GET /v1/admin/pool",
            "POST /v1/admin/policies/reload",
            "POST /v1/admin/sites/{site}/scout",
            "GET /v1/admin/keys",
            "POST /v1/admin/keys",
            "DELETE /v1/admin/keys/{id}",
            "POST /v1/admin/keys/{id}/rotate",
            "GET /v1/admin/audit",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert_eq!(served, expected);
        assert!(document["paths"][HEALTHZ_PATH]["get"].is_object());
    }

    #[test]
    fn every_operation_names_itself_and_the_scope_it_needs() {
        let document = document();
        let mut ids = BTreeSet::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                let id = operation["operationId"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{method} {path} has no operationId"));
                assert!(ids.insert(id.to_owned()), "{id} is used twice");
                let public = matches!(path.as_str(), "/healthz" | "/readyz");
                let scopes: Vec<&str> = operation["security"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|requirement| requirement["api_key"].as_array())
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                if public {
                    assert!(scopes.is_empty(), "{method} {path}");
                } else if path.starts_with("/v1/admin/") || path == "/metrics" {
                    assert_eq!(scopes, ["admin"], "{method} {path}");
                } else if path.starts_with("/v1/sessions") {
                    assert_eq!(scopes, ["sessions"], "{method} {path}");
                } else if path.starts_with("/v1/jobs") {
                    assert_eq!(scopes, ["jobs"], "{method} {path}");
                } else {
                    assert_eq!(scopes, ["fetch"], "{method} {path}");
                }
            }
        }
        // The scheme the operations name exists, and is a bearer token.
        let scheme = &document["components"]["securitySchemes"]["api_key"];
        assert_eq!(scheme["type"], "http");
        assert_eq!(scheme["scheme"], "bearer");
    }

    #[test]
    fn the_document_lists_every_error_code_and_the_fetch_schemas() {
        let document = document();
        let schemas = &document["components"]["schemas"];

        let listed: BTreeSet<&str> = schemas["ErrorCode"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        let codes: BTreeSet<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
        assert_eq!(listed, codes);

        for schema in [
            "FetchRequest",
            "FetchResult",
            "FetchAccepted",
            "Problem",
            "SessionRequest",
            "SessionView",
            "PolicyPreview",
            "KeyRequest",
            "KeyCreated",
            "AuditPage",
            "PoolView",
            "JobRequest",
            "BatchJob",
            "FlowJob",
            "FlowStepField",
            "CrawlJob",
            "WebhookField",
            "JobView",
            "CrawlView",
            "JobItemsPage",
            "CrawlItemView",
            "PurgeRequest",
            "Purged",
        ] {
            assert!(
                schemas[schema].is_object(),
                "{schema} is not in the document"
            );
        }
        // The request has no `locale`: Accept-Language is the persona's.
        let properties = schemas["FetchRequest"]["properties"].as_object().unwrap();
        assert!(!properties.contains_key("locale"));
        for field in [
            "url",
            "render",
            "request_kind",
            "include",
            "behavior",
            "session_id",
        ] {
            assert!(properties.contains_key(field), "{field}");
        }
        assert_eq!(
            schemas["FetchRequest"]["required"],
            serde_json::json!(["url"])
        );
        assert_eq!(
            schemas["RequestKindField"]["enum"],
            serde_json::json!(["navigation", "fetch", "xhr"])
        );
        // A non-admin result shows a persona and never a proxy.
        let result = schemas["FetchResult"]["properties"].as_object().unwrap();
        assert!(result.contains_key("persona") && !result.contains_key("proxy"));
        let fetch = &document["paths"]["/v1/fetch"]["post"];
        assert!(fetch["responses"]["200"].is_object());
        assert!(fetch["responses"]["202"].is_object());
        assert_eq!(
            fetch["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/FetchRequest"
        );
    }

    #[test]
    fn no_cors_header_is_sent_unless_an_origin_is_named() {
        assert!(cors(&[]).is_none());
        assert!(cors(&["https://console.example.org".to_owned()]).is_some());
    }
}
