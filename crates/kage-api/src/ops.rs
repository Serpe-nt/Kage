//! What an operator's tools ask: liveness, readiness, metrics and the
//! OpenAPI document (spec §16.1, §20).
//!
//! `/healthz`, `/readyz` and `/openapi.json` take no key: a container's
//! health check and a load balancer have none, and none of the three says
//! anything about the pool. `/metrics` on the API takes a key with the
//! `admin` scope; the listener of `server.metrics_bind`, when one is
//! configured, serves it without a key to a Prometheus on the same private
//! network.

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{CONTENT_TYPE, HeaderValue};
use axum::response::{IntoResponse, Response};
use kage_core::domain::{Engine, PersonaState, Render, Scope};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult, Problem};
use crate::metrics::{PROMETHEUS_TEXT, gauge_family, merge};
use crate::state::ApiState;

/// How long an answer about readiness is reused: a probe that asks every
/// second does not ask the database and the sidecars every second.
const READINESS_TTL: Duration = Duration::from_secs(2);

/// How long a sidecar may take to answer a readiness or metrics question.
const SIDECAR_PATIENCE: Duration = Duration::from_secs(3);

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

/// Liveness probe: answers as long as the process is running. It checks no
/// dependency; readiness is a separate endpoint (spec §16.1).
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "ops",
    operation_id = "healthz",
    responses((status = OK, description = "The process is alive.", body = Liveness))
)]
pub(crate) async fn healthz() -> Json<Liveness> {
    Json(Liveness {
        status: LivenessStatus::Ok,
    })
}

/// What readiness was found to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct Checks {
    /// The database answers.
    pub(crate) database: bool,
    /// At least one persona is ready (active, warmed up) behind a proxy
    /// that can carry traffic.
    pub(crate) persona: bool,
    /// At least one browser sidecar answers.
    pub(crate) sidecar: bool,
}

/// Whether Kage can fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReadyStatus {
    /// Every check passes.
    Ready,
    /// A check fails; `checks` says which.
    NotReady,
    /// The server is stopping and takes no new work.
    Stopping,
}

/// Body of a readiness response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct ReadyReport {
    /// Whether Kage can fetch.
    pub(crate) status: ReadyStatus,
    /// What was checked.
    pub(crate) checks: Checks,
}

/// An answer about readiness and when it was found.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Readiness {
    checks: Checks,
    at: Instant,
}

async fn check(state: &ApiState) -> Checks {
    let database = sqlx_ping(state).await;
    let Some(engine) = state.engine() else {
        return Checks {
            database,
            persona: false,
            sidecar: false,
        };
    };
    let persona = match (
        state.store().personas().list_live().await,
        engine.pool.live().await,
    ) {
        (Ok(personas), Ok(proxies)) => personas.iter().any(|persona| {
            persona.is_ready()
                && proxies
                    .iter()
                    .any(|proxy| proxy.id == persona.proxy_id && proxy.state.is_usable())
        }),
        _ => false,
    };
    let sidecars = engine.personas.sidecars();
    let (chromium, firefox) = tokio::join!(
        tokio::time::timeout(SIDECAR_PATIENCE, sidecars.of(Engine::Chromium).health()),
        tokio::time::timeout(SIDECAR_PATIENCE, sidecars.of(Engine::Firefox).health()),
    );
    let sidecar = matches!(chromium, Ok(Ok(_))) || matches!(firefox, Ok(Ok(_)));
    Checks {
        database,
        persona,
        sidecar,
    }
}

/// Whether the database answers: the cheapest query that needs it.
async fn sqlx_ping(state: &ApiState) -> bool {
    state.store().migration_status().await.is_ok()
}

