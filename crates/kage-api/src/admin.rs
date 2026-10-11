//! The admin endpoints (spec §16.3): personas, proxies, the pool, the
//! policies, API keys and the audit trail.
//!
//! All of them need the `admin` scope, and every call, whatever comes of
//! it, is written to `audit_events` by the layer in front of them
//! ([`crate::auth::audit`]); a handler only adds what it acted on.
//!
//! This is where the pool is visible: personas by their real ids, proxies
//! by their addresses. A proxy's user name and password are never
//! returned, here or anywhere; the token of an API key is returned once,
//! when the key is made or rotated.
//!
//! **Self-tests (follow-up F29).** `POST /v1/admin/personas/{id}/selftest`
//! runs in the server: it asks the scheduler's gate as an operator's
//! request, leaves through the forwarder like every other page, and uses
//! the persona's context when real work holds it open instead of meeting
//! `context_conflict` from another process.

// What `utoipa::path` writes for `params(SomeQuery)` names the type through
// its trait, with the span of the name given here.
#![allow(unused_qualifications)]

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, HeaderValue};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use kage_core::domain::{
    ApiKey, AuditActor, AuditEvent, KeyLimits, Persona, PersonaState, Proxy, ReplaceReason, Scope,
};
use kage_core::ids::{ApiKeyId, AuditEventId, PersonaId, ProxyId};
use kage_persona::selftest::{Verdict, pages};
use kage_persona::{PersonaError, manager::REASON_OPERATOR};
use kage_proxy::{PoolError, ReplacementOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;
use utoipa::{IntoParams, ToSchema};

use crate::auth::AuditNote;
use crate::error::{ApiError, ApiResult, ErrorCode, Problem};
use crate::extract::{Body, Params};
use crate::fetch::request::EngineField;
use crate::fetch::result::ClaimedOsField;
use crate::keys::{self, DEFAULT_LIMITS, KeyError, NewKey};
use crate::schema::{AnyJson, FreeForm};
use crate::state::{ApiState, EngineParts};
use crate::stored;

/// The most audit events one page holds.
const MAX_AUDIT_PAGE: u32 = 200;

/// A response with what its audit event should say about it.
fn noted(response: impl IntoResponse, note: AuditNote) -> Response {
    let mut response = response.into_response();
    response.extensions_mut().insert(note);
    response
}

fn engine(state: &ApiState) -> ApiResult<&EngineParts> {
    state.engine().ok_or_else(|| {
        ApiError::new(
            ErrorCode::Unavailable,
            "no proxy pool is configured: there are no personas and no proxies to act on",
        )
    })
}

/// What a failure of the persona manager is answered with. The cause is
/// logged; what is returned names no address and no sidecar's words.
fn persona_failed(error: &PersonaError) -> ApiError {
    match error {
        PersonaError::NotFound(_) => {
            ApiError::new(ErrorCode::PersonaNotFound, "there is no such persona")
        }
        PersonaError::Retired(_) => {
            ApiError::new(ErrorCode::Conflict, "the persona is retired").with("reason", "retired")
        }
        PersonaError::ProxyNotUsable { .. } | PersonaError::ProxyNotFound(_) => ApiError::new(
            ErrorCode::Conflict,
            "the persona's proxy cannot carry traffic now",
        )
        .with("reason", "proxy_not_usable"),
        PersonaError::EgressUnsafe => ApiError::new(
            ErrorCode::Conflict,
            "nothing may leave: a proxy leaked the host's address",
        )
        .with("reason", "egress_unsafe"),
        PersonaError::Held { held, .. } => ApiError::new(
            ErrorCode::Conflict,
            format!("the persona cannot be had now: {held}"),
        )
        .with("reason", "held"),
        PersonaError::NothingToTest(why) => {
            ApiError::new(ErrorCode::Conflict, why.clone()).with("reason", "nothing_to_test")
        }
        PersonaError::HttpPathOff { .. } => ApiError::new(
            ErrorCode::Conflict,
            "the persona has no http path: its browser's version has no verified profile",
        )
        .with("reason", "http_path_off"),
        PersonaError::Browser(browser) if browser.is_unreachable() => {
            tracing::warn!(error = %browser, "a sidecar did not answer an admin request");
            ApiError::new(
                ErrorCode::Unavailable,
                "the persona's sidecar does not answer",
            )
        }
        other => ApiError::internal(other),
    }
}

fn parse_id<T: std::str::FromStr>(text: &str, code: ErrorCode) -> ApiResult<T> {
    text.parse()
        .map_err(|_| ApiError::new(code, "there is no such thing: the id is not a UUID"))
}

// --- Personas ---------------------------------------------------------------

/// A persona, as an operator sees it.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct PersonaAdminView {
    /// The persona's id.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) id: PersonaId,
    /// The proxy it lives behind.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) proxy_id: ProxyId,
    /// Its engine.
    pub(crate) engine: EngineField,
    /// The operating system it claims.
    pub(crate) claimed_os: ClaimedOsField,
    /// The major version of its browser.
    pub(crate) browser_major: u32,
    /// What its browser sends as `User-Agent`.
    pub(crate) user_agent: String,
    /// The emulation profile of its http path; null for a browser-only
    /// persona.
    pub(crate) http_profile: Option<String>,
    /// The country its address is in.
    pub(crate) country: Option<String>,
    /// Its locale.
    pub(crate) locale: String,
    /// Its languages, most preferred first.
    pub(crate) languages: Vec<String>,
    /// Its time zone.
    pub(crate) timezone: String,
    /// Its screen: `width`, `height`, `dpr`, `color_depth`.
    #[schema(value_type = FreeForm)]
    pub(crate) screen: Value,
    /// `active`, `cooling` or `retired`.
    pub(crate) state: String,
    /// Why it cools: `proxy_backoff`, `selftest_failed` or `geo_changed`.
    pub(crate) cooling_reason: Option<String>,
    /// Whether real work may use it: active and warmed up.
    pub(crate) ready: bool,
    /// Warm-up visits made.
    pub(crate) warmup_visits: u32,
    /// When it was warmed up.
    pub(crate) warmed_at: Option<DateTime<Utc>>,
    /// Whether its browser is open now.
    pub(crate) browser_open: bool,
    /// When it was generated.
    pub(crate) created_at: DateTime<Utc>,
    /// When it was retired.
    pub(crate) retired_at: Option<DateTime<Utc>>,
    /// Why it was retired.
    pub(crate) retire_reason: Option<String>,
    /// The stored fingerprint document; only when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<FreeForm>)]
    pub(crate) fingerprint: Option<Value>,
}

