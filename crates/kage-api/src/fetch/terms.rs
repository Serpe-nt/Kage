//! What a site's policy and the configuration make of what a request says
//! about `robots.txt`, the response cache and extraction (spec §14, §15).
//!
//! [`request::plan`](super::request::plan) checks a request by itself.
//! What is checked here needs the site: whether the request's wish about
//! `robots.txt` is one its policy allows, for how long its result may be
//! served from the cache and to whom, and the key the cache knows it by.
//! Policies are reloaded while the server runs, so a request is settled
//! when it is accepted and again when it is worked.

use std::time::Duration;

use kage_core::cache::CacheKey;
use kage_core::config::Config;
use kage_core::ids::ApiKeyId;
use kage_core::policy::PolicySet;
use kage_scheduler::{CachePlan, Conflict, robots};
use serde_json::{Map, Value};

use super::request::{FetchRequest, Planned};
use crate::error::{ApiError, ApiResult, ErrorCode};

/// What a request may do with the response cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheUse {
    /// A fetch and an item of a batch: answered from the cache when it
    /// holds the page, and fills it.
    ReadAndFill,
    /// A page of a crawl: it is a visit every time (a page that is not
    /// open in the tab cannot be clicked from), and what it read is there
    /// for later fetches.
    Fill,
    /// A flow: a script in a browser, which the cache knows nothing of.
    Nothing,
}

/// The fields of a request that shape what its result holds besides the
/// address, the method, the body, the headers, the render mode and the
/// include set: what the browser waits for, what the persona does on the
/// page, how a screenshot and a PDF are made, which responses are kept,
/// what is extracted, the size limit, and the persona hints.
const SHAPING: [&str; 8] = [
    "wait",
    "behavior",
    "screenshot",
    "pdf",
    "xhr",
    "extract",
    "max_body_bytes",
    "persona",
];

fn conflict(conflict: Conflict, host: &str) -> ApiError {
    let (code, detail) = match conflict {
        Conflict::Enforced => (
            ErrorCode::RobotsEnforced,
            format!(
                "the policy for {host} enforces its robots.txt, and a request cannot ask for \
                 less: leave robots out, or say enforce"
            ),
        ),
        Conflict::Ignored => (
            ErrorCode::RobotsIgnored,
            format!(
                "the policy for {host} never asks for its robots.txt, so nothing could be \
                 enforced: a reader does not ask for that file, and a request does not make \
                 Kage ask. Leave robots out, or have the operator set robots for the site"
            ),
        ),
    };
    ApiError::new(code, detail).with("field", "robots")
}