/// Readiness probe: the database answers, at least one persona is ready
/// behind a usable proxy, and at least one sidecar answers (spec §16.1).
/// `503` otherwise, and from the moment the server is told to stop.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "ops",
    operation_id = "readyz",
    responses(
        (status = 200, description = "Kage can fetch.", body = ReadyReport),
        (status = 503, description = "Kage cannot fetch now; `checks` says why.", body = ReadyReport),
    )
)]
pub(crate) async fn readyz(State(state): State<ApiState>) -> Response {
    let known = {
        let cached = state.0.readiness.lock().await;
        cached.filter(|readiness| readiness.at.elapsed() < READINESS_TTL)
    };
    let checks = if let Some(readiness) = known {
        readiness.checks
    } else {
        let checks = check(&state).await;
        *state.0.readiness.lock().await = Some(Readiness {
            checks,
            at: Instant::now(),
        });
        checks
    };
    let ready = checks.database && checks.persona && checks.sidecar;
    let (code, status) = if state.is_stopping() {
        (StatusCode::SERVICE_UNAVAILABLE, ReadyStatus::Stopping)
    } else if ready {
        (StatusCode::OK, ReadyStatus::Ready)
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, ReadyStatus::NotReady)
    };
    (code, Json(ReadyReport { status, checks })).into_response()
}

/// The gauges that are read from the store when a scrape arrives.
async fn stored_gauges(state: &ApiState) -> String {
    let Ok(personas) = state.store().personas().list(None).await else {
        return String::new();
    };
    let states = [
        PersonaState::Active,
        PersonaState::Cooling,
        PersonaState::Retired,
    ];
    let counts: Vec<(Vec<(&str, &str)>, f64)> = states
        .iter()
        .map(|state| {
            let count = personas
                .iter()
                .filter(|persona| persona.state == *state)
                .count();
            // A pool is tens of personas: the count is exact as a float.
            let count = u32::try_from(count).map_or(f64::MAX, f64::from);
            (vec![("state", state.as_str())], count)
        })
        .collect();
    let mut text = gauge_family("kage_persona_state", "Personas by state", &counts);

    let mut scores = Vec::new();
    for persona in personas.iter().filter(|persona| persona.is_live()) {
        if let Ok(Some(run)) = state
            .store()
            .selftest_runs()
            .latest(persona.id, Render::Browser)
            .await
            && let Some(score) = run.score
        {
            scores.push((
                persona.id.to_string(),
                persona.engine.as_str(),
                f64::from(score),
            ));
        }
    }
    let samples: Vec<(Vec<(&str, &str)>, f64)> = scores
        .iter()
        .map(|(persona, engine, score)| {
            (
                vec![("persona", persona.as_str()), ("engine", *engine)],
                *score,
            )
        })
        .collect();
    text.push_str(&gauge_family(
        "kage_selftest_score",
        "Score of the latest browser self-test of each live persona",
        &samples,
    ));
    text.push_str(&job_gauges(state).await);
    text
}

/// The queues of the job worker and of the webhook sender, as the store
/// has them at this moment (spec §20).
async fn job_gauges(state: &ApiState) -> String {
    // A count of rows is exact as a float far beyond what a store holds.
    #[allow(clippy::cast_precision_loss)]
    let float = |count: u64| count as f64;
    let mut text = String::new();
    if let Ok(counts) = state.store().jobs().counts().await {
        let samples: Vec<(Vec<(&str, &str)>, f64)> = counts
            .iter()
            .map(|count| {
                (
                    vec![
                        ("job_type", count.kind.as_str()),
                        ("state", count.state.as_str()),
                    ],
                    float(count.jobs),
                )
            })
            .collect();
        text.push_str(&gauge_family(
            "kage_jobs",
            "Jobs by type and state",
            &samples,
        ));
    }
    if let Ok((queued, running)) = state.store().job_items().backlog().await {
        text.push_str(&gauge_family(
            "kage_job_items",
            "Items of jobs that wait their turn, and items that are with the scheduler",
            &[
                (vec![("state", "queued")], float(queued)),
                (vec![("state", "running")], float(running)),
            ],
        ));
    }
    if let Ok((pending, _)) = state.store().webhooks().pending().await {
        text.push_str(&gauge_family(
            "kage_webhook_deliveries_pending",
            "Events of jobs that wait to be delivered to their webhook",
            &[(Vec::new(), float(pending))],
        ));
    }
    text
}