fn persona_view(persona: &Persona, open: &[PersonaId], fingerprint: bool) -> PersonaAdminView {
    PersonaAdminView {
        id: persona.id,
        proxy_id: persona.proxy_id,
        engine: persona.engine.into(),
        claimed_os: persona.claimed_os.into(),
        browser_major: persona.browser_major,
        user_agent: persona.user_agent.clone(),
        http_profile: persona.wreq_profile.clone(),
        country: persona.country.map(|country| country.to_string()),
        locale: persona.locale.clone(),
        languages: persona.languages.clone(),
        timezone: persona.timezone.clone(),
        screen: serde_json::to_value(persona.screen).unwrap_or(Value::Null),
        state: persona.state.as_str().to_owned(),
        cooling_reason: persona
            .cooling_reason
            .map(|reason| reason.as_str().to_owned()),
        ready: persona.is_ready(),
        warmup_visits: persona.warmup_visits,
        warmed_at: persona.warmed_at.map(stored),
        browser_open: open.contains(&persona.id),
        created_at: stored(persona.created_at),
        retired_at: persona.retired_at.map(stored),
        retire_reason: persona.retire_reason.clone(),
        fingerprint: fingerprint.then(|| persona.fingerprint.clone()),
    }
}

/// Which personas to list.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct PersonaQuery {
    /// Only personas in this state: `active`, `cooling` or `retired`.
    state: Option<String>,
    /// Include retired personas; without `state`, only live ones are
    /// listed.
    #[serde(default)]
    all: bool,
}

/// The personas.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct PersonaList {
    /// The personas, oldest first.
    pub(crate) personas: Vec<PersonaAdminView>,
}

/// List the personas.
#[utoipa::path(
    get,
    path = "/v1/admin/personas",
    tag = "admin",
    operation_id = "list_personas",
    params(PersonaQuery),
    responses((status = 200, description = "The personas.", body = PersonaList)),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn list_personas(
    State(state): State<ApiState>,
    Params(query): Params<PersonaQuery>,
) -> ApiResult<Json<PersonaList>> {
    let wanted = query
        .state
        .as_deref()
        .map(|state| {
            state.parse::<PersonaState>().map_err(|_| {
                ApiError::field("state", "active, cooling or retired").with("in", "query")
            })
        })
        .transpose()?;
    let personas = state.store().personas();
    let listed = match wanted {
        Some(wanted) => personas.list(Some(wanted)).await,
        None if query.all => personas.list(None).await,
        None => personas.list_live().await,
    }
    .map_err(|error| ApiError::internal(&error))?;
    let open = state
        .engine()
        .map(|engine| engine.personas.contexts().open())
        .unwrap_or_default();
    Ok(Json(PersonaList {
        personas: listed
            .iter()
            .map(|persona| persona_view(persona, &open, false))
            .collect(),
    }))
}

/// What to show of a persona.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct PersonaShowQuery {
    /// Include the stored fingerprint document.
    #[serde(default)]
    fingerprint: bool,
}

