//! `GET /v1/policies/domains/{domain}`: what Kage would do with a request
//! for a host, before one is made (spec §16.3).

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use kage_core::domain::Scope;
use kage_core::pacing::PacingProfile;
use kage_core::policy::{EffectivePolicy, normalize_host};
use serde::Serialize;
use utoipa::ToSchema;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult, Problem};
use crate::schema::FreeForm;
use crate::state::ApiState;

/// The policy in force for a host.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub(crate) struct PolicyPreview {
    /// Every field of the policy as it resolves for the host: `host`,
    /// `matched` (the entry that applies), `inherits`, `min_level`,
    /// `countries`, `engines`, `pacing`, `pacing_locked`, `robots`,
    /// `referer_strategy`, `active_hours`, `min_dwell`, `behavior_locked`,
    /// `retries`, `breaker_threshold`, `breaker_pause`, `cooldowns`.
    #[schema(value_type = FreeForm)]
    pub(crate) policy: EffectivePolicy,
    /// The pacing profile the policy names, in the format of
    /// `policies/pacing-profiles.yaml`.
    #[schema(value_type = FreeForm)]
    pub(crate) pacing_profile: PacingProfile,
    /// The lowest level a request for the host may name (spec §25): one
    /// below it is refused with `level_below_policy`, never raised.
    #[schema(minimum = 0, maximum = 3, example = 0)]
    pub(crate) min_level: u64,
    /// Until when the host is paused because too many requests to it were
    /// blocked; null when it is not. A request made before then ends as
    /// `provider_error` with reason `domain_paused`.
    pub(crate) paused_until: Option<DateTime<Utc>>,
}

/// What Kage would do with a request for a host.
///
/// The policy is resolved as a request's would be: the host's own entry,
/// then its parent domains', then the defaults. Nothing is fetched and no
/// name is looked up.
#[utoipa::path(
    get,
    path = "/v1/policies/domains/{domain}",
    tag = "fetch",
    operation_id = "preview_policy",
    params(("domain" = String, Path, description = "A host name, for example `shop.example.org`.")),
    responses(
        (status = 200, description = "The policy in force for the host.", body = PolicyPreview),
        (status = 422, description = "Not a host name.", body = Problem, content_type = "application/problem+json"),
    ),
    security(("api_key" = ["fetch"]))
)]
pub(crate) async fn preview(
    State(state): State<ApiState>,
    caller: Caller,
    Path(domain): Path<String>,
) -> ApiResult<Json<PolicyPreview>> {
    caller.require(Scope::Fetch)?;
    let host = normalize_host(&domain)
        .filter(|_| !domain.contains('/'))
        .ok_or_else(|| ApiError::field("domain", "a host name, without a scheme or a path"))?;
    let policies = state.policies().current();
    let policy = policies.domains.resolve(&host);
    let pacing = policy
        .pacing_for(None, &policies.pacing)
        .map_err(|error| ApiError::internal(&error))?;
    let now = state.clock().now();
    let breaker = state
        .store()
        .domain_breakers()
        .get(&host)
        .await
        .map_err(|error| ApiError::internal(&error))?;
    let paused_until = breaker
        .filter(|breaker| breaker.is_open(now))
        .and_then(|breaker| breaker.paused_until);
    Ok(Json(PolicyPreview {
        pacing_profile: pacing.profile,
        min_level: u64::from(policy.min_level.number()),
        paused_until,
        policy,
    }))
}