/// Everything `/metrics` serves, as one exposition; see [`crate::metrics`].
pub(crate) async fn render_metrics(state: &ApiState) -> String {
    let mut expositions = Vec::new();
    if let Some(handle) = state.metrics() {
        expositions.push(handle.render());
    }
    expositions.push(stored_gauges(state).await);
    if let Some(engine) = state.engine() {
        let sidecars = engine.personas.sidecars();
        let (chromium, firefox) = tokio::join!(
            tokio::time::timeout(SIDECAR_PATIENCE, sidecars.of(Engine::Chromium).metrics()),
            tokio::time::timeout(SIDECAR_PATIENCE, sidecars.of(Engine::Firefox).metrics()),
        );
        // A sidecar that does not answer is missing from the scrape, which
        // is what its absence should look like.
        for answer in [chromium, firefox] {
            if let Ok(Ok(text)) = answer {
                expositions.push(text);
            }
        }
    }
    merge(&expositions)
}

fn prometheus(text: String) -> Response {
    let mut response = text.into_response();
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(PROMETHEUS_TEXT));
    response
}

/// Prometheus metrics (spec §20): Kage's own series, the personas' states
/// and self-test scores, and the sidecars' series merged in.
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "ops",
    operation_id = "metrics",
    responses(
        (status = 200, description = "The Prometheus text format.", body = String, content_type = "text/plain"),
        (status = 401, description = "No key, or not a valid one.", body = Problem, content_type = "application/problem+json"),
        (status = 403, description = "The key lacks the `admin` scope.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn metrics(State(state): State<ApiState>, caller: Caller) -> ApiResult<Response> {
    caller.require(Scope::Admin)?;
    Ok(prometheus(render_metrics(&state).await))
}

/// `/metrics` of the listener of `server.metrics_bind`: no key, private
/// network only.
pub(crate) async fn metrics_unauthenticated(State(state): State<ApiState>) -> Response {
    prometheus(render_metrics(&state).await)
}

/// What nothing is served under is answered with.
pub(crate) async fn not_found() -> ApiError {
    ApiError::new(
        crate::error::ErrorCode::NotFound,
        "nothing is served under this path; /openapi.json lists what is",
    )
}

/// What a path is answered with for a method it does not take.
pub(crate) async fn method_not_allowed() -> ApiError {
    ApiError::new(
        crate::error::ErrorCode::MethodNotAllowed,
        "this path does not take this method; /openapi.json lists what it takes",
    )
}

#[cfg(test)]
mod tests {
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;

    use super::{Checks, ReadyReport, ReadyStatus, healthz, method_not_allowed, not_found};
    use crate::error::ErrorCode;

    #[tokio::test]
    async fn liveness_says_ok_and_nothing_else() {
        let body = healthz().await.into_response().into_body();
        let bytes = body.collect().await.unwrap().to_bytes();

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::json!({ "status": "ok" })
        );
    }

    #[test]
    fn a_readiness_report_names_every_check() {
        let report = ReadyReport {
            status: ReadyStatus::NotReady,
            checks: Checks {
                database: true,
                persona: false,
                sidecar: true,
            },
        };

        assert_eq!(
            serde_json::to_value(report).unwrap(),
            serde_json::json!({
                "status": "not_ready",
                "checks": { "database": true, "persona": false, "sidecar": true },
            })
        );
    }

    #[tokio::test]
    async fn paths_and_methods_nobody_serves_are_problems() {
        assert_eq!(not_found().await.code(), ErrorCode::NotFound);
        assert_eq!(
            method_not_allowed().await.code(),
            ErrorCode::MethodNotAllowed
        );
    }
}