/// One persona.
#[utoipa::path(
    get,
    path = "/v1/admin/personas/{id}",
    tag = "admin",
    operation_id = "get_persona",
    params(("id" = String, Path, format = Uuid, description = "The persona's id."), PersonaShowQuery),
    responses(
        (status = 200, description = "The persona.", body = PersonaAdminView),
        (status = 404, description = "`persona_not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn show_persona(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Params(query): Params<PersonaShowQuery>,
) -> ApiResult<Response> {
    let id: PersonaId = parse_id(&id, ErrorCode::PersonaNotFound)?;
    let persona = state
        .store()
        .personas()
        .get(id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(|| ApiError::new(ErrorCode::PersonaNotFound, "there is no such persona"))?;
    let open = state
        .engine()
        .map(|engine| engine.personas.contexts().open())
        .unwrap_or_default();
    Ok(noted(
        Json(persona_view(&persona, &open, query.fingerprint)),
        AuditNote::about(id).with(json!({ "fingerprint": query.fingerprint })),
    ))
}

/// Retire a persona: its sessions end and its http cookies are deleted at
/// once, its profile is archived and later deleted, and its proxy gets a
/// new persona at the manager's next sweep. Retiring a retired persona
/// changes nothing.
#[utoipa::path(
    post,
    path = "/v1/admin/personas/{id}/retire",
    tag = "admin",
    operation_id = "retire_persona",
    params(("id" = String, Path, format = Uuid, description = "The persona's id.")),
    responses(
        (status = 200, description = "The persona, retired.", body = PersonaAdminView),
        (status = 404, description = "`persona_not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn retire_persona(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: PersonaId = parse_id(&id, ErrorCode::PersonaNotFound)?;
    let engine = engine(&state)?;
    let persona = engine
        .personas
        .retire(id, REASON_OPERATOR)
        .await
        .map_err(|error| persona_failed(&error))?;
    Ok(noted(
        Json(persona_view(&persona, &[], false)),
        AuditNote::about(id).with(json!({ "proxy": persona.proxy_id })),
    ))
}

/// Replace a persona with a new one behind the same proxy. The new
/// persona inherits nothing.
#[utoipa::path(
    post,
    path = "/v1/admin/personas/{id}/regenerate",
    tag = "admin",
    operation_id = "regenerate_persona",
    params(("id" = String, Path, format = Uuid, description = "The persona's id.")),
    responses(
        (status = 200, description = "The new persona.", body = PersonaAdminView),
        (status = 404, description = "`persona_not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: the persona is retired, or its proxy cannot carry traffic.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn regenerate_persona(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: PersonaId = parse_id(&id, ErrorCode::PersonaNotFound)?;
    let engine = engine(&state)?;
    let persona = engine
        .personas
        .regenerate(id)
        .await
        .map_err(|error| persona_failed(&error))?;
    Ok(noted(
        Json(persona_view(&persona, &[], false)),
        AuditNote::about(id).with(json!({ "new_persona": persona.id })),
    ))
}

/// What a self-test is to cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelftestRequest {
    /// Also use the echo and load the detection pages that others run
    /// (`selftest.http.third_party_url`,
    /// `selftest.browser.third_party_pages`). They see the address of the
    /// persona's proxy. Never done on a timer, only when asked for here.
    #[serde(default)]
    pub(crate) third_party: bool,
}

/// One path of a self-test.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct SelftestPart {
    /// `passed`, `failed`, `inconclusive` (the test site could not be
    /// read), or `skipped`.
    pub(crate) verdict: String,
    /// Why the path was not tested, when it was skipped.
    pub(crate) skipped: Option<String>,
    /// The score of a browser self-test, from 0 to 100.
    pub(crate) score: Option<f64>,
    /// The stored run.
    #[schema(value_type = Option<String>, format = Uuid)]
    pub(crate) run: Option<uuid::Uuid>,
    /// What was found: the echo's comparison on the http path, the
    /// sidecar's findings per page on the browser path. Null for a path
    /// that was skipped.
    #[schema(value_type = AnyJson)]
    pub(crate) findings: Value,
}

impl SelftestPart {
    fn skipped(why: impl Into<String>) -> Self {
        Self {
            verdict: "skipped".to_owned(),
            skipped: Some(why.into()),
            score: None,
            run: None,
            findings: Value::Null,
        }
    }
}

/// What a self-test of a persona found.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct SelftestView {
    /// Whether every path that was tested passed, and at least one was.
    pub(crate) passed: bool,
    /// The http path.
    pub(crate) http: SelftestPart,
    /// The browser path.
    pub(crate) browser: SelftestPart,
    /// The persona after the test: a failure cools it, a pass ends a
    /// cooling that waited for one.
    pub(crate) persona: PersonaAdminView,
}

/// A failure that means "this path cannot be tested for this persona" and
/// not "the test could not be run".
fn skip_reason(error: &PersonaError) -> Option<String> {
    match error {
        PersonaError::HttpPathOff { .. } | PersonaError::NothingToTest(_) => {
            Some(persona_failed(error).detail().to_owned())
        }
        PersonaError::Selftest(why) => Some(why.to_string()),
        _ => None,
    }
}

/// Run the self-tests of a persona, in the server.
///
/// The http path is tested when the persona has one and an echo is
/// configured; the browser path in the persona's own context, through the
/// scheduler's gate and the forwarder. A failed test cools the persona
/// until one passes; a passed one ends a cooling that waited for it. This
/// may take a minute or two.
#[utoipa::path(
    post,
    path = "/v1/admin/personas/{id}/selftest",
    tag = "admin",
    operation_id = "selftest_persona",
    params(("id" = String, Path, format = Uuid, description = "The persona's id.")),
    request_body(content = SelftestRequest, description = "Optional; `{}` tests against the operator's own echo and pages."),
    responses(
        (status = 200, description = "What the self-tests found; `passed` says whether they passed.", body = SelftestView),
        (status = 404, description = "`persona_not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: the persona is retired, held by other work, or its proxy cannot carry traffic.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn selftest_persona(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Response> {
    let id: PersonaId = parse_id(&id, ErrorCode::PersonaNotFound)?;
    let engine = engine(&state)?;
    // The body is optional: nothing at all asks for the default.
    let request = if body.iter().all(u8::is_ascii_whitespace) {
        SelftestRequest::default()
    } else {
        serde_json::from_slice::<SelftestRequest>(&body)
            .map_err(|error| ApiError::new(ErrorCode::ValidationFailed, error.to_string()))?
    };
    let third_party = request.third_party;
    let selftest = &state.config().selftest;
    // Unknown and retired personas are told apart from a test that fails.
    engine
        .personas
        .get(id)
        .await
        .map_err(|error| persona_failed(&error))?;

    let echo = if third_party {
        Some(selftest.http.third_party_url.as_str())
    } else {
        selftest.http.echo_url.as_deref()
    };
    let http = match echo.map(Url::parse) {
        None => SelftestPart::skipped(
            "selftest.http.echo_url is not set; ask for third_party to use the echo others run",
        ),
        Some(Err(_)) => SelftestPart::skipped("the configured echo URL does not parse"),
        Some(Ok(echo)) => match engine.personas.selftest_http(id, &echo).await {
            Ok(run) => SelftestPart {
                verdict: run.verdict.as_str().to_owned(),
                skipped: None,
                score: None,
                run: Some(run.run.id.as_uuid()),
                findings: run.run.findings,
            },
            Err(error) => match skip_reason(&error) {
                Some(why) => SelftestPart::skipped(why),
                None => return Err(persona_failed(&error)),
            },
        },
    };
    let browser = match engine
        .personas
        .selftest_browser(id, pages(&selftest.browser, third_party))
        .await
    {
        Ok(run) => SelftestPart {
            verdict: run.verdict.as_str().to_owned(),
            skipped: None,
            score: Some(run.report.score),
            run: Some(run.run.id.as_uuid()),
            findings: run.run.findings,
        },
        Err(error) => match skip_reason(&error) {
            Some(why) => SelftestPart::skipped(why),
            None => return Err(persona_failed(&error)),
        },
    };

    let verdicts: Vec<&str> = [&http, &browser]
        .into_iter()
        .filter(|part| part.skipped.is_none())
        .map(|part| part.verdict.as_str())
        .collect();
    let passed = !verdicts.is_empty()
        && verdicts
            .iter()
            .all(|verdict| *verdict == Verdict::Passed.as_str());
    let after = engine
        .personas
        .get(id)
        .await
        .map_err(|error| persona_failed(&error))?;
    let open = engine.personas.contexts().open();
    let note = AuditNote::about(id).with(json!({
        "third_party": third_party,
        "http": http.verdict,
        "browser": browser.verdict,
        "passed": passed,
    }));
    let view = SelftestView {
        passed,
        http,
        browser,
        persona: persona_view(&after, &open, false),
    };
    Ok(noted(Json(view), note))
}

// --- Proxies and the pool ---------------------------------------------------

/// A proxy, as an operator sees it. Its user name and password are never
/// returned.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct ProxyAdminView {
    /// The proxy's id.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) id: ProxyId,
    /// `webshare` or `static`.
    pub(crate) provider: String,
    /// Its address.
    pub(crate) host: String,
    /// Its port.
    pub(crate) port: u16,
    /// The country the provider states.
    pub(crate) country: Option<String>,
    /// The city the provider states.
    pub(crate) city: Option<String>,
    /// `new`, `healthy`, `degraded`, `backoff`, `replacing` or `retired`.
    pub(crate) state: String,
    /// Failed probes in a row.
    pub(crate) consecutive_failures: u32,
    /// Until when it is left alone.
    pub(crate) backoff_until: Option<DateTime<Utc>>,
    /// Since when it fails without a break.
    pub(crate) failing_since: Option<DateTime<Utc>>,
    /// Why a replacement was requested, when one was.
    pub(crate) replacement_reason: Option<String>,
    /// What the last probe and the last lookups found: reachability,
    /// latency, the exit address, the observed geo record, reputation.
    #[schema(value_type = Option<FreeForm>)]
    pub(crate) last_health: Option<Value>,
    /// When it joined the pool.
    pub(crate) created_at: DateTime<Utc>,
    /// When it left.
    pub(crate) retired_at: Option<DateTime<Utc>>,
}

fn proxy_view(proxy: &Proxy) -> ProxyAdminView {
    ProxyAdminView {
        id: proxy.id,
        provider: proxy.provider.as_str().to_owned(),
        host: proxy.host.clone(),
        port: proxy.port,
        country: proxy.country.map(|country| country.to_string()),
        city: proxy.city.clone(),
        state: proxy.state.as_str().to_owned(),
        consecutive_failures: proxy.consecutive_failures,
        backoff_until: proxy.backoff_until,
        failing_since: proxy.failing_since,
        replacement_reason: proxy
            .replacement
            .as_ref()
            .map(|replacement| replacement.reason.as_str().to_owned()),
        last_health: proxy
            .last_health
            .as_ref()
            .and_then(|health| serde_json::to_value(health).ok()),
        created_at: proxy.created_at,
        retired_at: proxy.retired_at,
    }
}

/// The proxies.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct ProxyList {
    /// The proxies, oldest first.
    pub(crate) proxies: Vec<ProxyAdminView>,
}

