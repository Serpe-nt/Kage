//! The limits on what the API takes in (spec §19 "Input limits"), on the
//! router and on a real listener: bodies, headers, request targets,
//! clients that trickle, CORS, and what is answered where nothing is
//! served.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

mod common;

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{Method, Request, StatusCode};
use common::sched::lab::{LENIENT_FAST, article};
use common::{Api, read_all, request};
use kage_core::domain::Scope;
use kage_core::duration::HumanDuration;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn millis(count: u64) -> HumanDuration {
    HumanDuration::new(Duration::from_millis(count))
}

/// The status line's code and the JSON body of a raw HTTP/1.1 answer.
fn parsed(answer: &str) -> (u16, Option<Value>) {
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
    (status, body)
}

#[tokio::test]
async fn a_body_or_a_request_target_past_its_limit_is_refused() {
    let mut api = Api::start_with(0, 149, LENIENT_FAST, |config| {
        config.server.max_body_bytes = 4096;
        config.server.max_uri_bytes = 512;
    })
    .await;
    let token = api.key(&[Scope::Fetch, Scope::Admin]).await;

    // A body that says it is too long is refused before it is read.
    let long = json!({ "url": format!("https://example.org/{}", "a".repeat(5000)) });
    api.post(&token, "/v1/fetch", &long)
        .await
        .problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    // One that does not say how long it is, is cut off at the limit.
    let chunks = futures_util::stream::iter(
        (0..8).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b' '; 1024]))),
    );
    let streamed = Request::builder()
        .method(Method::POST)
        .uri("/v1/fetch")
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from_stream(chunks))
        .unwrap();
    api.send(streamed)
        .await
        .problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    // The same limit holds on every endpoint that takes a body.
    let name = "n".repeat(5000);
    api.post(
        &token,
        "/v1/admin/keys",
        &json!({ "name": name, "scopes": ["fetch"] }),
    )
    .await
    .problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    // A body below the limit is read: this one fails for what it says.
    api.post(&token, "/v1/fetch", &json!({ "url": "ftp://example.org/" }))
        .await
        .problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid_url");

    // A request target past its limit, with or without a key.
    let target = format!("/v1/policies/domains/example.org?pad={}", "x".repeat(600));
    api.get(&token, &target)
        .await
        .problem(StatusCode::URI_TOO_LONG, "uri_too_long");
    api.call(Method::GET, &target, None, None)
        .await
        .problem(StatusCode::URI_TOO_LONG, "uri_too_long");
    api.shut_down().await;
}

#[tokio::test]
async fn the_listener_refuses_heads_that_are_too_long_and_cuts_off_clients_that_trickle() {
    let mut api = Api::start_with(0, 149, LENIENT_FAST, |config| {
        config.server.max_header_bytes = 8192;
        config.server.max_headers = 16;
        config.server.header_read_timeout = millis(600);
        config.server.body_read_timeout = millis(600);
    })
    .await;
    let token = api.key(&[Scope::Fetch]).await;
    let served = api.listen().await;
    let patience = Duration::from_secs(5);

    // An ordinary request is served, and the connection is kept.
    let ok = served
        .exchange(
            b"GET /healthz HTTP/1.1\r\nHost: kage\r\nConnection: close\r\n\r\n",
            patience,
        )
        .await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    assert!(ok.contains("{\"status\":\"ok\"}"), "{ok}");
    assert!(ok.to_ascii_lowercase().contains("x-request-id:"), "{ok}");

    // More headers than allowed.
    let mut many = String::from("GET /healthz HTTP/1.1\r\nHost: kage\r\n");
    for number in 0..40 {
        let _ = write!(many, "X-Pad-{number}: 1\r\n");
    }
    many.push_str("\r\n");
    let answer = served.exchange(many.as_bytes(), patience).await;
    assert!(answer.starts_with("HTTP/1.1 431"), "{answer}");

    // A head longer than allowed, in one header or in the target.
    let fat = format!(
        "GET /healthz HTTP/1.1\r\nHost: kage\r\nX-Pad: {}\r\n\r\n",
        "p".repeat(10_000)
    );
    let answer = served.exchange(fat.as_bytes(), patience).await;
    assert!(answer.starts_with("HTTP/1.1 431"), "{answer}");
    let long = format!(
        "GET /healthz?pad={} HTTP/1.1\r\nHost: kage\r\n\r\n",
        "p".repeat(10_000)
    );
    let answer = served.exchange(long.as_bytes(), patience).await;
    let (status, _) = parsed(&answer);
    assert!(status == 414 || status == 431, "{answer}");

    // Slowloris: a head that arrives a byte at a time is cut off when the
    // time for a head is over, however alive the connection looks.
    let mut slow = served.connect().await;
    slow.write_all(b"GET /healthz HTTP/1.1\r\nHost: kage\r\n")
        .await
        .unwrap();
    let started = Instant::now();
    let trickle = tokio::spawn(async move {
        let mut cut = None;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            if slow.write_all(b"X-Slow: 1\r\n").await.is_err() {
                cut = Some(String::new());
                break;
            }
            // A closed connection reads as its end at once.
            let mut probe = [0_u8; 256];
            let readable = tokio::time::timeout(Duration::from_millis(10), slow.readable()).await;
            if matches!(readable, Ok(Ok(()))) {
                match slow.try_read(&mut probe) {
                    Ok(0) => {
                        cut = Some(String::new());
                        break;
                    }
                    Ok(read) => {
                        cut = Some(String::from_utf8_lossy(&probe[..read]).into_owned());
                        break;
                    }
                    Err(_) => {}
                }
            }
        }
        cut
    });
    // Meanwhile others are served.
    let ok = served
        .exchange(
            b"GET /healthz HTTP/1.1\r\nHost: kage\r\nConnection: close\r\n\r\n",
            patience,
        )
        .await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    let cut = trickle.await.unwrap();
    let took = started.elapsed();
    let answer = cut.expect("the trickling client was never cut off");
    // Either the connection just ends, or it ends with a 408.
    assert!(
        answer.is_empty() || answer.starts_with("HTTP/1.1 408"),
        "{answer}"
    );
    assert!(
        took >= Duration::from_millis(500) && took < Duration::from_secs(4),
        "{took:?}"
    );

    // A connection that sends nothing at all is closed after the same time.
    let mut idle = served.connect().await;
    let started = Instant::now();
    let answer = read_all(&mut idle, patience).await;
    assert!(
        answer.is_empty() || answer.starts_with("HTTP/1.1 408"),
        "{answer}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );

    // A body that is announced and does not come is given up on.
    let head = format!(
        "POST /v1/fetch HTTP/1.1\r\nHost: kage\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: 200\r\n\r\n{{\"url\":"
    );
    let started = Instant::now();
    let answer = served.exchange(head.as_bytes(), patience).await;
    let (status, problem) = parsed(&answer);
    assert_eq!(status, 408, "{answer}");
    assert_eq!(problem.unwrap()["code"], "request_timeout");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );

    served.stop().await;
    api.shut_down().await;
}

