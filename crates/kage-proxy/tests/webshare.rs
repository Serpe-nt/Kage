//! The Webshare client against a mock of the Webshare API: list paging, the
//! replacement lifecycle, the replaced list, the account, the stats, and how
//! the client behaves when the API rate limits, fails or lies.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::TcpListener;
use std::time::{Duration, Instant};

use chrono::Utc;
use common::{MockWebshare, PROXY_PASS, PROXY_USER, WEBSHARE_TOKEN};
use kage_core::config::WebshareConfig;
use kage_core::domain::{Proxy, ProxyProviderKind, ProxyScheme, ProxyState};
use kage_core::duration::HumanDuration;
use kage_core::ids::ProxyId;
use kage_core::secret::{SealedSecret, Secret};
use kage_proxy::{
    ProviderError, ProxyProvider, ReplaceSpec, ReplaceState, ReplaceTicket, WebshareProvider,
};
use serde_json::json;

fn config(mock: &MockWebshare) -> WebshareConfig {
    WebshareConfig {
        api_key: Some(Secret::new(WEBSHARE_TOKEN)),
        base_url: mock.base_url(),
        page_size: 10,
        timeout: HumanDuration::from_secs(5),
        rate_limit_backoff: HumanDuration::new(Duration::from_millis(50)),
        max_retries: 2,
        ..WebshareConfig::default()
    }
}

fn provider(mock: &MockWebshare) -> WebshareProvider {
    WebshareProvider::new(&config(mock)).unwrap()
}

/// A stored proxy at this address; the provider only reads the address.
fn stored(ip: &str) -> Proxy {
    Proxy {
        id: ProxyId::new(),
        provider: ProxyProviderKind::Webshare,
        provider_ref: "d-1".to_owned(),
        host: ip.to_owned(),
        port: 8000,
        username: SealedSecret::new(vec![1]),
        password: SealedSecret::new(vec![2]),
        scheme: ProxyScheme::Http,
        country: "DE".parse().ok(),
        locale: None,
        timezone: None,
        city: None,
        asn: None,
        state: ProxyState::Backoff,
        consecutive_failures: 3,
        backoff_count: 3,
        backoff_until: None,
        failing_since: None,
        replacement: None,
        last_health: None,
        created_at: Utc::now(),
        retired_at: None,
    }
}

fn germany() -> ReplaceSpec {
    ReplaceSpec {
        country: "DE".parse().ok(),
    }
}