/// Which proxies to list.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct ProxyQuery {
    /// Include retired proxies.
    #[serde(default)]
    all: bool,
}

/// List the proxies.
#[utoipa::path(
    get,
    path = "/v1/admin/proxies",
    tag = "admin",
    operation_id = "list_proxies",
    params(ProxyQuery),
    responses((status = 200, description = "The proxies.", body = ProxyList)),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn list_proxies(
    State(state): State<ApiState>,
    Params(query): Params<ProxyQuery>,
) -> ApiResult<Json<ProxyList>> {
    let proxies = state
        .store()
        .proxies()
        .list(None)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok(Json(ProxyList {
        proxies: proxies
            .iter()
            .filter(|proxy| query.all || proxy.retired_at.is_none())
            .map(proxy_view)
            .collect(),
    }))
}

fn pool_failed(error: &PoolError) -> ApiError {
    match error {
        PoolError::NotFound(_) => ApiError::new(ErrorCode::ProxyNotFound, "there is no such proxy"),
        PoolError::NoProvider => ApiError::new(
            ErrorCode::Conflict,
            "no proxy provider is configured: nothing can be replaced",
        )
        .with("reason", "no_provider"),
        other => ApiError::internal(other),
    }
}

/// What became of a request to replace a proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct ReplacementView {
    /// `requested`, `budget_exhausted`, `quota_exhausted`,
    /// `country_unavailable`, `unsupported`, `not_eligible` or `failed`.
    pub(crate) outcome: String,
    /// What the outcome means here.
    pub(crate) detail: String,
}

fn replacement_view(outcome: &ReplacementOutcome) -> ReplacementView {
    let (name, detail) = match outcome {
        ReplacementOutcome::Requested { .. } => (
            "requested",
            "the provider was asked; the proxy is replaced when the provider is done, and its \
             persona is retired with it"
                .to_owned(),
        ),
        ReplacementOutcome::BudgetExhausted { used, budget } => (
            "budget_exhausted",
            format!("{used} of {budget} replacements of this term are used"),
        ),
        ReplacementOutcome::QuotaExhausted => (
            "quota_exhausted",
            "the provider's plan has no replacements left".to_owned(),
        ),
        ReplacementOutcome::CountryUnavailable { country } => (
            "country_unavailable",
            format!("the provider has no proxy to offer in {country}"),
        ),
        ReplacementOutcome::Unsupported => (
            "unsupported",
            "the provider cannot replace proxies".to_owned(),
        ),
        ReplacementOutcome::NotEligible { state } => {
            ("not_eligible", format!("the proxy is {state}"))
        }
        ReplacementOutcome::Failed { .. } => (
            "failed",
            "the provider refused the request; the proxy is in backoff".to_owned(),
        ),
    };
    ReplacementView {
        outcome: name.to_owned(),
        detail,
    }
}

