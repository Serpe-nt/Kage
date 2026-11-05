//! What the API answers at its edges (QA round one, F112 to F136): bodies
//! read by one set of rules on every endpoint, refusals that name their
//! member, heads and paths that are problems, keys that read and cancel
//! jobs, the audit trail, the policy preview, and a stopping server.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::fmt::Write as _;
use std::time::Duration;

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{Method, Request, StatusCode};
use common::sched::lab::{FAST, article};
use common::{Api, Reply, read_all};
use kage_core::config::Config;
use kage_core::domain::Scope;
use kage_core::duration::HumanDuration;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, ResponseTemplate};

/// A worker that looks often.
fn quick(config: &mut Config) {
    config.jobs.poll_interval = HumanDuration::new(Duration::from_millis(50));
    config.jobs.lease = HumanDuration::new(Duration::from_secs(3));
}

/// A request with a body of this media type, sent as it is.
fn raw(path: &str, token: &str, content_type: Option<&str>, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(AUTHORIZATION, format!("Bearer {token}"));
    if let Some(content_type) = content_type {
        request = request.header(CONTENT_TYPE, content_type);
    }
    request.body(Body::from(body.to_owned())).unwrap()
}

/// The member a problem names.
#[track_caller]
fn field(reply: &Reply) -> String {
    let problem = reply.problem(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed");
    problem["field"]
        .as_str()
        .unwrap_or_else(|| panic!("no field in {problem}"))
        .to_owned()
}

/// The status line's code and the JSON body of a raw HTTP/1.1 answer, and
/// whether it carries a request id.
fn parsed(answer: &str) -> (u16, Option<Value>, bool) {
    let status = answer
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {answer:?}"));
    let body = answer
        .split_once("\r\n\r\n")
        .and_then(|(_, body)| body.find('{').map(|at| &body[at..]))
        .and_then(|body| body.rfind('}').map(|at| &body[..=at]))
        .and_then(|body| serde_json::from_str(body).ok());
    let identified = answer.to_ascii_lowercase().contains("x-request-id:");
    (status, body, identified)
}

async fn audit(api: &Api, admin: &str) -> Vec<Value> {
    let reply = api.get(admin, "/v1/admin/audit?limit=200").await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.json()["events"].as_array().cloned().unwrap()
}

#[tokio::test]
async fn a_nul_is_refused_naming_the_member_and_nothing_is_kept() {
    // F112 (api-02, secrecy-10): before, a 500 with the body in Postgres'
    // log.
    let mut api = Api::start(1, 149, FAST).await;
    let token = api
        .key(&[
            Scope::Fetch,
            Scope::Jobs,
            Scope::Crawl,
            Scope::Sessions,
            Scope::Admin,
        ])
        .await;
    let page = Api::url("/k");
    let nul = |text: &str| format!("{text}\u{0}x");
    for (at, body, member) in [
        ("/v1/fetch", json!({ "url": nul(&page), "level": 0 }), "url"),
        (
            "/v1/fetch",
            json!({ "url": page, "level": 0, "method": "POST", "body": nul("password=a"),
                    "headers": { "Content-Type": "text/plain" } }),
            "body",
        ),
        (
            "/v1/jobs",
            json!({ "type": "batch", "urls": [page, nul(&page)], "defaults": { "level": 0 } }),
            "urls[1]",
        ),
        (
            "/v1/jobs",
            json!({ "type": "flow", "level": 2, "actions": [
                { "action": "goto", "url": page },
                { "action": "type", "selector": "input", "text": nul("secret") },
            ] }),
            "actions[1].text",
        ),
        (
            "/v1/jobs",
            json!({ "type": "crawl", "seeds": [nul(&page)], "defaults": { "level": 0 } }),
            "seeds[0]",
        ),
        (
            "/v1/sessions",
            json!({ "id": "s1", "domain": nul("example.org") }),
            "domain",
        ),
        (
            "/v1/admin/keys",
            json!({ "name": nul("k"), "scopes": ["fetch"] }),
            "name",
        ),
    ] {
        let reply = api.post(&token, at, &body).await;
        assert_eq!(field(&reply), member, "{at} {body}");
        assert!(!String::from_utf8_lossy(&reply.body).contains("secret"));
    }
    let requests: i64 = sqlx::query_scalar("SELECT count(*) FROM requests")
        .fetch_one(api.lab.store().pool())
        .await
        .unwrap();
    assert_eq!(requests, 0);
    api.shut_down().await;
}

#[tokio::test]
async fn a_refusal_while_reading_names_its_member_its_step_and_its_item() {
    // F115 (api-06, api-07, jobs-08).
    let mut api = Api::start(1, 149, FAST).await;
    let token = api
        .key(&[Scope::Fetch, Scope::Jobs, Scope::Sessions, Scope::Admin])
        .await;
    let page = Api::url("/k");

    // A null or a number where a name of an enumeration belongs.
    for (extra, member) in [
        (json!({ "robots": null }), "robots"),
        (json!({ "robots": 5 }), "robots"),
        (json!({ "request_kind": null }), "request_kind"),
        (json!({ "wait": { "until": null } }), "wait.until"),
        (json!({ "wait": { "until": 5 } }), "wait.until"),
        (
            json!({ "include": ["screenshot"], "screenshot": { "format": 5 } }),
            "screenshot.format",
        ),
        (json!({ "behavior": { "zzq": 1 } }), "behavior.zzq"),
    ] {
        let mut body = json!({ "url": page, "level": 2 });
        for (name, value) in extra.as_object().unwrap() {
            body[name] = value.clone();
        }
        let reply = api.post(&token, "/v1/fetch", &body).await;
        assert_eq!(field(&reply), member, "{body}");
    }
    let reply = api
        .post(
            &token,
            "/v1/sessions",
            &json!({ "persona": { "engines": [5] } }),
        )
        .await;
    assert_eq!(field(&reply), "persona.engines[0]");
    let reply = api
        .post(
            &token,
            "/v1/admin/keys",
            &json!({ "name": "x", "scopes": [5] }),
        )
        .await;
    assert_eq!(field(&reply), "scopes[0]");

    // A job: the member, and where in the job it was written.
    let (api_ref, token_ref) = (&api, &token);
    let job = |body: Value| async move { api_ref.post(token_ref, "/v1/jobs", &body).await };
    let reply = job(json!({ "type": "batch", "items": [
        { "url": page, "level": 0 },
        { "url": page, "level": 0, "timeout_ms": "x" },
    ] }))
    .await;
    assert_eq!(field(&reply), "items[1].timeout_ms");
    assert_eq!(reply.json()["item"], 1);
    let reply = job(json!({ "type": "batch", "urls": [page],
                           "defaults": { "level": 0, "timeout_ms": "x" } }))
    .await;
    assert_eq!(field(&reply), "defaults.timeout_ms");
    assert_eq!(reply.json()["item"], 0);
    let reply = job(
        json!({ "type": "batch", "urls": [page], "defaults": { "level": 0 },
                           "visitors": "x" }),
    )
    .await;
    assert_eq!(field(&reply), "visitors");
    let reply = job(
        json!({ "type": "batch", "urls": [page], "defaults": { "level": 0 },
                           "webhook": { "url": 5, "secret": "a secret of sixteen bytes" } }),
    )
    .await;
    assert_eq!(field(&reply), "webhook.url");
    let reply = job(json!({ "type": 5, "urls": [page] })).await;
    assert_eq!(field(&reply), "type");
    let reply = job(json!({ "type": "zzq", "urls": [page] })).await;
    assert_eq!(field(&reply), "type");
    // A flow's step: its member and its place.
    let flow = |step: Value| {
        json!({ "type": "flow", "level": 3, "actions": [
            { "action": "goto", "url": page },
            { "action": "goto", "url": page },
            step,
        ] })
    };
    for (step, member) in [
        (json!({ "action": "click" }), "actions[2]"),
        (
            json!({ "action": "click", "selector": "a", "timeout_ms": "x" }),
            "actions[2].timeout_ms",
        ),
        (
            json!({ "action": "type", "selector": "a", "text": 5 }),
            "actions[2].text",
        ),
        (
            json!({ "action": "click", "selector": "a", "zzq": 1 }),
            "actions[2].zzq",
        ),
        (json!({ "action": "zzq" }), "actions[2].action"),
        (json!({ "selector": "a" }), "actions[2].action"),
    ] {
        let reply = job(flow(step.clone())).await;
        assert_eq!(field(&reply), member, "{step}");
        assert_eq!(reply.json()["step"], 2, "{step}");
    }
    let missing = job(flow(json!({ "action": "click" }))).await.json();
    assert!(
        missing["detail"].as_str().unwrap().contains("selector"),
        "{missing}"
    );
    api.shut_down().await;
}

#[tokio::test]
async fn a_body_is_json_once_and_says_its_length_on_every_endpoint() {
    // F134 (api-13, api-14, api-15).
    let mut api = Api::start_with(1, 149, FAST, |config| {
        config.server.max_body_bytes = 4096;
    })
    .await;
    let token = api.key(&[Scope::Fetch, Scope::Jobs, Scope::Admin]).await;
    let page = Api::url("/k");

    // A member given twice.
    for (at, body, member) in [
        (
            "/v1/jobs",
            format!(
                r#"{{"type":"batch","urls":["{page}?first"],"urls":["{page}?second"],"defaults":{{"level":0}}}}"#
            ),
            "urls",
        ),
        (
            "/v1/jobs",
            format!(
                r#"{{"type":"flow","type":"batch","urls":["{page}"],"defaults":{{"level":0}}}}"#
            ),
            "type",
        ),
        (
            "/v1/jobs",
            format!(r#"{{"type":"batch","urls":["{page}"],"defaults":{{"level":3,"level":0}}}}"#),
            "defaults.level",
        ),
        (
            "/v1/fetch",
            format!(r#"{{"url":"{page}","level":0,"headers":{{"X-A":"1","X-A":"2"}}}}"#),
            "headers.X-A",
        ),
    ] {
        let reply = api
            .send(raw(at, &token, Some("application/json"), &body))
            .await;
        assert_eq!(field(&reply), member, "{body}");
        assert!(
            reply.json()["detail"].as_str().unwrap().contains("twice"),
            "{body}"
        );
    }
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(api.lab.store().pool())
        .await
        .unwrap();
    assert_eq!(jobs, 0);

    // `application/json` and no other type, a `+json` one included.
    let body = json!({ "url": page, "level": 0 }).to_string();
    for other in [
        "application/problem+json",
        "application/vnd.x+json",
        "text/plain",
    ] {
        api.send(raw("/v1/fetch", &token, Some(other), &body))
            .await
            .problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
    }

    // A body that says it is too large, on endpoints that take none.
    for (verb, at) in [
        (Method::POST, "/v1/admin/policies/reload"),
        (
            Method::POST,
            "/v1/jobs/0190a0b0-0000-7000-8000-000000000001/cancel",
        ),
        (Method::GET, "/v1/admin/pool"),
        (Method::GET, "/healthz"),
        (Method::POST, "/nope"),
    ] {
        let sent = Request::builder()
            .method(verb.clone())
            .uri(at)
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_LENGTH, "1048577")
            .body(Body::empty())
            .unwrap();
        api.send(sent)
            .await
            .problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    }
    api.shut_down().await;
}

#[tokio::test]
async fn a_path_that_cannot_be_read_is_a_problem() {
    // F134 (api-10): before, a line of plain text.
    let mut api = Api::start(0, 149, FAST).await;
    let token = api
        .key(&[Scope::Fetch, Scope::Jobs, Scope::Sessions, Scope::Admin])
        .await;
    for (verb, at) in [
        (Method::GET, "/v1/results/%ff"),
        (Method::GET, "/v1/artifacts/%ff"),
        (Method::GET, "/v1/jobs/%ff"),
        (Method::GET, "/v1/jobs/%ff/items"),
        (Method::POST, "/v1/jobs/%ff/cancel"),
        (Method::GET, "/v1/sessions/%ff"),
        (Method::DELETE, "/v1/sessions/%ff"),
        (Method::GET, "/v1/admin/personas/%ff"),
        (Method::POST, "/v1/admin/keys/%ff/rotate"),
        (Method::GET, "/v1/policies/domains/%ff"),
    ] {
        let reply = api.call(verb.clone(), at, Some(&token), None).await;
        let problem = reply.problem(StatusCode::BAD_REQUEST, "invalid_request");
        assert!(
            problem["detail"].as_str().unwrap().contains("UTF-8"),
            "{verb} {at}: {problem}"
        );
    }
    api.shut_down().await;
}

#[tokio::test]
async fn a_head_over_the_limits_is_a_problem_with_its_request_id() {
    // F134 (api-11, api-12).
    let mut api = Api::start_with(0, 149, FAST, |config| {
        config.server.max_header_bytes = 8192;
        config.server.max_headers = 16;
    })
    .await;
    let served = api.listen().await;
    let patience = Duration::from_secs(5);
    let with_headers = |count: usize, pad: usize| {
        let mut head = String::from("GET /healthz HTTP/1.1\r\nHost: kage\r\n");
        for number in 0..count {
            let _ = write!(head, "X-Pad-{number}: 1\r\n");
        }
        if pad > 0 {
            let _ = write!(head, "X-Long: {}\r\n", "p".repeat(pad));
        }
        head.push_str("Connection: close\r\n\r\n");
        head
    };

    // Over the limits the API checks: a problem, with the request's id.
    for (head, status, code) in [
        (with_headers(20, 0), 431, "headers_too_large"),
        (with_headers(0, 10_000), 431, "headers_too_large"),
        (
            format!(
                "GET /healthz?pad={} HTTP/1.1\r\nHost: kage\r\nConnection: close\r\n\r\n",
                "p".repeat(12_000)
            ),
            414,
            "uri_too_long",
        ),
    ] {
        let answer = served.exchange(head.as_bytes(), patience).await;
        let (got, problem, identified) = parsed(&answer);
        assert_eq!(got, status, "{answer}");
        assert_eq!(problem.unwrap()["code"], code, "{answer}");
        assert!(identified, "{answer}");
    }
    // Beyond twice the limits the HTTP layer answers alone, as documented.
    let answer = served
        .exchange(with_headers(40, 0).as_bytes(), patience)
        .await;
    assert!(answer.starts_with("HTTP/1.1 431"), "{answer}");
    // Within them, an ordinary request.
    let answer = served
        .exchange(with_headers(10, 1000).as_bytes(), patience)
        .await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    served.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn the_self_test_reads_its_body_by_the_rules_of_every_other() {
    // F113 (api-03).
    let mut api = Api::start_with(1, 149, FAST, |config| {
        config.server.max_body_bytes = 4096;
        config.server.body_read_timeout = HumanDuration::new(Duration::from_millis(600));
    })
    .await;
    let admin = api.key(&[Scope::Admin]).await;
    let at = "/v1/admin/personas/0190a0b0-0000-7000-8000-000000000001/selftest";
    for content_type in [Some("text/plain"), None] {
        api.send(raw(at, &admin, content_type, "{}"))
            .await
            .problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
    }
    api.send(raw(at, &admin, Some("application/json"), "{"))
        .await
        .problem(StatusCode::BAD_REQUEST, "invalid_request");
    // Read whole and well formed, the request is the handler's.
    api.send(raw(at, &admin, Some("application/json"), "{}"))
        .await
        .problem(StatusCode::NOT_FOUND, "persona_not_found");
    api.send(raw(at, &admin, None, ""))
        .await
        .problem(StatusCode::NOT_FOUND, "persona_not_found");
    // Too large, whether it says so or not.
    api.send(raw(at, &admin, Some("application/json"), &" ".repeat(5000)))
        .await
        .problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    let chunks = futures_util::stream::iter(
        (0..8).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b' '; 1024]))),
    );
    let streamed = Request::builder()
        .method(Method::POST)
        .uri(at)
        .header(AUTHORIZATION, format!("Bearer {admin}"))
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from_stream(chunks))
        .unwrap();
    api.send(streamed)
        .await
        .problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    // A body that does not come is given up on.
    let served = api.listen().await;
    let head = format!(
        "POST {at} HTTP/1.1\r\nHost: kage\r\nAuthorization: Bearer {admin}\r\n\
         Content-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
    );
    let started = std::time::Instant::now();
    let answer = served
        .exchange(head.as_bytes(), Duration::from_secs(10))
        .await;
    let (status, problem, _) = parsed(&answer);
    assert_eq!(status, 408, "{answer}");
    assert_eq!(problem.unwrap()["code"], "request_timeout");
    assert!(started.elapsed() < Duration::from_secs(5));
    served.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_key_is_given_no_budget_the_store_cannot_keep() {
    // F134 (auth-01): before, 500 for 2^31 and more.
    let mut api = Api::start(0, 149, FAST).await;
    let admin = api.key(&[Scope::Admin]).await;
    for budget in [0_u64, 100_001, 2_147_483_648, 4_294_967_295] {
        let reply = api
            .post(
                &admin,
                "/v1/admin/keys",
                &json!({ "name": "x", "scopes": ["fetch"], "daily_page_budget": budget }),
            )
            .await;
        assert_eq!(field(&reply), "daily_page_budget", "{budget}");
    }
    let made = api
        .post(
            &admin,
            "/v1/admin/keys",
            &json!({ "name": "x", "scopes": ["fetch"], "daily_page_budget": 100_000 }),
        )
        .await;
    assert_eq!(made.status, StatusCode::CREATED);
    api.shut_down().await;
}

