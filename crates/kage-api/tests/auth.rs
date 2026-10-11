//! Who may ask (spec §16.2, §19): a request without a key, with a wrong
//! one, with a disabled one, with one that lacks the scope, and with one
//! that is at a limit.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod common;

use axum::body::Body;
use axum::http::header::AUTHORIZATION;
use axum::http::{Method, Request, StatusCode};
use chrono::Utc;
use common::{Api, ROOMY};
use kage_core::domain::{KeyLimits, Scope};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use common::sched::lab::{LENIENT_FAST, article};

async fn with_header(api: &Api, value: &str) -> common::Reply {
    api.send(
        Request::builder()
            .method(Method::GET)
            .uri("/v1/policies/domains/example.org")
            .header(AUTHORIZATION, value)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn a_request_without_a_key_or_with_a_wrong_one_is_refused() {
    let mut api = Api::start(0, 149, LENIENT_FAST).await;
    let (id, token) = api.key_with(&[Scope::Fetch], ROOMY).await;

    // No header at all.
    let reply = api
        .call(Method::GET, "/v1/policies/domains/example.org", None, None)
        .await;
    reply.problem(StatusCode::UNAUTHORIZED, "missing_key");
    assert_eq!(
        reply.header("www-authenticate"),
        Some("Bearer realm=\"kage\"")
    );

    // Not a bearer token, not a token of ours, a token that does not
    // parse, a key that does not exist, and a secret that is wrong: one
    // answer for all, which does not say which it was.
    let unknown = token.replace(&id.as_uuid().simple().to_string(), &"0".repeat(32));
    let mut wrong_secret = token.clone();
    let last = wrong_secret.pop().unwrap();
    wrong_secret.push(if last == 'A' { 'B' } else { 'A' });
    let mut details = Vec::new();
    for value in [
        "Basic dXNlcjpwYXNz".to_owned(),
        "Bearer sk-live-something".to_owned(),
        "Bearer kage_nothing".to_owned(),
        format!("Bearer {unknown}"),
        format!("Bearer {wrong_secret}"),
        format!("Bearer {token} trailing"),
    ] {
        let reply = with_header(&api, &value).await;
        let problem = reply.problem(StatusCode::UNAUTHORIZED, "invalid_key");
        details.push(problem["detail"].clone());
        // Nothing of the token comes back.
        assert!(!String::from_utf8_lossy(&reply.body).contains("kage_"));
    }
    details.dedup();
    assert_eq!(details.len(), 1, "{details:?}");

    // The right one is let in; the scheme's case does not matter.
    for value in [format!("Bearer {token}"), format!("bearer {token}")] {
        let reply = with_header(&api, &value).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    }
    api.shut_down().await;
}

#[tokio::test]
async fn a_disabled_key_is_refused_and_only_its_holder_learns_why() {
    let mut api = Api::start(0, 149, LENIENT_FAST).await;
    let (id, token) = api.key_with(&[Scope::Fetch], ROOMY).await;
    let path = "/v1/policies/domains/example.org";
    assert_eq!(api.get(&token, path).await.status, StatusCode::OK);

    assert!(
        api.lab
            .store()
            .api_keys()
            .set_disabled(id, Some(Utc::now()))
            .await
            .unwrap()
    );

    api.get(&token, path)
        .await
        .problem(StatusCode::UNAUTHORIZED, "key_disabled");
    // Who does not hold the secret is told nothing about the key.
    let mut guess = token.clone();
    let last = guess.pop().unwrap();
    guess.push(if last == 'A' { 'B' } else { 'A' });
    api.get(&guess, path)
        .await
        .problem(StatusCode::UNAUTHORIZED, "invalid_key");
    api.shut_down().await;
}

#[tokio::test]
async fn every_endpoint_asks_for_its_own_scope() {
    let mut api = Api::start(0, 149, LENIENT_FAST).await;
    let fetch = api.key(&[Scope::Fetch]).await;
    let sessions = api.key(&[Scope::Sessions]).await;
    let admin = api.key(&[Scope::Admin]).await;
    let jobs = api.key(&[Scope::Jobs, Scope::Crawl]).await;
    let body = json!({ "url": "https://example.org/" });

    // A key without the scope is told which one is missing.
    for (token, method, at, scope) in [
        (&sessions, Method::POST, "/v1/fetch", "fetch"),
        (&admin, Method::POST, "/v1/fetch", "fetch"),
        (&jobs, Method::POST, "/v1/fetch", "fetch"),
        (&fetch, Method::POST, "/v1/sessions", "sessions"),
        (&admin, Method::POST, "/v1/sessions", "sessions"),
        (&fetch, Method::GET, "/v1/sessions/s1", "sessions"),
        (&fetch, Method::DELETE, "/v1/sessions/s1", "sessions"),
        (
            &sessions,
            Method::GET,
            "/v1/results/0190a0b0-0000-7000-8000-000000000001",
            "fetch",
        ),
        (&sessions, Method::GET, "/v1/artifacts/art_0", "fetch"),
        (&fetch, Method::GET, "/v1/admin/personas", "admin"),
        (&sessions, Method::GET, "/v1/admin/keys", "admin"),
        (&jobs, Method::POST, "/v1/admin/policies/reload", "admin"),
        (&fetch, Method::GET, "/v1/admin/audit", "admin"),
        (&fetch, Method::GET, "/v1/admin/pool", "admin"),
        (&fetch, Method::GET, "/metrics", "admin"),
        (
            &sessions,
            Method::GET,
            "/v1/policies/domains/example.org",
            "fetch",
        ),
        (
            &admin,
            Method::GET,
            "/v1/policies/domains/example.org",
            "fetch",
        ),
    ] {
        let sent = (method == Method::POST).then_some(&body);
        let reply = api.call(method.clone(), at, Some(token), sent).await;
        let problem = reply.problem(StatusCode::FORBIDDEN, "scope_missing");
        assert_eq!(problem["scope"], scope, "{method} {at}");
    }
    // `admin` implies no other scope, and no other scope implies it; a
    // session in a fetch needs both scopes.
    let reply = api
        .post(
            &fetch,
            "/v1/fetch",
            &json!({ "url": "https://example.org/", "session_id": "s1" }),
        )
        .await;
    assert_eq!(
        reply.problem(StatusCode::FORBIDDEN, "scope_missing")["scope"],
        "sessions"
    );

    // A refused admin call is in the audit trail, as denied.
    let trail = api.get(&admin, "/v1/admin/audit?limit=200").await.json();
    let denied: Vec<&str> = trail["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["outcome"] == "denied")
        .map(|event| event["action"].as_str().unwrap())
        .collect();
    for action in [
        "admin.persona.list",
        "admin.key.list",
        "admin.policy.reload",
        "admin.audit.list",
        "admin.pool.show",
    ] {
        assert!(denied.contains(&action), "{action} in {denied:?}");
    }

    // A key with the scope is let in.
    let reply = api.get(&fetch, "/v1/policies/domains/example.org").await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
    // What needs no key.
    for at in ["/healthz", "/openapi.json"] {
        let reply = api.call(Method::GET, at, None, None).await;
        assert_eq!(reply.status, StatusCode::OK, "{at}");
    }
    api.shut_down().await;
}

#[tokio::test]
async fn a_key_at_its_requests_per_minute_is_told_how_long_to_wait() {
    let mut api = Api::start(0, 149, LENIENT_FAST).await;
    let limits = KeyLimits {
        requests_per_minute: 3,
        ..ROOMY
    };
    let (_, limited) = api.key_with(&[Scope::Fetch], limits).await;
    let other = api.key(&[Scope::Fetch]).await;
    let path = "/v1/policies/domains/example.org";

    for _ in 0..3 {
        assert_eq!(api.get(&limited, path).await.status, StatusCode::OK);
    }
    let reply = api.get(&limited, path).await;
    let problem = reply.problem(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    assert_eq!(problem["limit"], "requests_per_minute");
    // A bucket of three a minute gives one back every twenty seconds.
    let wait: u64 = reply.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=20).contains(&wait), "{wait}");
    assert_eq!(problem["retry_after_s"], wait);

    // Another key is not held back by it.
    assert_eq!(api.get(&other, path).await.status, StatusCode::OK);
    api.shut_down().await;
}

#[tokio::test]
async fn the_api_as_a_whole_has_a_limit_that_counts_before_a_key_is_looked_at() {
    let mut api = Api::start_with(0, 149, LENIENT_FAST, |config| {
        config.server.requests_per_minute = 4;
    })
    .await;
    let token = api.key(&[Scope::Fetch]).await;
    let path = "/v1/policies/domains/example.org";

    // Requests without a valid key count too: guessing keys is limited.
    for _ in 0..2 {
        api.get("kage_nothing", path)
            .await
            .problem(StatusCode::UNAUTHORIZED, "invalid_key");
        assert_eq!(api.get(&token, path).await.status, StatusCode::OK);
    }
    let reply = api.get(&token, path).await;
    let problem = reply.problem(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    assert_eq!(problem["limit"], "server.requests_per_minute");
    assert!(reply.header("retry-after").is_some());
    // Liveness is not behind it.
    let reply = api.call(Method::GET, "/healthz", None, None).await;
    assert_eq!(reply.status, StatusCode::OK);
    api.shut_down().await;
}

#[tokio::test]
async fn a_key_has_so_many_requests_in_flight_and_so_many_pages_a_day() {
    let mut api = Api::start(2, 149, LENIENT_FAST).await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(article("Slow"))
                .set_delay(std::time::Duration::from_millis(1500)),
        )
        .mount(&api.lab.site)
        .await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(article("A page")),
        )
        .mount(&api.lab.site)
        .await;

    // One request in flight at a time.
    let limits = KeyLimits {
        max_concurrent: 1,
        ..ROOMY
    };
    let (_, single) = api.key_with(&[Scope::Fetch], limits).await;
    let slow = {
        let router = api.router.clone();
        let sent = common::request(
            Method::POST,
            "/v1/fetch",
            Some(&single),
            Some(&json!({ "url": Api::url("/slow"), "render": "http" })),
        );
        tokio::spawn(async move { common::ask(&router, sent).await.status })
    };
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let reply = api.get(&single, "/v1/policies/domains/example.org").await;
    let problem = reply.problem(StatusCode::TOO_MANY_REQUESTS, "concurrency_limited");
    assert_eq!(problem["limit"], "max_concurrent");
    assert_eq!(reply.header("retry-after"), Some("1"));
    assert_eq!(slow.await.unwrap(), StatusCode::OK);
    // Its answer was written: the key may ask again.
    let reply = api.get(&single, "/v1/policies/domains/example.org").await;
    assert_eq!(reply.status, StatusCode::OK);

    // Two pages a day. A page counts when its fetch starts, whatever
    // comes of it; what is not a fetch does not count.
    let limits = KeyLimits {
        daily_page_budget: Some(2),
        ..ROOMY
    };
    let (budgeted, token) = api.key_with(&[Scope::Fetch], limits).await;
    let page = json!({ "url": Api::url("/page"), "render": "http" });
    for _ in 0..2 {
        let reply = api.post(&token, "/v1/fetch", &page).await;
        assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
        assert_eq!(reply.json()["outcome"], "ok");
    }
    let reply = api.post(&token, "/v1/fetch", &page).await;
    let problem = reply.problem(StatusCode::TOO_MANY_REQUESTS, "budget_exhausted");
    assert_eq!(problem["limit"], "daily_page_budget");
    let wait: u64 = reply.header("retry-after").unwrap().parse().unwrap();
    assert!((1..=86_400).contains(&wait), "{wait}");
    // A request that is refused before it is a fetch is no page.
    api.post(&token, "/v1/fetch", &json!({ "url": "ftp://example.org/" }))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid_url");
    assert_eq!(
        api.get(&token, "/v1/policies/domains/example.org")
            .await
            .status,
        StatusCode::OK
    );
    // The count is the store's: a server that starts again knows it.
    let since = Utc::now() - chrono::Duration::hours(1);
    assert_eq!(
        api.lab
            .store()
            .requests()
            .count_for_key_since(budgeted, since)
            .await
            .unwrap(),
        2
    );
    let again = api.restarted();
    let sent = common::request(Method::POST, "/v1/fetch", Some(&token), Some(&page));
    common::ask(&again, sent)
        .await
        .problem(StatusCode::TOO_MANY_REQUESTS, "budget_exhausted");
    api.shut_down().await;
}