/// Ask the provider to replace a proxy, inside the replacement budget.
/// The proxy's persona is retired when the proxy is. A refusal by the
/// budget or by the provider is an `outcome`, not an error.
#[utoipa::path(
    post,
    path = "/v1/admin/proxies/{id}/replace",
    tag = "admin",
    operation_id = "replace_proxy",
    params(("id" = String, Path, format = Uuid, description = "The proxy's id.")),
    responses(
        (status = 200, description = "What became of the request.", body = ReplacementView),
        (status = 404, description = "`proxy_not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: no provider is configured.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn replace_proxy(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: ProxyId = parse_id(&id, ErrorCode::ProxyNotFound)?;
    let engine = engine(&state)?;
    let outcome = engine
        .pool
        .request_replacement(id, ReplaceReason::Operator)
        .await
        .map_err(|error| pool_failed(&error))?;
    let view = replacement_view(&outcome);
    let note = AuditNote::about(id).with(json!({ "outcome": view.outcome }));
    Ok(noted(Json(view), note))
}

/// Probe a proxy now: reachability, latency, the exit address, a leak.
#[utoipa::path(
    post,
    path = "/v1/admin/proxies/{id}/health",
    tag = "admin",
    operation_id = "check_proxy",
    params(("id" = String, Path, format = Uuid, description = "The proxy's id.")),
    responses(
        (status = 200, description = "The proxy after the probe.", body = ProxyAdminView),
        (status = 404, description = "`proxy_not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: the proxy is being replaced or is retired, and is not probed.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn check_proxy(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: ProxyId = parse_id(&id, ErrorCode::ProxyNotFound)?;
    let engine = engine(&state)?;
    let known = engine
        .pool
        .get(id)
        .await
        .map_err(|error| pool_failed(&error))?
        .ok_or_else(|| ApiError::new(ErrorCode::ProxyNotFound, "there is no such proxy"))?;
    let checked = engine
        .health
        .check_now(Some(&[id]))
        .await
        .map_err(|error| pool_failed(&error))?;
    let Some(result) = checked.into_iter().next() else {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            format!("the proxy is {} and is not probed", known.state),
        )
        .with("reason", "not_probed"));
    };
    let view = proxy_view(&result.proxy);
    let note = AuditNote::about(id).with(json!({ "state": view.state }));
    Ok(noted(Json(view), note))
}

/// The pool at a glance.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct PoolView {
    /// The proxies: by state, how many are usable, whether a leak is on
    /// record, and the replacement budget of the current term.
    #[schema(value_type = FreeForm)]
    pub(crate) proxies: Value,
    /// The personas: how many per state and per engine, how many are
    /// ready, and how many browsers are open.
    #[schema(value_type = FreeForm)]
    pub(crate) personas: Value,
    /// The bandwidth budget of the proxy plan: bytes counted by Kage and
    /// by the provider, the cap, the share used, and whether the hard stop
    /// is in force. Null until it was first judged.
    #[schema(value_type = Option<FreeForm>)]
    pub(crate) bandwidth: Option<Value>,
}

/// The pool and the bandwidth budget at a glance.
#[utoipa::path(
    get,
    path = "/v1/admin/pool",
    tag = "admin",
    operation_id = "get_pool",
    responses((status = 200, description = "The pool.", body = PoolView)),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn pool(State(state): State<ApiState>) -> ApiResult<Json<PoolView>> {
    let engine = engine(&state)?;
    let summary = engine
        .pool
        .summary()
        .await
        .map_err(|error| pool_failed(&error))?;
    let personas = state
        .store()
        .personas()
        .list_live()
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let count = |wanted: &dyn Fn(&Persona) -> bool| {
        personas.iter().filter(|persona| wanted(persona)).count()
    };
    let budget = match engine.bandwidth.last() {
        Some(budget) => Some(budget),
        None => engine.bandwidth.assess().await.ok(),
    };
    Ok(Json(PoolView {
        proxies: serde_json::to_value(&summary).map_err(|error| ApiError::internal(&error))?,
        personas: json!({
            "live": personas.len(),
            "ready": count(&|persona| persona.is_ready()),
            "active": count(&|persona| persona.state == PersonaState::Active),
            "cooling": count(&|persona| persona.state == PersonaState::Cooling),
            "by_engine": {
                "chromium": count(&|persona| persona.engine == kage_core::domain::Engine::Chromium),
                "firefox": count(&|persona| persona.engine == kage_core::domain::Engine::Firefox),
            },
            "browsers_open": engine.personas.contexts().open().len(),
        }),
        bandwidth: budget.and_then(|budget| serde_json::to_value(budget).ok()),
    }))
}

/// What is in force after a reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct PoliciesView {
    /// Domains with an entry of their own.
    pub(crate) domains: usize,
    /// Pacing profiles.
    pub(crate) pacing_profiles: usize,
    /// Block signature rules.
    pub(crate) signature_rules: usize,
}

/// Read the policy files again, now. An invalid file changes nothing: the
/// policies in force stay, and the answer says what is wrong.
#[utoipa::path(
    post,
    path = "/v1/admin/policies/reload",
    tag = "admin",
    operation_id = "reload_policies",
    responses(
        (status = 200, description = "The policies are in force.", body = PoliciesView),
        (status = 422, description = "`validation_failed`: a policy file is invalid; the last good policies stay in force.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn reload_policies(State(state): State<ApiState>) -> ApiResult<Response> {
    let policies = state.policies().reload().map_err(|error| {
        ApiError::new(ErrorCode::ValidationFailed, error.to_string())
            .with("kept", "the last good policies stay in force")
    })?;
    let view = PoliciesView {
        domains: policies.domains.domains().len(),
        pacing_profiles: policies.pacing.as_map().len(),
        signature_rules: policies.signatures.rules().len(),
    };
    let note = AuditNote::default().with(json!({
        "domains": view.domains,
        "pacing_profiles": view.pacing_profiles,
        "signature_rules": view.signature_rules,
    }));
    Ok(noted(Json(view), note))
}

// --- API keys ---------------------------------------------------------------

/// What an API key may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScopeField {
    /// Fetches, results, artifacts, the policy preview.
    Fetch,
    /// Batch and flow jobs (milestone M9).
    Jobs,
    /// Crawl jobs (milestone M10).
    Crawl,
    /// Sessions, and `session_id` in a fetch.
    Sessions,
    /// The admin endpoints, `/metrics` and `/docs`. Implies no other.
    Admin,
}