#[tokio::test]
async fn the_list_is_read_page_by_page_in_direct_mode() {
    let mock = MockWebshare::start().await;
    let ips = mock.with(|state| {
        let mut ips = state.add_proxies(20, "DE");
        ips.extend(state.add_proxies(5, "AT"));
        ips
    });
    let provider = provider(&mock);
    assert_eq!(provider.kind(), ProxyProviderKind::Webshare);

    let listed = provider.list().await.unwrap();

    assert_eq!(listed.len(), 25);
    assert_eq!(
        listed.iter().map(|proxy| &proxy.host).collect::<Vec<_>>(),
        ips.iter().collect::<Vec<_>>()
    );
    let first = &listed[0];
    assert_eq!(first.provider_ref, "d-10001");
    assert_eq!(first.port, 8001);
    assert_eq!(first.username.expose(), PROXY_USER);
    assert_eq!(first.password.expose(), PROXY_PASS);
    assert_eq!(first.country.unwrap().as_str(), "DE");
    assert_eq!(first.city.as_deref(), Some("Testville"));
    assert_eq!(first.valid, Some(true));
    assert_eq!(listed[24].country.unwrap().as_str(), "AT");

    // Three pages of ten, each asked for in direct mode, none twice.
    let queries: Vec<String> = mock.with(|state| {
        state
            .seen
            .iter()
            .map(|(method, path, query)| {
                assert_eq!(method, "GET");
                assert_eq!(path, "/api/v2/proxy/list/");
                query.clone()
            })
            .collect()
    });
    assert_eq!(
        queries,
        [
            "mode=direct&page=1&page_size=10",
            "mode=direct&page=2&page_size=10",
            "mode=direct&page=3&page_size=10",
        ]
    );
    // Every request carries the token and says honestly who is asking.
    for request in mock.requests().await {
        assert_eq!(
            request.headers["authorization"],
            format!("Token {WEBSHARE_TOKEN}")
        );
        assert_eq!(
            request.headers["user-agent"],
            format!("kage/{}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(request.headers["accept"], "application/json");
    }
}

#[tokio::test]
async fn an_empty_list_is_one_request_and_a_configured_plan_is_named() {
    let mock = MockWebshare::start().await;
    let with_plan = WebshareProvider::new(&WebshareConfig {
        plan_id: Some(4242),
        ..config(&mock)
    })
    .unwrap();

    assert_eq!(with_plan.list().await.unwrap().len(), 0);

    mock.with(|state| {
        assert_eq!(state.seen.len(), 1);
        assert_eq!(
            state.seen[0].2,
            "mode=direct&page=1&page_size=10&plan_id=4242"
        );
    });

    // A plan the account does not have is the provider's error to report.
    let wrong_plan = WebshareProvider::new(&WebshareConfig {
        plan_id: Some(1),
        ..config(&mock)
    })
    .unwrap();
    assert_eq!(
        wrong_plan.list().await.unwrap_err(),
        ProviderError::Status {
            status: 400,
            code: Some("invalid".to_owned()),
            message: "Unknown plan".to_owned(),
        }
    );
}

#[tokio::test]
async fn a_replacement_runs_from_request_to_done() {
    let mock = MockWebshare::start().await;
    let ips = mock.with(|state| state.add_proxies(3, "DE"));
    let provider = provider(&mock);
    let before = Utc::now() - chrono::Duration::minutes(1);

    let ticket = provider.replace(&stored(&ips[1]), germany()).await.unwrap();

    assert_eq!(ticket, ReplaceTicket("98315".to_owned()));
    // The request names the one address and asks for the same country.
    let posted = mock
        .requests()
        .await
        .into_iter()
        .find(|request| request.method.as_str() == "POST")
        .unwrap();
    assert_eq!(posted.url.path(), "/api/v3/proxy/replace/");
    assert_eq!(posted.headers["content-type"], "application/json");
    assert_eq!(
        posted.body_json::<serde_json::Value>().unwrap(),
        json!({
            "to_replace": { "type": "ip_address", "ip_addresses": [ips[1]] },
            "replace_with": [{ "type": "country", "country_code": "DE", "count": 1 }],
            "dry_run": false,
        })
    );

    // Two reads while the provider works, then it is done.
    assert_eq!(
        provider.replace_status(&ticket).await.unwrap(),
        ReplaceState::Pending
    );
    assert_eq!(
        provider.replace_status(&ticket).await.unwrap(),
        ReplaceState::Pending
    );
    assert_eq!(
        provider.replace_status(&ticket).await.unwrap(),
        ReplaceState::Done {
            removed: 1,
            added: 1
        }
    );
    // Asking again changes nothing.
    assert_eq!(
        provider.replace_status(&ticket).await.unwrap(),
        ReplaceState::Done {
            removed: 1,
            added: 1
        }
    );
    mock.with(|state| {
        assert_eq!(state.count(Some("GET"), "/api/v3/proxy/replace/98315/"), 4);
        assert_eq!(state.replacements_used, 1);
    });

    // The list has the new address instead of the old one.
    let listed = provider.list().await.unwrap();
    assert_eq!(listed.len(), 3);
    assert!(listed.iter().all(|proxy| proxy.host != ips[1]));
    let new = listed
        .iter()
        .find(|proxy| !ips.contains(&proxy.host))
        .unwrap();

    // And the provider's log says which replaced which.
    let replaced = provider.replaced(before).await.unwrap();
    assert_eq!(replaced.len(), 1);
    assert_eq!(replaced[0].reason, "proxy_replaced");
    assert_eq!(replaced[0].old_host, ips[1]);
    assert_eq!(replaced[0].old_port, Some(8002));
    assert_eq!(replaced[0].new_host.as_deref(), Some(new.host.as_str()));
    assert_eq!(replaced[0].new_port, Some(new.port));
    assert_eq!(replaced[0].new_country.unwrap().as_str(), "DE");
    assert!(replaced[0].at >= before);
    // Nothing that old is of interest to a later reader.
    assert_eq!(
        provider
            .replaced(Utc::now() + chrono::Duration::minutes(1))
            .await
            .unwrap(),
        []
    );
}

#[tokio::test]
async fn without_a_country_any_address_is_asked_for() {
    let mock = MockWebshare::start().await;
    let ips = mock.with(|state| state.add_proxies(1, "DE"));

    provider(&mock)
        .replace(&stored(&ips[0]), ReplaceSpec::default())
        .await
        .unwrap();

    let posted = mock
        .requests()
        .await
        .into_iter()
        .find(|request| request.method.as_str() == "POST")
        .unwrap();
    assert_eq!(
        posted.body_json::<serde_json::Value>().unwrap()["replace_with"],
        json!([{ "type": "any", "count": 1 }])
    );
}

#[tokio::test]
async fn the_provider_refuses_without_quota_and_while_another_replacement_runs() {
    let mock = MockWebshare::start().await;
    let ips = mock.with(|state| state.add_proxies(3, "DE"));
    let provider = provider(&mock);

    // One at a time.
    provider.replace(&stored(&ips[0]), germany()).await.unwrap();
    assert_eq!(
        provider
            .replace(&stored(&ips[1]), germany())
            .await
            .unwrap_err(),
        ProviderError::Busy
    );

    // Nothing left in the subscription.
    mock.with(|state| {
        state.replacements.clear();
        state.replacements_used = state.replacements_total;
    });
    assert_eq!(
        provider
            .replace(&stored(&ips[1]), germany())
            .await
            .unwrap_err(),
        ProviderError::QuotaExhausted
    );

    // Any other refusal comes back with the provider's own words.
    mock.with(|state| state.replacements_used = 0);
    assert_eq!(
        provider
            .replace(&stored("203.0.113.99"), germany())
            .await
            .unwrap_err(),
        ProviderError::Status {
            status: 400,
            code: Some("invalid".to_owned()),
            message: "No proxies match".to_owned(),
        }
    );
    // None of the refusals was sent twice.
    mock.with(|state| assert_eq!(state.count(Some("POST"), "/api/v3/proxy/replace/"), 4));
}

#[tokio::test]
async fn a_replacement_the_provider_fails_reports_its_reason() {
    let mock = MockWebshare::start().await;
    let ips = mock.with(|state| {
        state.polls_to_complete = 0;
        state.fail_replacement = Some((
            "no_proxies_available".to_owned(),
            "No proxies available in DE".to_owned(),
        ));
        state.add_proxies(2, "DE")
    });
    let provider = provider(&mock);

    let ticket = provider.replace(&stored(&ips[0]), germany()).await.unwrap();

    assert_eq!(
        provider.replace_status(&ticket).await.unwrap(),
        ReplaceState::Failed {
            code: Some("no_proxies_available".to_owned()),
            message: "No proxies available in DE".to_owned(),
        }
    );
    // Nothing was replaced.
    assert_eq!(provider.list().await.unwrap().len(), 2);
    mock.with(|state| assert_eq!(state.replacements_used, 0));

    // A ticket the provider does not know, and one that is no id at all.
    assert!(matches!(
        provider
            .replace_status(&ReplaceTicket("1".to_owned()))
            .await
            .unwrap_err(),
        ProviderError::Status { status: 404, .. }
    ));
    for ticket in ["", "../../v2/subscription", "12/"] {
        assert!(matches!(
            provider
                .replace_status(&ReplaceTicket(ticket.to_owned()))
                .await
                .unwrap_err(),
            ProviderError::Decode(_)
        ));
    }
}

#[tokio::test]
async fn the_account_combines_subscription_plan_settings_and_stock() {
    let mock = MockWebshare::start().await;
    mock.with(|state| {
        state.add_proxies(18, "DE");
        state.add_proxies(2, "AT");
        state.replacements_used = 3;
        state.auto_replace = [true, false, true, false];
        state.throttled = true;
    });
    let provider = provider(&mock);

    let account = provider.account().await.unwrap();

    assert_eq!(account.plan_id.as_deref(), Some("4242"));
    assert_eq!(account.plan_status.as_deref(), Some("active"));
    assert_eq!(account.proxy_type.as_deref(), Some("dedicated/isp"));
    assert_eq!(account.proxy_count, Some(20));
    assert_eq!(account.countries["DE"], 18);
    assert_eq!(account.countries["AT"], 2);
    assert_eq!(account.bandwidth_limit_gb, Some(0.0));
    assert_eq!(account.replacements_total, Some(10));
    assert_eq!(account.replacements_used, Some(3));
    assert_eq!(account.replacements_available, Some(7));
    assert_eq!(
        account.term_ends_at.unwrap().to_rfc3339(),
        "2026-10-31T18:19:14.489461+00:00"
    );
    assert_eq!(account.renewals_enabled, Some(true));
    assert_eq!(account.paused, Some(false));
    assert_eq!(account.throttled, Some(true));
    let auto = account.auto_replace.unwrap();
    assert_eq!(
        auto.enabled(),
        [
            "auto_replace_invalid_proxies",
            "auto_replace_out_of_rotation_proxies"
        ]
    );
    assert_eq!(account.available_countries.as_ref().unwrap()["DE"], 40);

    // Four reads, the two v3 ones for the plan the subscription named.
    mock.with(|state| {
        let seen: Vec<(&str, &str)> = state
            .seen
            .iter()
            .map(|(_, path, query)| (path.as_str(), query.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("/api/v2/subscription/", ""),
                ("/api/v2/subscription/plan/4242/", ""),
                ("/api/v3/proxy/config", "plan_id=4242"),
                ("/api/v3/proxy/list/stats", "plan_id=4242"),
            ]
        );
    });
    // Reading an account changes nothing in it.
    assert!(
        mock.requests()
            .await
            .iter()
            .all(|request| request.method.as_str() == "GET")
    );
}

#[tokio::test]
async fn the_account_is_still_reported_when_its_details_cannot_be_read() {
    let mock = MockWebshare::start().await;
    mock.with(|state| {
        state.add_proxies(4, "DE");
    });
    // Both v3 reads fail through every retry.
    mock.inject(
        None,
        "/api/v3/proxy/config",
        500,
        json!({ "detail": "boom" }),
        3,
    );
    mock.inject(
        None,
        "/api/v3/proxy/list/stats",
        404,
        json!({ "detail": "Not found." }),
        1,
    );

    let account = provider(&mock).account().await.unwrap();

    assert_eq!(account.proxy_count, Some(4));
    assert_eq!(account.replacements_available, Some(10));
    // Unknown, not "off" and not "none in stock".
    assert_eq!(account.auto_replace, None);
    assert_eq!(account.available_countries, None);
}

#[tokio::test]
async fn hourly_stats_are_read_for_a_window_without_projections() {
    let mock = MockWebshare::start().await;
    mock.with(|state| {
        state.stats = vec![
            json!({
                "timestamp": "2026-10-08T10:00:00-07:00", "is_projected": false,
                "bandwidth_total": 5000, "bandwidth_average": 1000, "requests_total": 5,
                "requests_successful": 4, "requests_failed": 1,
                "error_reasons": [{ "reason": "client_connect_forbidden_host", "type": "configuration",
                                    "how_to_fix": "…", "http_status": 403, "count": 1 }],
                "countries_used": { "DE": 5 }, "number_of_proxies_used": 2,
                "protocols_used": { "http": 5 }, "average_concurrency": 0.0001,
                "average_rps": 0.0002, "last_request_sent_at": "2026-10-08T10:12:32-07:00"
            }),
            json!({
                "timestamp": "2026-10-09T10:00:00-07:00", "is_projected": true,
                "bandwidth_total": 9999, "requests_total": 9, "requests_successful": 9,
                "requests_failed": 0, "error_reasons": [], "number_of_proxies_used": 20
            }),
        ];
    });
    let provider = provider(&mock);

    let stats = provider
        .stats(Utc::now() - chrono::Duration::days(2))
        .await
        .unwrap();

    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].hour.to_rfc3339(), "2026-10-08T17:00:00+00:00");
    assert_eq!(stats[0].requests_total, 5);
    assert_eq!(stats[0].requests_successful, 4);
    assert_eq!(stats[0].requests_failed, 1);
    assert_eq!(stats[0].bytes, 5000);
    assert_eq!(stats[0].proxies_used, 2);
    assert_eq!(stats[0].errors["client_connect_forbidden_host"], 1);

    // A start further back than Webshare keeps is moved up, not refused.
    provider
        .stats(Utc::now() - chrono::Duration::days(400))
        .await
        .unwrap();
    mock.with(|state| {
        let query = &state.seen.last().unwrap().2;
        let since = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("timestamp__gte="))
            .unwrap()
            .replace("%3A", ":");
        let since: chrono::DateTime<Utc> = since.parse().unwrap();
        let age = Utc::now() - since;
        assert!(age < chrono::Duration::days(90), "{age}");
        assert!(age > chrono::Duration::days(89), "{age}");
        assert!(query.contains("timestamp__lte="), "{query}");
    });
}

