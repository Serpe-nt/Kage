//! axum router, handlers, OpenAPI document, auth and webhooks.
//!
//! Scaffold only. The liveness endpoint is the single route; the public API
//! lands in milestone M8 (see `docs/02-plan.md`). The route is already
//! registered through `utoipa-axum` so that the router and the OpenAPI
//! document are built from the same source from the first commit.

#![forbid(unsafe_code)]

use axum::{Json, Router};
use serde::Serialize;
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

/// Name of this crate as recorded in its manifest.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Path of the liveness endpoint (spec §16.1).
pub const HEALTHZ_PATH: &str = "/healthz";

/// Whether the process can answer requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LivenessStatus {
    /// The process is up and answering requests.
    Ok,
}

/// Body of a liveness response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub struct Liveness {
    /// Current liveness status.
    pub status: LivenessStatus,
}

#[derive(OpenApi)]
#[openapi(info(
    title = "Kage API",
    description = "Self-hosted page fetching API. See docs/01-spec.md §16."
))]
struct ApiDoc;

/// Liveness probe: answers as long as the process is running. It checks no
/// dependency; readiness is a separate endpoint (spec §16.1).
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "ops",
    responses((status = OK, description = "The process is alive.", body = Liveness))
)]
async fn healthz() -> Json<Liveness> {
    Json(Liveness {
        status: LivenessStatus::Ok,
    })
}

/// Single registration point: every route added here is served and documented.
fn api() -> OpenApiRouter {
    OpenApiRouter::with_openapi(ApiDoc::openapi()).routes(routes!(healthz))
}

/// Builds the HTTP router with every public route.
pub fn router() -> Router {
    let (router, _openapi) = api().split_for_parts();
    router
}

/// Builds the OpenAPI document that describes [`router`].
pub fn openapi() -> utoipa::openapi::OpenApi {
    let (_router, openapi) = api().split_for_parts();
    openapi
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::{CRATE_NAME, HEALTHZ_PATH, openapi, router};

    #[test]
    fn smoke() {
        assert_eq!(CRATE_NAME, "kage-api");
    }

    #[tokio::test]
    async fn healthz_reports_ok() {
        let request = Request::get(HEALTHZ_PATH).body(Body::empty()).unwrap();
        let response = router().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!({ "status": "ok" }));
    }

    #[tokio::test]
    async fn unknown_path_is_not_found() {
        let request = Request::get("/nope").body(Body::empty()).unwrap();
        let response = router().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn openapi_documents_every_served_route() {
        let document = serde_json::to_value(openapi()).unwrap();

        assert_eq!(document["info"]["title"], "Kage API");
        assert!(
            document["openapi"].as_str().unwrap().starts_with("3.1"),
            "spec §16.1 requires OpenAPI 3.1, got {}",
            document["openapi"]
        );
        assert!(document["paths"][HEALTHZ_PATH]["get"].is_object());
    }
}