impl From<ScopeField> for Scope {
    fn from(scope: ScopeField) -> Self {
        match scope {
            ScopeField::Fetch => Self::Fetch,
            ScopeField::Jobs => Self::Jobs,
            ScopeField::Crawl => Self::Crawl,
            ScopeField::Sessions => Self::Sessions,
            ScopeField::Admin => Self::Admin,
        }
    }
}

impl From<Scope> for ScopeField {
    fn from(scope: Scope) -> Self {
        match scope {
            Scope::Fetch => Self::Fetch,
            Scope::Jobs => Self::Jobs,
            Scope::Crawl => Self::Crawl,
            Scope::Sessions => Self::Sessions,
            Scope::Admin => Self::Admin,
        }
    }
}

/// An API key. Its secret is not here: only its hash is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct KeyView {
    /// The key's id; the `<keyid>` of its token, with hyphens.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) id: ApiKeyId,
    /// Its name.
    pub(crate) name: String,
    /// What it may do.
    pub(crate) scopes: Vec<ScopeField>,
    /// Requests per minute.
    pub(crate) requests_per_minute: u32,
    /// Requests in flight at once.
    pub(crate) max_concurrent: u32,
    /// Pages per day (UTC); null for no budget.
    pub(crate) daily_page_budget: Option<u32>,
    /// Whether it may authenticate requests.
    pub(crate) active: bool,
    /// When it was disabled.
    pub(crate) disabled_at: Option<DateTime<Utc>>,
    /// When it was made.
    pub(crate) created_at: DateTime<Utc>,
    /// When it last authenticated a request, to the minute.
    pub(crate) last_used_at: Option<DateTime<Utc>>,
}

fn key_view(key: &ApiKey) -> KeyView {
    KeyView {
        id: key.id,
        name: key.name.clone(),
        scopes: key.scopes.iter().copied().map(ScopeField::from).collect(),
        requests_per_minute: key.limits.requests_per_minute,
        max_concurrent: key.limits.max_concurrent,
        daily_page_budget: key.limits.daily_page_budget,
        active: key.is_active(),
        disabled_at: key.disabled_at.map(stored),
        created_at: stored(key.created_at),
        last_used_at: key.last_used_at.map(stored),
    }
}

/// The keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct KeyList {
    /// The keys, oldest first, disabled ones included.
    pub(crate) keys: Vec<KeyView>,
}

/// List the API keys.
#[utoipa::path(
    get,
    path = "/v1/admin/keys",
    tag = "admin",
    operation_id = "list_keys",
    responses((status = 200, description = "The keys.", body = KeyList)),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn list_keys(State(state): State<ApiState>) -> ApiResult<Json<KeyList>> {
    let keys = state
        .store()
        .api_keys()
        .list()
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok(Json(KeyList {
        keys: keys.iter().map(key_view).collect(),
    }))
}

/// What a new key is to be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KeyRequest {
    /// A name for humans: 1 to 100 characters.
    #[schema(max_length = 100, example = "project-a")]
    pub(crate) name: String,
    /// What the key may do; at least one scope.
    #[schema(min_items = 1)]
    pub(crate) scopes: Vec<ScopeField>,
    /// Requests per minute; 60 when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1)]
    pub(crate) requests_per_minute: Option<u32>,
    /// Requests in flight at once; 4 when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1)]
    pub(crate) max_concurrent: Option<u32>,
    /// Pages per day (UTC); no budget when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1)]
    pub(crate) daily_page_budget: Option<u32>,
}

/// A key with its token, which is shown this once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub(crate) struct KeyCreated {
    /// The key.
    pub(crate) key: KeyView,
    /// The token: `Authorization: Bearer <token>`. Kage keeps only a hash
    /// of its secret and cannot show it again.
    #[schema(example = "kage_0199c1f26b7a7c3e9d412f8a5b6c7d8e_k3J...")]
    pub(crate) token: String,
}

fn key_refused(error: &KeyError) -> ApiError {
    match error {
        KeyError::Name => ApiError::field("name", error.to_string()),
        KeyError::NoScopes => ApiError::field("scopes", error.to_string()),
        KeyError::Limit(field) => ApiError::field(field, error.to_string()),
        KeyError::Random => ApiError::internal(error),
    }
}

/// A token is not to be kept by anything between the server and its
/// reader.
fn shown_once(status: StatusCode, body: &KeyCreated, note: AuditNote) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.extensions_mut().insert(note);
    response
}