#[tokio::test]
async fn a_429_is_waited_out_and_then_the_request_goes_through() {
    let mock = MockWebshare::start().await;
    mock.with(|state| {
        state.add_proxies(3, "DE");
    });
    // The server names the wait once, and gives none the second time.
    mock.rate_limit("/api/v2/proxy/list/", Some(1), 1);
    mock.rate_limit("/api/v2/proxy/list/", None, 1);
    let provider = provider(&mock);
    let started = Instant::now();

    let listed = provider.list().await.unwrap();

    assert_eq!(listed.len(), 3);
    // One second as told, then the configured 50 ms.
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(1050), "{waited:?}");
    assert!(waited < Duration::from_secs(5), "{waited:?}");
    mock.with(|state| assert_eq!(state.count(None, "/api/v2/proxy/list/"), 3));
}

#[tokio::test]
async fn rate_limiting_that_does_not_end_is_reported_after_the_retries() {
    let mock = MockWebshare::start().await;
    mock.rate_limit("/api/v2/subscription/", None, 10);
    let provider = provider(&mock);

    let error = provider.account().await.unwrap_err();

    assert_eq!(
        error,
        ProviderError::RateLimited {
            retry_after: Duration::from_millis(50)
        }
    );
    assert!(error.is_transient());
    // The first try and two more.
    mock.with(|state| assert_eq!(state.count(None, "/api/v2/subscription/"), 3));
}