#[tokio::test]
async fn an_operator_reads_and_cancels_any_keys_job_and_the_trail_says_so() {
    // F117 (auth-02), as the planner decided it: an operator's power, on
    // the audit trail with both keys' ids; every cancel is there.
    let mut api = Api::start_with(2, 149, FAST, quick).await;
    Mock::given(method("GET"))
        .and(path_regex("^/slow/\\d+$"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(article("Slow"), "text/html; charset=utf-8")
                .set_delay(Duration::from_millis(400)),
        )
        .mount(&api.lab.site)
        .await;
    let (owner_id, owner) = api.key_with(&[Scope::Jobs], common::ROOMY).await;
    let other = api.key(&[Scope::Jobs]).await;
    let (operator_id, operator) = api
        .key_with(&[Scope::Admin, Scope::Jobs], common::ROOMY)
        .await;
    let auditor = api.key(&[Scope::Admin]).await;
    let urls: Vec<String> = (1..=12).map(|n| Api::url(&format!("/slow/{n}"))).collect();
    let batch = json!({ "type": "batch", "urls": urls, "defaults": { "level": 1 } });
    let work = api.work();
    let id = api.post(&owner, "/v1/jobs", &batch).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Another key without `admin`: not there.
    for (verb, at) in [
        (Method::GET, format!("/v1/jobs/{id}")),
        (Method::GET, format!("/v1/jobs/{id}/items")),
        (Method::POST, format!("/v1/jobs/{id}/cancel")),
    ] {
        api.call(verb, &at, Some(&other), None)
            .await
            .problem(StatusCode::NOT_FOUND, "job_not_found");
    }
    // The owner reads its own job: nothing on the trail.
    let before = audit(&api, &auditor).await.len();
    assert_eq!(
        api.get(&owner, &format!("/v1/jobs/{id}")).await.status,
        StatusCode::OK
    );
    let after = audit(&api, &auditor).await;
    assert!(
        !after.iter().any(|event| event["action"] == "job.show"),
        "{after:?}"
    );
    // The listing itself is on the trail, nothing else came.
    assert_eq!(after.len(), before + 1);

    // An operator reads and cancels it.
    assert_eq!(
        api.get(&operator, &format!("/v1/jobs/{id}")).await.status,
        StatusCode::OK
    );
    assert_eq!(
        api.get(&operator, &format!("/v1/jobs/{id}/items"))
            .await
            .status,
        StatusCode::OK
    );
    let cancelled = api
        .call(
            Method::POST,
            &format!("/v1/jobs/{id}/cancel"),
            Some(&operator),
            None,
        )
        .await;
    assert_eq!(cancelled.status, StatusCode::OK);
    let done = api
        .job_until(&owner, &id, |job| job["state"] == "cancelled")
        .await;
    assert_eq!(done["cancel_requested"], true);

    let trail = audit(&api, &auditor).await;
    for action in ["job.show", "job.items", "job.cancel"] {
        let event = trail
            .iter()
            .find(|event| event["action"] == action)
            .unwrap_or_else(|| panic!("no {action} in {trail:?}"));
        assert_eq!(event["api_key_id"], operator_id.to_string(), "{event}");
        assert_eq!(event["target"], id, "{event}");
        assert_eq!(event["outcome"], "success", "{event}");
        assert_eq!(
            event["details"]["owner_key"],
            owner_id.to_string(),
            "{event}"
        );
        assert_eq!(event["details"]["another_keys"], true, "{event}");
    }

    // The owner's own cancel is on the trail too.
    let second = api.post(&owner, "/v1/jobs", &batch).await.json()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    api.call(
        Method::POST,
        &format!("/v1/jobs/{second}/cancel"),
        Some(&owner),
        None,
    )
    .await;
    let trail = audit(&api, &auditor).await;
    let own = trail
        .iter()
        .find(|event| event["action"] == "job.cancel" && event["target"] == second)
        .unwrap_or_else(|| panic!("no cancel of {second} in {trail:?}"));
    assert_eq!(own["api_key_id"], owner_id.to_string());
    assert_eq!(own["details"]["another_keys"], false);
    api.job_until(&owner, &second, |job| job["state"] == "cancelled")
        .await;
    work.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_refusal_of_the_metrics_or_the_docs_is_on_the_trail() {
    // F135 (auth-04).
    let mut api = Api::start(0, 149, FAST).await;
    let (fetcher_id, fetcher) = api.key_with(&[Scope::Fetch], common::ROOMY).await;
    let admin = api.key(&[Scope::Admin]).await;
    api.get(&fetcher, "/metrics")
        .await
        .problem(StatusCode::FORBIDDEN, "scope_missing");
    api.get(&fetcher, "/docs/")
        .await
        .problem(StatusCode::FORBIDDEN, "scope_missing");
    // A scrape that is let through is no event.
    assert_eq!(api.get(&admin, "/metrics").await.status, StatusCode::OK);
    let trail = audit(&api, &admin).await;
    for (action, at) in [
        ("admin.metrics.read", "/metrics"),
        ("admin.docs.read", "/docs/"),
    ] {
        let events: Vec<&Value> = trail
            .iter()
            .filter(|event| event["action"] == action)
            .collect();
        assert_eq!(events.len(), 1, "{action}: {trail:?}");
        assert_eq!(events[0]["outcome"], "denied");
        assert_eq!(events[0]["api_key_id"], fetcher_id.to_string());
        assert_eq!(events[0]["details"]["path"], at);
        assert_eq!(events[0]["details"]["code"], "scope_missing");
    }
    api.shut_down().await;
}

#[tokio::test]
async fn the_preview_is_a_host_names_and_shows_the_cache_that_holds() {
    // F136 (policy-06, extract-11).
    let rules = "defaults: { pacing: fast, active_hours: ignore }\n\
                 domains:\n  long.test:\n    cache: { max_ttl: 30d, ttl: 30d }\n";
    let mut api = Api::start(0, 149, rules).await;
    let token = api.key(&[Scope::Fetch]).await;
    let label = "a".repeat(300);
    for name in ["*.kage.test", "*", "x..y.test", &format!("{label}.test")] {
        let reply = api
            .get(&token, &format!("/v1/policies/domains/{name}"))
            .await;
        assert_eq!(field(&reply), "domain", "{name}");
    }
    // The configuration's bound is a day: that is what holds.
    let preview = api.get(&token, "/v1/policies/domains/long.test").await;
    assert_eq!(preview.status, StatusCode::OK);
    let cache = &preview.json()["policy"]["cache"];
    assert_eq!(cache["max_ttl"], "1d", "{cache}");
    assert_eq!(cache["ttl"], "1d", "{cache}");
    let plain = api
        .get(&token, "/v1/policies/domains/example.org")
        .await
        .json();
    assert_eq!(plain["policy"]["cache"]["max_ttl"], "1d");
    assert_eq!(plain["policy"]["cache"]["ttl"], Value::Null);
    api.shut_down().await;
}

#[tokio::test]
async fn a_stopping_server_answers_until_its_requests_have_ended() {
    // F121 (jobs-04, api-16): the listener stays while the server stops,
    // and says `503 stopping` to whoever asks.
    let mut api = Api::start(1, 149, FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(article("Slow"), "text/html; charset=utf-8")
                .set_delay(Duration::from_millis(1500)),
        )
        .mount(&api.lab.site)
        .await;
    let served = api.listen().await;
    let body = json!({ "url": Api::url("/slow"), "level": 1 }).to_string();
    let head = format!(
        "POST /v1/fetch HTTP/1.1\r\nHost: kage\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut in_flight = served.connect().await;
    in_flight.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The signal: what `kage serve` does first.
    api.state.stop();
    let settled = tokio::spawn({
        let state = api.state.clone();
        async move {
            state
                .settle(tokio::time::Instant::now() + Duration::from_secs(20))
                .await;
        }
    });
    // A new connection is answered, and told the server stops.
    let answer = served
        .exchange(
            b"GET /readyz HTTP/1.1\r\nHost: kage\r\nConnection: close\r\n\r\n",
            Duration::from_secs(5),
        )
        .await;
    let (status, report, identified) = parsed(&answer);
    assert_eq!(status, 503, "{answer}");
    assert_eq!(report.unwrap()["status"], "stopping");
    assert!(identified);
    assert!(!settled.is_finished());
    // The request in flight ends, and only then is the server settled.
    let answer = read_all(&mut in_flight, Duration::from_secs(10)).await;
    assert_eq!(parsed(&answer).0, 200, "{answer}");
    tokio::time::timeout(Duration::from_secs(5), settled)
        .await
        .expect("the server did not settle once its request ended")
        .unwrap();
    served.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn a_result_says_when_it_was_made_as_a_read_of_it_says_later() {
    // F134 (api-09): an answer from the cache said nanoseconds.
    let mut api = Api::start(1, 149, FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    Mock::given(method("GET"))
        .and(path("/cached"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(article("Cached"), "text/html; charset=utf-8"),
        )
        .mount(&api.lab.site)
        .await;
    let body = json!({ "url": Api::url("/cached"), "level": 1, "cache_ttl_s": 600 });
    for _ in 0..2 {
        let answer = api.post(&token, "/v1/fetch", &body).await.json();
        let read = api
            .get(
                &token,
                &format!("/v1/results/{}", answer["id"].as_str().unwrap()),
            )
            .await
            .json();
        assert_eq!(answer["created_at"], read["created_at"], "{answer}");
    }
    let last = api.post(&token, "/v1/fetch", &body).await.json();
    assert_eq!(last["cached"], true, "{last}");
    api.shut_down().await;
}

#[tokio::test]
async fn an_unknown_method_is_a_problem_like_any_other_answer() {
    // F131 (secrecy-04): the label it is counted under is `other` (see
    // the unit test of `method_label`); the answer is a problem.
    let mut api = Api::start(0, 149, FAST).await;
    for at in ["/nosuch", "/healthz"] {
        let sent = Request::builder()
            .method(Method::from_bytes(b"QAS04X1").unwrap())
            .uri(at)
            .body(Body::empty())
            .unwrap();
        let reply = api.send(sent).await;
        assert!(reply.status.is_client_error(), "{at}: {}", reply.status);
        assert_eq!(
            reply.header("content-type"),
            Some("application/problem+json")
        );
    }
    api.shut_down().await;
}