/// Make an API key. Its token is in the answer and nowhere else.
#[utoipa::path(
    post,
    path = "/v1/admin/keys",
    tag = "admin",
    operation_id = "create_key",
    request_body = KeyRequest,
    responses(
        (status = 201, description = "The key and its token.", body = KeyCreated),
        (status = 422, description = "The name, the scopes or a limit is not acceptable.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn create_key(
    State(state): State<ApiState>,
    Body(request): Body<KeyRequest>,
) -> ApiResult<Response> {
    let new = NewKey {
        name: request.name,
        scopes: request
            .scopes
            .iter()
            .copied()
            .map(Scope::from)
            .collect::<BTreeSet<_>>(),
        limits: KeyLimits {
            requests_per_minute: request
                .requests_per_minute
                .unwrap_or(DEFAULT_LIMITS.requests_per_minute),
            max_concurrent: request
                .max_concurrent
                .unwrap_or(DEFAULT_LIMITS.max_concurrent),
            daily_page_budget: request.daily_page_budget,
        },
    };
    let (key, token) =
        keys::mint(&new, state.clock().now()).map_err(|error| key_refused(&error))?;
    state
        .store()
        .api_keys()
        .insert(&key)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let view = key_view(&key);
    let note = AuditNote::about(key.id).with(json!({
        "name": view.name,
        "scopes": view.scopes,
        "requests_per_minute": view.requests_per_minute,
        "max_concurrent": view.max_concurrent,
        "daily_page_budget": view.daily_page_budget,
    }));
    Ok(shown_once(
        StatusCode::CREATED,
        &KeyCreated {
            key: view,
            token: token.reveal().to_owned(),
        },
        note,
    ))
}

async fn known_key(state: &ApiState, id: ApiKeyId) -> ApiResult<ApiKey> {
    state
        .store()
        .api_keys()
        .get(id)
        .await
        .map_err(|error| ApiError::internal(&error))?
        .ok_or_else(|| ApiError::new(ErrorCode::KeyNotFound, "there is no such API key"))
}

/// Disable an API key. It stops authenticating at once; its row and its
/// audit trail stay. Disabling a disabled key changes nothing.
#[utoipa::path(
    delete,
    path = "/v1/admin/keys/{id}",
    tag = "admin",
    operation_id = "revoke_key",
    params(("id" = String, Path, format = Uuid, description = "The key's id.")),
    responses(
        (status = 200, description = "The key, disabled.", body = KeyView),
        (status = 404, description = "`key_not_found`.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn revoke_key(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: ApiKeyId = parse_id(&id, ErrorCode::KeyNotFound)?;
    let mut key = known_key(&state, id).await?;
    if key.is_active() {
        let now = state.clock().now();
        state
            .store()
            .api_keys()
            .set_disabled(id, Some(now))
            .await
            .map_err(|error| ApiError::internal(&error))?;
        key.disabled_at = Some(now);
    }
    Ok(noted(
        Json(key_view(&key)),
        AuditNote::about(id).with(json!({ "name": key.name })),
    ))
}

/// Give an API key a new secret. The old token stops working at once; the
/// new one is in the answer and nowhere else.
#[utoipa::path(
    post,
    path = "/v1/admin/keys/{id}/rotate",
    tag = "admin",
    operation_id = "rotate_key",
    params(("id" = String, Path, format = Uuid, description = "The key's id.")),
    responses(
        (status = 200, description = "The key and its new token.", body = KeyCreated),
        (status = 404, description = "`key_not_found`.", body = Problem, content_type = "application/problem+json"),
        (status = 409, description = "`conflict`: the key is disabled.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn rotate_key(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let id: ApiKeyId = parse_id(&id, ErrorCode::KeyNotFound)?;
    let key = known_key(&state, id).await?;
    if !key.is_active() {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            "the key is disabled; a disabled key is not given a new secret",
        )
        .with("reason", "disabled"));
    }
    let (hash, token) = keys::rotate(id).map_err(|error| key_refused(&error))?;
    state
        .store()
        .api_keys()
        .set_secret_hash(id, &hash)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    Ok(shown_once(
        StatusCode::OK,
        &KeyCreated {
            key: key_view(&key),
            token: token.reveal().to_owned(),
        },
        AuditNote::about(id).with(json!({ "name": key.name })),
    ))
}

// --- The audit trail --------------------------------------------------------

/// One entry of the audit trail.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct AuditView {
    /// The event's id; ids sort by time.
    #[schema(value_type = String, format = Uuid)]
    pub(crate) id: AuditEventId,
    /// When it happened.
    pub(crate) at: DateTime<Utc>,
    /// Who did it: `api_key`, `cli` or `system`.
    pub(crate) actor: String,
    /// The key, when the actor was one.
    #[schema(value_type = Option<String>, format = Uuid)]
    pub(crate) api_key_id: Option<ApiKeyId>,
    /// What was done, for example `admin.persona.retire`.
    pub(crate) action: String,
    /// What it was done to.
    pub(crate) target: Option<String>,
    /// `success`, `failure` or `denied`.
    pub(crate) outcome: String,
    /// What came of it. Never a secret.
    #[schema(value_type = AnyJson)]
    pub(crate) details: Value,
}

fn audit_view(event: AuditEvent) -> AuditView {
    AuditView {
        id: event.id,
        at: event.at,
        actor: event.actor.kind().to_owned(),
        api_key_id: match event.actor {
            AuditActor::ApiKey(id) => Some(id),
            AuditActor::Cli | AuditActor::System => None,
        },
        action: event.action,
        target: event.target,
        outcome: event.outcome.as_str().to_owned(),
        details: event.details,
    }
}

/// Which part of the audit trail to read.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(crate) struct AuditQuery {
    /// The `next_cursor` of the page before; left out for the newest.
    cursor: Option<String>,
    /// How many events, at most 200; 50 when left out.
    limit: Option<u32>,
}

/// A page of the audit trail.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct AuditPage {
    /// The events, newest first.
    pub(crate) events: Vec<AuditView>,
    /// Pass it as `cursor` for the events before these; null at the end.
    #[schema(value_type = Option<String>, format = Uuid)]
    pub(crate) next_cursor: Option<AuditEventId>,
}