#[tokio::test]
async fn a_rate_limited_write_is_retried_because_it_was_never_applied() {
    let mock = MockWebshare::start().await;
    let ips = mock.with(|state| state.add_proxies(2, "DE"));
    mock.rate_limit("/api/v3/proxy/replace/", None, 1);

    let ticket = provider(&mock)
        .replace(&stored(&ips[0]), germany())
        .await
        .unwrap();

    assert_eq!(ticket.0, "98315");
    mock.with(|state| {
        assert_eq!(state.count(Some("POST"), "/api/v3/proxy/replace/"), 2);
        assert_eq!(state.replacements.len(), 1);
    });
}

#[tokio::test]
async fn a_failing_read_is_retried_and_a_failing_write_is_never_sent_twice() {
    let mock = MockWebshare::start().await;
    let ips = mock.with(|state| state.add_proxies(2, "DE"));
    let provider = provider(&mock);

    // Two server errors, then the list.
    mock.inject(
        None,
        "/api/v2/proxy/list/",
        502,
        json!({ "detail": "bad gateway" }),
        2,
    );
    assert_eq!(provider.list().await.unwrap().len(), 2);
    mock.with(|state| assert_eq!(state.count(None, "/api/v2/proxy/list/"), 3));

    // A read that keeps failing gives the last error.
    mock.inject(
        None,
        "/api/v2/proxy/list/",
        503,
        json!({ "detail": "maintenance" }),
        3,
    );
    assert_eq!(
        provider.list().await.unwrap_err(),
        ProviderError::Status {
            status: 503,
            code: None,
            message: "maintenance".to_owned(),
        }
    );

    // A write that fails on the server may have been applied: the outcome
    // is unknown and it is not sent again.
    mock.inject(
        Some("POST"),
        "/api/v3/proxy/replace/",
        502,
        json!({ "detail": "bad gateway" }),
        1,
    );
    let error = provider
        .replace(&stored(&ips[0]), germany())
        .await
        .unwrap_err();
    assert!(matches!(&error, ProviderError::OutcomeUnknown(detail) if detail.contains("502")));
    assert!(!error.is_transient());
    mock.with(|state| {
        assert_eq!(state.count(Some("POST"), "/api/v3/proxy/replace/"), 1);
        assert_eq!(state.replacements.len(), 0);
    });
}