#[tokio::test]
async fn cors_is_off_until_origins_are_named() {
    let mut api = Api::start(0, 149, LENIENT_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    let at = "/v1/policies/domains/example.org";
    let with_origin = |verb: Method, origin: &str| {
        let mut sent = request(verb, at, Some(&token), None);
        sent.headers_mut().insert("origin", origin.parse().unwrap());
        sent.headers_mut()
            .insert("access-control-request-method", "GET".parse().unwrap());
        sent
    };

    // The default: no answer carries a CORS header, so no browser lets a
    // page of another origin read the API.
    let plain = api
        .send(with_origin(Method::GET, "https://console.example"))
        .await;
    assert_eq!(plain.status, StatusCode::OK);
    let preflight = api
        .send(with_origin(Method::OPTIONS, "https://console.example"))
        .await;
    for reply in [&plain, &preflight] {
        for (name, _) in &reply.headers {
            assert!(
                !name.as_str().starts_with("access-control-"),
                "{name} in {:?}",
                reply.headers
            );
        }
    }

    // With an origin named, that origin is answered and no other.
    let open = {
        let mut config = api.lab.config.clone();
        config.server.cors_allowed_origins = vec!["https://console.example".to_owned()];
        api.with_parts(|parts| parts.config = std::sync::Arc::new(config))
    };
    let allowed = common::ask(
        &open,
        with_origin(Method::OPTIONS, "https://console.example"),
    )
    .await;
    assert_eq!(
        allowed.header("access-control-allow-origin"),
        Some("https://console.example")
    );
    assert!(
        allowed
            .header("access-control-allow-headers")
            .unwrap()
            .to_ascii_lowercase()
            .contains("authorization")
    );
    // Never credentials, never a wildcard.
    assert_eq!(allowed.header("access-control-allow-credentials"), None);
    let got = common::ask(&open, with_origin(Method::GET, "https://console.example")).await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(
        got.header("access-control-allow-origin"),
        Some("https://console.example")
    );
    let stranger = common::ask(&open, with_origin(Method::GET, "https://evil.example")).await;
    assert_eq!(stranger.header("access-control-allow-origin"), None);
    api.shut_down().await;
}

#[tokio::test]
async fn what_is_not_served_is_a_problem_document_too() {
    let mut api = Api::start(0, 149, LENIENT_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;

    // No such path, with or without a key: nothing says which paths exist
    // for whom.
    for at in [
        "/",
        "/v1",
        "/v1/jobs",
        "/v2/fetch",
        "/admin",
        "/v1/admin/nothing",
    ] {
        api.call(Method::GET, at, None, None)
            .await
            .problem(StatusCode::NOT_FOUND, "not_found");
    }
    api.get(&token, "/v1/nothing")
        .await
        .problem(StatusCode::NOT_FOUND, "not_found");
    // A path that is served, with a method that is not.
    for (verb, at) in [
        (Method::GET, "/v1/fetch"),
        (Method::PUT, "/v1/sessions"),
        (Method::DELETE, "/healthz"),
        (Method::POST, "/openapi.json"),
    ] {
        let reply = api.call(verb.clone(), at, Some(&token), None).await;
        reply.problem(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
    }
    // Every answer names its request, and two requests never share a name.
    let first = api.call(Method::GET, "/healthz", None, None).await;
    let second = api.call(Method::GET, "/healthz", None, None).await;
    let id = first.header("x-request-id").unwrap();
    assert_eq!(id.len(), 36);
    assert_ne!(Some(id), second.header("x-request-id"));
    // An id a client sends is not taken over: it could be anyone's.
    let mut sent = request(Method::GET, "/healthz", None, None);
    sent.headers_mut()
        .insert("x-request-id", "mine".parse().unwrap());
    assert_ne!(api.send(sent).await.header("x-request-id"), Some("mine"));

    // The document of the API is public and says what it is; its viewer
    // is an admin's.
    let document = api.call(Method::GET, "/openapi.json", None, None).await;
    assert_eq!(document.status, StatusCode::OK);
    assert_eq!(document.json()["openapi"], "3.1.0");
    assert!(document.json()["paths"]["/v1/fetch"]["post"].is_object());
    api.call(Method::GET, "/docs/", None, None)
        .await
        .problem(StatusCode::UNAUTHORIZED, "missing_key");
    api.get(&token, "/docs/")
        .await
        .problem(StatusCode::FORBIDDEN, "scope_missing");
    let admin = api.key(&[Scope::Admin]).await;
    let viewer = api.get(&admin, "/docs/").await;
    assert_eq!(viewer.status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&viewer.body).contains("swagger"));
    let copy = api.get(&admin, "/docs/openapi.json").await;
    assert_eq!(copy.json()["info"]["title"], "Kage API");
    // Switched off, it is not there.
    let closed = {
        let mut config = api.lab.config.clone();
        config.server.docs = false;
        api.with_parts(|parts| parts.config = std::sync::Arc::new(config))
    };
    let sent = request(Method::GET, "/docs/", Some(&admin), None);
    common::ask(&closed, sent)
        .await
        .problem(StatusCode::NOT_FOUND, "not_found");
    api.shut_down().await;
}

#[tokio::test]
async fn readiness_says_what_is_missing_and_a_stop_lets_requests_in_flight_finish() {
    let mut api = Api::start(1, 149, LENIENT_FAST).await;
    let token = api.key(&[Scope::Fetch]).await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(article("Slow"))
                .set_delay(Duration::from_millis(1200)),
        )
        .mount(&api.lab.site)
        .await;

    // Ready: the database answers, a persona is ready behind a usable
    // proxy, a sidecar answers.
    let ready = api.call(Method::GET, "/readyz", None, None).await;
    assert_eq!(ready.status, StatusCode::OK, "{:?}", ready.json());
    assert_eq!(
        ready.json(),
        json!({
            "status": "ready",
            "checks": { "database": true, "persona": true, "sidecar": true },
        })
    );
    // Liveness asks nothing.
    let alive = api.call(Method::GET, "/healthz", None, None).await;
    assert_eq!(alive.json(), json!({ "status": "ok" }));

    // No sidecar answers: not ready, and it says why. (A server that
    // starts again asks again; an answer is reused for two seconds.)
    api.lab.chromium.with(|sidecar| sidecar.healthy = false);
    api.lab.firefox.with(|sidecar| sidecar.healthy = false);
    let fresh = api.restarted();
    let sent = request(Method::GET, "/readyz", None, None);
    let waiting = common::ask(&fresh, sent).await;
    assert_eq!(waiting.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(waiting.json()["status"], "not_ready");
    assert_eq!(
        waiting.json()["checks"],
        json!({ "database": true, "persona": true, "sidecar": false })
    );
    api.lab.chromium.with(|sidecar| sidecar.healthy = true);
    // Without a pool there is no persona to be ready.
    let bare = api.with_parts(|parts| parts.engine = None);
    let sent = request(Method::GET, "/readyz", None, None);
    let empty = common::ask(&bare, sent).await;
    assert_eq!(empty.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        empty.json()["checks"],
        json!({ "database": true, "persona": false, "sidecar": false })
    );

    // A stop: readiness says so at once, the fetch in flight gets its
    // answer, and nothing new is accepted.
    let mut served = api.listen().await;
    let body = json!({ "url": Api::url("/slow"), "render": "http" }).to_string();
    let head = format!(
        "POST /v1/fetch HTTP/1.1\r\nHost: kage\r\nAuthorization: Bearer {token}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut in_flight = served.connect().await;
    in_flight.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    api.state.stop();
    served.signal_stop();
    let stopping = api.call(Method::GET, "/readyz", None, None).await;
    assert_eq!(stopping.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(stopping.json()["status"], "stopping");
    let answer = read_all(&mut in_flight, Duration::from_secs(10)).await;
    let (status, result) = parsed(&answer);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(result.unwrap()["outcome"], "ok");
    // The listener has ended: nobody answers at its address.
    for _ in 0..50 {
        if served.is_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(served.is_finished());
    assert!(tokio::net::TcpStream::connect(served.addr).await.is_err());
    api.shut_down().await;
}