/// Settles what a planned request says about `robots.txt`, the cache and
/// extraction against the policy of its site and the configuration, and
/// completes its order with them.
///
/// # Errors
///
/// `robots_enforced` and `robots_ignored` when the request's wish and the
/// policy cannot both be had; `cache_ttl_too_long` when the request asks
/// for a longer life than results of the site may have; a validation
/// failure for a request in a session that names a cache lifetime.
pub(crate) fn settle(
    planned: &mut Planned,
    request: &FetchRequest,
    config: &Config,
    policies: &PolicySet,
    key: ApiKeyId,
    cache: CacheUse,
) -> ApiResult<()> {
    let host = planned
        .order
        .url
        .host_str()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let policy = policies.domains.resolve(&host);

    robots::mode(policy.robots, planned.order.robots).map_err(|wish| conflict(wish, &host))?;

    if let Some(options) = planned.order.extract.as_mut() {
        options.thin_words = usize::try_from(config.extract.thin_words).unwrap_or(usize::MAX);
    }

    planned.order.cache = None;
    let asked = request.cache_ttl_s.map(Duration::from_secs);
    let ttl = policy
        .cache
        .ttl_for(asked, config.cache.max_ttl.get())
        .map_err(|longest| {
            ApiError::new(
                ErrorCode::CacheTtlTooLong,
                format!(
                    "cache_ttl_s is at most {} for {host}: results are not served for longer",
                    longest.as_secs()
                ),
            )
            .with("field", "cache_ttl_s")
            .with("maximum_s", longest.as_secs())
        })?;
    let Some(ttl) = ttl else {
        return Ok(());
    };
    if planned.session.is_some() {
        // What a session sees depends on its cookies. A request that asks
        // for the cache there is told; one that only inherits the policy's
        // default is fetched like any request of a session.
        if asked.is_some() {
            return Err(ApiError::field(
                "cache_ttl_s",
                "a request in a session is never answered from the cache and never fills it: \
                 what a session sees depends on its cookies. Leave cache_ttl_s out",
            ));
        }
        return Ok(());
    }
    if cache == CacheUse::Nothing {
        return Ok(());
    }

    let scope = (!policy.cache.shared).then_some(key);
    let written = serde_json::to_value(request).map_err(|error| ApiError::internal(&error))?;
    let options: Map<String, Value> = SHAPING
        .iter()
        .filter_map(|name| Some(((*name).to_owned(), written.get(name)?.clone())))
        .collect();
    let options = Value::Object(options);
    let include = planned.include.names();
    let order = &planned.order;
    let digest = CacheKey {
        url: &order.url,
        method: order.method.as_str(),
        body: order.body.as_deref(),
        request_kind: request.request_kind.as_str(),
        headers: &order.headers,
        render: order.render,
        include: &include,
        options: &options,
        scope,
    }
    // The address as a crawl normalizes it (the configuration's `urls`).
    .digest(&config.urls);
    planned.order.cache = Some(CachePlan {
        key: digest.key,
        url_key: digest.url_key,
        scope,
        ttl,
        read: cache == CacheUse::ReadAndFill,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use kage_core::config::Config;
    use kage_core::ids::ApiKeyId;
    use kage_core::pacing::PacingProfiles;
    use kage_core::policy::{DomainPolicies, PolicySet};
    use kage_scheduler::{CachePlan, RobotsWish};
    use serde_json::{Value, json};

    use super::{CacheUse, settle};
    use crate::error::{ApiError, ErrorCode};
    use crate::fetch::request::{FetchRequest, Planned, plan};

    const HUMAN: &str = "
human:
  gap_ms: { dist: lognormal, median: 4500, p95: 15000, min: 1500 }
  burst_pages: { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 60000, p95: 240000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
";

    const SIGNATURES: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../policies/block-signatures.yaml"
    ));

    const DOMAINS: &str = "
domains:
  enforced.example: { robots: enforce }
  reported.example: { robots: report }
  shared.example: { cache: { shared: true, max_ttl: 10m, ttl: 2m } }
";

    fn config(overrides: &[(&str, &str)]) -> Config {
        let base = [
            ("KAGE__DATABASE__URL", "postgres://kage@localhost/kage"),
            ("KAGE__SIDECAR__TOKEN", "token"),
            (
                "KAGE__SECRETS__MASTER_KEY",
                "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
            ),
        ];
        Config::load(
            None,
            base.iter()
                .chain(overrides)
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
        )
        .unwrap()
    }

    fn policies() -> PolicySet {
        PolicySet {
            domains: DomainPolicies::from_yaml(DOMAINS).unwrap(),
            pacing: PacingProfiles::from_yaml(HUMAN).unwrap(),
            signatures: kage_core::classifier::SignatureSet::from_yaml(SIGNATURES).unwrap(),
        }
    }

    fn settled_as(body: Value, key: ApiKeyId, cache: CacheUse) -> Result<Planned, ApiError> {
        let request: FetchRequest = serde_json::from_value(body).unwrap();
        let policies = policies();
        let mut planned = plan(&request, &policies.pacing).unwrap();
        settle(&mut planned, &request, &config(&[]), &policies, key, cache)?;
        Ok(planned)
    }

    fn settled(body: Value) -> Result<Planned, ApiError> {
        settled_as(body, ApiKeyId::new(), CacheUse::ReadAndFill)
    }

    fn plan_of(body: Value, key: ApiKeyId) -> CachePlan {
        settled_as(body, key, CacheUse::ReadAndFill)
            .unwrap()
            .order
            .cache
            .unwrap()
    }

    #[test]
    fn a_wish_about_robots_txt_is_checked_against_the_sites_policy() {
        let robots = |host: &str, wish: &str| {
            settled(json!({ "url": format!("https://{host}/a"), "robots": wish }))
        };
        // Whatever the policy says is fine by a request that says nothing.
        for host in ["quiet.example", "reported.example", "enforced.example"] {
            let planned = settled(json!({ "url": format!("https://{host}/a") })).unwrap();
            assert_eq!(planned.order.robots, RobotsWish::Policy);
            assert!(robots(host, "policy").is_ok());
        }
        for (host, wish) in [
            ("quiet.example", "ignore"),
            ("quiet.example", "report"),
            ("reported.example", "ignore"),
            ("reported.example", "report"),
            ("reported.example", "enforce"),
            ("www.enforced.example", "enforce"),
        ] {
            assert!(robots(host, wish).is_ok(), "{host} {wish}");
        }
        assert_eq!(
            robots("reported.example", "enforce").unwrap().order.robots,
            RobotsWish::Enforce
        );
        // Less than the policy enforces, and what the policy never asks.
        for wish in ["ignore", "report"] {
            let refused = robots("www.enforced.example", wish).unwrap_err();
            assert_eq!(refused.code(), ErrorCode::RobotsEnforced);
            assert_eq!(refused.problem().extensions["field"], "robots");
        }
        let refused = robots("quiet.example", "enforce").unwrap_err();
        assert_eq!(refused.code(), ErrorCode::RobotsIgnored);
        assert_eq!(refused.problem().extensions["field"], "robots");
    }

    #[test]
    fn extraction_calls_thin_what_the_configuration_says() {
        let request: FetchRequest = serde_json::from_value(json!({
            "url": "https://quiet.example/a",
            "include": ["extract"],
            "extract": { "markdown": false },
        }))
        .unwrap();
        let policies = policies();
        let mut planned = plan(&request, &policies.pacing).unwrap();
        let config = config(&[("KAGE__EXTRACT__THIN_WORDS", "25")]);
        let key = ApiKeyId::new();
        settle(
            &mut planned,
            &request,
            &config,
            &policies,
            key,
            CacheUse::ReadAndFill,
        )
        .unwrap();
        let options = planned.order.extract.unwrap();
        assert_eq!(options.thin_words, 25);
        assert!(!options.markdown && options.links);
        // Nothing is extracted for a request that does not ask.
        assert!(
            settled(json!({ "url": "https://quiet.example/a" }))
                .unwrap()
                .order
                .extract
                .is_none()
        );
    }

    #[test]
    fn a_result_is_served_from_the_cache_for_as_long_as_asked_and_allowed() {
        let key = ApiKeyId::new();
        let url = "https://quiet.example/a?utm_source=x&b=1";
        // Nothing asked, no default: no cache.
        assert!(
            settled(json!({ "url": url }))
                .unwrap()
                .order
                .cache
                .is_none()
        );
        // Zero is never.
        assert!(
            settled(json!({ "url": url, "cache_ttl_s": 0 }))
                .unwrap()
                .order
                .cache
                .is_none()
        );
        // What is asked for: the key's own, read and filled.
        let plan = plan_of(json!({ "url": url, "cache_ttl_s": 600 }), key);
        assert_eq!(plan.ttl, Duration::from_mins(10));
        assert_eq!(plan.scope, Some(key));
        assert!(plan.read);
        assert_eq!(plan.key.len(), 64);
        // The address as a crawl normalizes it.
        assert_eq!(plan.url_key, "https://quiet.example/a?b=1");
        // Up to the bound and no further.
        assert!(settled(json!({ "url": url, "cache_ttl_s": 86_400 })).is_ok());
        let refused = settled(json!({ "url": url, "cache_ttl_s": 86_401 })).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::CacheTtlTooLong);
        assert_eq!(refused.problem().extensions["field"], "cache_ttl_s");
        assert_eq!(refused.problem().extensions["maximum_s"], 86_400);

        // A site whose policy shares its pages, bounds them and has a
        // default: nobody's entry, the default for a request that says
        // nothing, never for one that says 0.
        let shared = "https://www.shared.example/a";
        let plan = plan_of(json!({ "url": shared }), key);
        assert_eq!((plan.scope, plan.ttl), (None, Duration::from_mins(2)));
        assert!(
            settled(json!({ "url": shared, "cache_ttl_s": 0 }))
                .unwrap()
                .order
                .cache
                .is_none()
        );
        assert_eq!(
            plan_of(json!({ "url": shared, "cache_ttl_s": 600 }), key).ttl,
            Duration::from_mins(10)
        );
        let refused = settled(json!({ "url": shared, "cache_ttl_s": 601 })).unwrap_err();
        assert_eq!(refused.problem().extensions["maximum_s"], 600);

        // A crawl's page fills and never reads; a flow has no part in it.
        let fill = settled_as(json!({ "url": shared }), key, CacheUse::Fill)
            .unwrap()
            .order
            .cache
            .unwrap();
        assert!(!fill.read);
        assert_eq!(fill.key, plan.key);
        assert!(
            settled_as(json!({ "url": shared }), key, CacheUse::Nothing)
                .unwrap()
                .order
                .cache
                .is_none()
        );
        // A session stays out of it: told when it asks, silently when it
        // only inherits the site's default.
        let refused =
            settled(json!({ "url": url, "session_id": "s1", "cache_ttl_s": 60 })).unwrap_err();
        assert_eq!(refused.code(), ErrorCode::ValidationFailed);
        assert_eq!(refused.problem().extensions["field"], "cache_ttl_s");
        assert!(
            settled(json!({ "url": shared, "session_id": "s1" }))
                .unwrap()
                .order
                .cache
                .is_none()
        );
        assert!(settled(json!({ "url": url, "session_id": "s1", "cache_ttl_s": 0 })).is_ok());
    }

    #[test]
    fn the_key_of_the_cache_is_what_changes_what_a_result_holds() {
        let key = ApiKeyId::new();
        let base = json!({ "url": "https://quiet.example/a?b=1", "cache_ttl_s": 600 });
        let with = |change: Value| {
            let mut body = base.clone();
            for (name, value) in change.as_object().unwrap() {
                body[name.as_str()] = value.clone();
            }
            plan_of(body, key).key
        };
        let plain = with(json!({}));

        // The same request is the same key, however it is spelled.
        for same in [
            json!({ "url": "https://QUIET.example/a?b=1#top" }),
            json!({ "url": "https://quiet.example/a?b=1&utm_campaign=x" }),
            json!({ "include": ["headers", "dom"] }),
            json!({ "render": "auto", "method": "GET" }),
            // How long, how patiently and how politely is no part of it.
            json!({ "cache_ttl_s": 60 }),
            json!({ "timeout_ms": 20_000, "max_retries": 0 }),
            json!({ "pacing": { "profile": "human" } }),
            json!({ "robots": "report" }),
        ] {
            assert_eq!(with(same.clone()), plain, "{same}");
        }
        // What changes what the result holds is another key.
        let mut seen = vec![plain.clone()];
        for other in [
            json!({ "url": "https://quiet.example/a?b=2" }),
            json!({ "url": "http://quiet.example/a?b=1" }),
            json!({ "render": "browser" }),
            json!({ "render": "http" }),
            json!({ "render": "http", "method": "HEAD" }),
            json!({ "render": "http", "headers": { "X-Api": "1" } }),
            json!({ "render": "http", "method": "POST", "body": "a=1",
                    "headers": { "Content-Type": "text/plain" } }),
            json!({ "render": "http", "method": "POST", "body": "a=2",
                    "headers": { "Content-Type": "text/plain" } }),
            json!({ "include": ["dom"] }),
            json!({ "include": ["dom", "headers", "extract"] }),
            json!({ "include": ["dom", "headers", "extract"], "extract": { "links": false } }),
            json!({ "include": ["dom", "headers", "screenshot"] }),
            json!({ "include": ["dom", "headers", "screenshot"],
                    "screenshot": { "full_page": true } }),
            json!({ "wait": { "until": "network_idle" } }),
            json!({ "behavior": { "scroll_depth": 0.2 } }),
            json!({ "max_body_bytes": 1000 }),
            json!({ "persona": { "countries": ["DE"] } }),
            json!({ "persona": { "engines": ["firefox"] } }),
        ] {
            let key = with(other.clone());
            assert!(!seen.contains(&key), "{other}");
            seen.push(key);
        }
        // And whose it is: another API key has entries of its own, unless
        // the site's policy shares its pages.
        assert_ne!(plan_of(base.clone(), ApiKeyId::new()).key, plain);
        let shared = json!({ "url": "https://shared.example/a", "cache_ttl_s": 60 });
        assert_eq!(
            plan_of(shared.clone(), ApiKeyId::new()).key,
            plan_of(shared, key).key
        );
    }
}