#[tokio::test]
async fn a_refused_key_is_reported_and_not_retried() {
    let mock = MockWebshare::start().await;
    let provider = WebshareProvider::new(&WebshareConfig {
        api_key: Some(Secret::new("wrong-token")),
        ..config(&mock)
    })
    .unwrap();

    let error = provider.list().await.unwrap_err();

    assert_eq!(error, ProviderError::Unauthorized { status: 401 });
    assert!(!error.to_string().contains("wrong-token"));
    mock.with(|state| assert_eq!(state.seen.len(), 1));
}

#[tokio::test]
async fn answers_the_documentation_does_not_describe_are_decode_errors() {
    let mock = MockWebshare::start().await;
    let provider = provider(&mock);

    mock.inject(
        None,
        "/api/v2/proxy/list/",
        200,
        json!({ "results": "nope" }),
        1,
    );
    let error = provider.list().await.unwrap_err();
    assert!(matches!(&error, ProviderError::Decode(detail) if detail.contains("v2/proxy/list/")));

    // A rotating residential plan lists proxies without an address.
    mock.inject(
        None,
        "/api/v2/proxy/list/",
        200,
        json!({ "count": 1, "next": null, "previous": null, "results": [{
            "id": "r-1", "username": "u", "password": "p", "proxy_address": null, "port": 80,
            "valid": true, "country_code": "DE", "city_name": "Berlin"
        }] }),
        1,
    );
    let error = provider.list().await.unwrap_err();
    assert!(error.to_string().contains("direct mode"), "{error}");

    // A list that never ends is cut off. This client may ask as fast as it
    // likes, so that two hundred pages do not take minutes.
    let provider = WebshareProvider::new(&WebshareConfig {
        list_requests_per_minute: 60_000,
        ..config(&mock)
    })
    .unwrap();
    mock.inject(
        None,
        "/api/v2/proxy/list/",
        200,
        json!({ "count": 1_000_000, "next": "more", "previous": null, "results": [{
            "id": "d-1", "username": "u", "password": "p", "proxy_address": "198.51.100.1",
            "port": 80
        }] }),
        250,
    );
    let error = provider.list().await.unwrap_err();
    assert!(error.to_string().contains("did not end"), "{error}");
}