/// Read the audit trail, newest first: every admin call through the API
/// and every change an operator made at the command line.
#[utoipa::path(
    get,
    path = "/v1/admin/audit",
    tag = "admin",
    operation_id = "list_audit",
    params(AuditQuery),
    responses((status = 200, description = "A page of events.", body = AuditPage)),
    security(("api_key" = ["admin"]))
)]
pub(crate) async fn list_audit(
    State(state): State<ApiState>,
    Params(query): Params<AuditQuery>,
) -> ApiResult<Json<AuditPage>> {
    let before = query
        .cursor
        .as_deref()
        .map(|cursor| {
            cursor.parse::<AuditEventId>().map_err(|_| {
                ApiError::field("cursor", "the next_cursor of an earlier page").with("in", "query")
            })
        })
        .transpose()?;
    let limit = query.limit.unwrap_or(50).clamp(1, MAX_AUDIT_PAGE);
    let events = state
        .store()
        .audit_events()
        .list(before, limit)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let full = u32::try_from(events.len()).is_ok_and(|count| count == limit);
    let next_cursor = events.last().map(|event| event.id).filter(|_| full);
    Ok(Json(AuditPage {
        events: events.into_iter().map(audit_view).collect(),
        next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::Utc;
    use kage_core::domain::{
        ApiKey, AuditActor, AuditEvent, AuditOutcome, Engine, KeyLimits, Scope,
    };
    use kage_core::ids::{ApiKeyId, AuditEventId, PersonaId, ProxyId};
    use kage_persona::PersonaError;
    use kage_proxy::{PoolError, ReplacementOutcome};
    use serde_json::json;

    use super::{
        audit_view, key_view, persona_failed, persona_view, pool_failed, replacement_view,
    };
    use crate::error::ErrorCode;
    use crate::test_support::persona;

    #[test]
    fn a_persona_is_shown_whole_and_its_fingerprint_only_when_asked() {
        let chrome = persona(Engine::Chromium, 155);

        let view = serde_json::to_value(persona_view(&chrome, &[chrome.id], false)).unwrap();

        assert_eq!(view["id"], json!(chrome.id));
        assert_eq!(view["engine"], "chromium");
        assert_eq!(view["claimed_os"], "linux");
        assert_eq!(view["state"], "active");
        assert_eq!(view["ready"], true);
        assert_eq!(view["browser_open"], true);
        assert_eq!(view["country"], "DE");
        assert_eq!(view["screen"]["width"], 1920);
        // Chrome 155 has no verified http profile.
        assert_eq!(view["http_profile"], json!(null));
        assert!(view.get("fingerprint").is_none());
        // The seed is never part of a view.
        assert!(view.get("seed").is_none());

        let with = serde_json::to_value(persona_view(&chrome, &[], true)).unwrap();
        assert_eq!(with["fingerprint"]["engine"], "chromium");
        assert_eq!(with["browser_open"], false);
    }

    #[test]
    fn a_key_is_shown_without_its_secret() {
        let key = ApiKey {
            id: ApiKeyId::new(),
            name: "project-a".to_owned(),
            secret_hash: [0xab; 32],
            scopes: BTreeSet::from([Scope::Fetch, Scope::Admin]),
            limits: KeyLimits {
                requests_per_minute: 30,
                max_concurrent: 2,
                daily_page_budget: Some(500),
            },
            disabled_at: None,
            created_at: Utc::now(),
            last_used_at: None,
        };

        let view = serde_json::to_value(key_view(&key)).unwrap();

        assert_eq!(view["scopes"], json!(["fetch", "admin"]));
        assert_eq!(view["daily_page_budget"], 500);
        assert_eq!(view["active"], true);
        let text = view.to_string();
        assert!(!text.contains("abab") && !text.contains("secret"), "{text}");
    }

    #[test]
    fn failures_of_the_manager_and_the_pool_are_told_by_code_and_never_by_cause() {
        let id = PersonaId::new();
        for (error, code, reason) in [
            (PersonaError::NotFound(id), ErrorCode::PersonaNotFound, None),
            (
                PersonaError::Retired(id),
                ErrorCode::Conflict,
                Some("retired"),
            ),
            (
                PersonaError::EgressUnsafe,
                ErrorCode::Conflict,
                Some("egress_unsafe"),
            ),
            (
                PersonaError::NothingToTest("no page".to_owned()),
                ErrorCode::Conflict,
                Some("nothing_to_test"),
            ),
            (
                PersonaError::Mix("10.0.0.7 said no".to_owned()),
                ErrorCode::Internal,
                None,
            ),
        ] {
            let told = persona_failed(&error);
            assert_eq!(told.code(), code, "{error}");
            assert_eq!(
                told.problem()
                    .extensions
                    .get("reason")
                    .and_then(|r| r.as_str()),
                reason,
                "{error}"
            );
            assert!(!told.detail().contains("10.0.0.7"), "{}", told.detail());
        }

        assert_eq!(
            pool_failed(&PoolError::NotFound(ProxyId::new())).code(),
            ErrorCode::ProxyNotFound
        );
        assert_eq!(
            pool_failed(&PoolError::NoProvider).code(),
            ErrorCode::Conflict
        );
        assert_eq!(
            pool_failed(&PoolError::Setup("key at 10.0.0.7".to_owned())).code(),
            ErrorCode::Internal
        );
    }

    #[test]
    fn a_replacement_that_was_refused_is_an_outcome() {
        let requested = replacement_view(&ReplacementOutcome::Requested {
            ticket: Some("t-1".to_owned()),
        });
        assert_eq!(requested.outcome, "requested");
        // The provider's ticket stays inside.
        assert!(!requested.detail.contains("t-1"));

        let spent = replacement_view(&ReplacementOutcome::BudgetExhausted { used: 5, budget: 5 });
        assert_eq!(spent.outcome, "budget_exhausted");
        assert!(spent.detail.contains("5 of 5"));
        assert_eq!(
            replacement_view(&ReplacementOutcome::QuotaExhausted).outcome,
            "quota_exhausted"
        );
        assert_eq!(
            replacement_view(&ReplacementOutcome::CountryUnavailable {
                country: "AT".parse().unwrap()
            })
            .detail,
            "the provider has no proxy to offer in AT"
        );
        assert_eq!(
            replacement_view(&ReplacementOutcome::Unsupported).outcome,
            "unsupported"
        );
    }

    #[test]
    fn an_audit_event_names_its_actor() {
        let key = ApiKeyId::new();
        let event = |actor| AuditEvent {
            id: AuditEventId::new(),
            at: Utc::now(),
            actor,
            action: "admin.persona.retire".to_owned(),
            target: Some("0199".to_owned()),
            outcome: AuditOutcome::Denied,
            details: json!({ "status": 403 }),
        };

        let by_key = audit_view(event(AuditActor::ApiKey(key)));
        assert_eq!(by_key.actor, "api_key");
        assert_eq!(by_key.api_key_id, Some(key));
        assert_eq!(by_key.outcome, "denied");
        let by_cli = audit_view(event(AuditActor::Cli));
        assert_eq!((by_cli.actor.as_str(), by_cli.api_key_id), ("cli", None));
    }
}
