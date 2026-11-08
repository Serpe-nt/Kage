//! `GET /v1/policies/domains/{domain}`: what Kage would do with a request
//! for a host, before one is made (spec §16.3).

use axum::Json;
use axum::extract::{Path, State};
use chrono::{DateTime, Utc};
use kage_core::domain::Scope;
use kage_core::pacing::PacingProfile;
use kage_core::policy::{EffectivePolicy, is_host_name, normalize_host};
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
    /// `countries`, `engines`, `pacing`, `pacing_locked`, `robots`, `cache` (as it
    /// holds under the configuration's `cache.max_ttl`),
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

/// The host a path names, normalized, when it is one: a host name (no
/// wildcard, no empty label, no label over 63 characters, RFC 1035) or an
/// address in brackets. What the policy preview (F136) and a site's scout
/// (QA round three api-03) take.
pub(crate) fn host_named(text: &str) -> Option<String> {
    normalize_host(text).filter(|host| {
        !text.contains('/') && !text.contains('*') && (host.starts_with('[') || is_host_name(host))
    })
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
    // What is no host name has no policy: a wildcard, an empty label, a
    // label too long (F136).
    let host = host_named(&domain).ok_or_else(|| {
        ApiError::field(
            "domain",
            "a host name, without a scheme, a path or a wildcard: an entry already covers its \
             subdomains",
        )
    })?;
    let policies = state.policies().current();
    let mut policy = policies.domains.resolve(&host);
    // The cache as it holds under the configuration's bound.
    policy.cache = policy.cache.bounded(state.config().cache.max_ttl.get());
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