#[tokio::test]
async fn an_api_that_cannot_be_reached_is_a_transport_error() {
    // A loopback port that was free a moment ago: nothing listens there.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let provider = WebshareProvider::new(&WebshareConfig {
        api_key: Some(Secret::new(WEBSHARE_TOKEN)),
        base_url: format!("http://127.0.0.1:{port}/api/"),
        max_retries: 1,
        timeout: HumanDuration::from_secs(2),
        ..WebshareConfig::default()
    })
    .unwrap();

    let error = provider.list().await.unwrap_err();
    assert!(matches!(&error, ProviderError::Transport(_)), "{error}");
    assert!(error.is_transient());

    // A write that never left the host is safe to try again later.
    let error = provider
        .replace(&stored("198.51.100.1"), germany())
        .await
        .unwrap_err();
    assert!(matches!(&error, ProviderError::Transport(_)), "{error}");
}

#[tokio::test]
async fn a_client_needs_a_key_it_can_send() {
    let mock = MockWebshare::start().await;

    let missing = WebshareProvider::new(&WebshareConfig {
        api_key: None,
        ..config(&mock)
    })
    .unwrap_err();
    assert!(
        matches!(&missing, ProviderError::Setup(detail) if detail.contains("KAGE__WEBSHARE__API_KEY"))
    );

    let unsendable = WebshareProvider::new(&WebshareConfig {
        api_key: Some(Secret::new("line\nbreak-51aa")),
        ..config(&mock)
    })
    .unwrap_err();
    assert!(matches!(&unsendable, ProviderError::Setup(_)));
    assert!(!unsendable.to_string().contains("51aa"));

    // The client never prints its key.
    let printed = format!("{:?}", provider(&mock));
    assert!(printed.contains("WebshareProvider"), "{printed}");
    assert!(!printed.contains(WEBSHARE_TOKEN), "{printed}");
}
