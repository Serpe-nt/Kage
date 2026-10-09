//! The sidecar client against a mock sidecar: what it sends, how it reads
//! answers and refusals, when it tries again and when it must not.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::BodySays;

use kage_browser::contract::{
    self, ErrorCode, ExportStateRequest, GeneratePersonaRequest, ImportStateRequest,
    NavigateRequest, OpenContextRequest, PageResult, SelftestRequest,
};
use kage_browser::{BrowserError, Closed, Navigation, SidecarClient, SidecarSettings};
use kage_core::config::Config;
use kage_core::domain::Engine;
use kage_core::secret::Secret;
use kage_core::ssrf::{DEFAULT_MAX_REDIRECTS, SsrfPolicy};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "client-test-token-0123456789abcdef";
const CONTEXT: &str = "0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f";

fn example<T: DeserializeOwned>(schema: &str, index: usize) -> T {
    serde_json::from_value(example_json(schema, index)).unwrap()
}

fn example_json(schema: &str, index: usize) -> Value {
    let examples: Value = serde_json::from_str(contract::EXAMPLES_JSON).unwrap();
    examples[schema][index].clone()
}

fn settings(url: &str) -> SidecarSettings {
    SidecarSettings {
        url: url.to_owned(),
        token: Secret::new(TOKEN),
        connect_timeout: Duration::from_secs(2),
        timeout: Duration::from_secs(5),
        connect_retries: 2,
    }
}

fn client(server: &MockServer) -> SidecarClient {
    SidecarClient::new(&settings(&server.uri())).unwrap()
}

fn context() -> Uuid {
    CONTEXT.parse().unwrap()
}

/// A sidecar that answers every operation with the contract's example, once,
/// and only to a call that carries the shared secret and the example body.
async fn sidecar_of_the_examples() -> MockServer {
    let server = MockServer::start().await;
    let authorized = || header("x-kage-sidecar-token", TOKEN);
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .and(authorized())
        .respond_with(ResponseTemplate::new(200).set_body_json(example_json("HealthResponse", 0)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/metrics"))
        .and(authorized())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("sidecar_contexts_open{engine=\"chromium\"} 1\n"),
        )
        .expect(1)
        .mount(&server)
        .await;
    for (route, request, response, status) in [
        (
            "/v1/personas/generate",
            "GeneratePersonaRequest",
            "GeneratePersonaResponse",
            200,
        ),
        (
            "/v1/contexts",
            "OpenContextRequest",
            "OpenContextResponse",
            201,
        ),
        (
            "/v1/contexts/0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f/navigate",
            "NavigateRequest",
            "PageResult",
            200,
        ),
        (
            "/v1/contexts/0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f/state/export",
            "ExportStateRequest",
            "ExportStateResponse",
            200,
        ),
        (
            "/v1/contexts/0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f/state/import",
            "ImportStateRequest",
            "ImportStateResponse",
            200,
        ),
        (
            "/v1/contexts/0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f/selftest",
            "SelftestRequest",
            "SelftestResponse",
            200,
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(route))
            .and(authorized())
            .and(header("content-type", "application/json"))
            // The body the sidecar receives is the contract's example, to the letter.
            .and(BodySays(example_json(request, 0)))
            .respond_with(ResponseTemplate::new(status).set_body_json(example_json(response, 0)))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/contexts/{CONTEXT}")))
        .and(authorized())
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn every_call_carries_the_shared_secret_and_reads_the_contracts_answer() {
    let server = sidecar_of_the_examples().await;
    let client = client(&server);
    let health = client.health().await.unwrap();
    assert_eq!(health.browser_version.as_deref(), Some("155.0.8059.39"));
    assert_eq!(health.contexts_in_use, 1);
    assert!(
        client
            .metrics()
            .await
            .unwrap()
            .starts_with("sidecar_contexts_open")
    );

    let persona = client
        .generate_persona(&example::<GeneratePersonaRequest>(
            "GeneratePersonaRequest",
            0,
        ))
        .await
        .unwrap();
    assert_eq!(persona.summary.browser_major, 155);
    assert_eq!(persona.fingerprint["schema"], 1);

    let opened = client
        .open_context(&example::<OpenContextRequest>("OpenContextRequest", 0))
        .await
        .unwrap();
    assert_eq!(opened.context_id, context());
    assert!(!opened.reused);

    let page = client
        .navigate(context(), &example::<NavigateRequest>("NavigateRequest", 0))
        .await
        .unwrap();
    assert_eq!(page, example::<PageResult>("PageResult", 0));

    let exported = client
        .export_state(
            context(),
            &example::<ExportStateRequest>("ExportStateRequest", 0),
        )
        .await
        .unwrap();
    assert_eq!(exported.cookies.len(), 2);
    let imported = client
        .import_state(
            context(),
            &example::<ImportStateRequest>("ImportStateRequest", 0),
        )
        .await
        .unwrap();
    assert_eq!(imported.imported, 1);
    let selftest = client
        .selftest(context(), &example::<SelftestRequest>("SelftestRequest", 0))
        .await
        .unwrap();
    assert!(selftest.passed && !selftest.complete);

    assert_eq!(
        client.close_context(context()).await.unwrap(),
        Closed::Closed
    );
    // The mock verifies on drop that every route was called exactly once.
}

#[tokio::test]
async fn a_refusal_carries_the_contracts_code_and_status() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/contexts"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "code": "context_limit", "message": "this container already runs 3 contexts"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/personas/generate"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "code": "cross_os_claim_disabled", "message": "linux only"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "code": "unauthorized", "message": "the x-kage-sidecar-token header is missing or wrong"
        })))
        .mount(&server)
        .await;
    let client = client(&server);

    let full = client
        .open_context(&example::<OpenContextRequest>("OpenContextRequest", 0))
        .await
        .unwrap_err();
    assert!(
        matches!(&full, BrowserError::Refused { operation: "open_context", status: 409, code: ErrorCode::ContextLimit, message } if message.contains("3 contexts")),
        "{full:?}"
    );
    assert!(full.is_at_capacity() && !full.is_unreachable() && !full.is_context_gone());
    assert_eq!(full.code(), Some(ErrorCode::ContextLimit));
    assert!(full.to_string().contains("context_limit (409)"), "{full}");

    let claim = client
        .generate_persona(&example::<GeneratePersonaRequest>(
            "GeneratePersonaRequest",
            0,
        ))
        .await
        .unwrap_err();
    assert_eq!(claim.code(), Some(ErrorCode::CrossOsClaimDisabled));

    let denied = client.health().await.unwrap_err();
    assert_eq!(denied.code(), Some(ErrorCode::Unauthorized));
    // The secret is never part of what an error says.
    assert!(!format!("{denied:?} {denied}").contains(TOKEN));
    assert!(!format!("{client:?}").contains(TOKEN));
}

#[tokio::test]
async fn closing_a_context_the_sidecar_forgot_counts_as_closed() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/contexts/{CONTEXT}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "code": "context_not_found", "message": "no open context has the id"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/contexts/{CONTEXT}")))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "code": "context_busy", "message": "running an operation"
        })))
        .mount(&server)
        .await;
    let client = client(&server);
    assert_eq!(
        client.close_context(context()).await.unwrap(),
        Closed::AlreadyGone
    );
    // Busy is not gone: the caller must know the context is still there.
    let busy = client.close_context(context()).await.unwrap_err();
    assert_eq!(busy.code(), Some(ErrorCode::ContextBusy));

    // A navigation on a forgotten context says so, and the caller reopens.
    let gone = client
        .navigate(context(), &example::<NavigateRequest>("NavigateRequest", 0))
        .await
        .unwrap_err();
    // (No mock for the route: wiremock answers 404 without a contract body.)
    assert!(
        matches!(gone, BrowserError::Unexpected { status: 404, .. }),
        "{gone:?}"
    );
}

#[tokio::test]
async fn an_answer_outside_the_contract_is_an_error_and_never_a_guess() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "status": "ok" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/contexts"))
        .respond_with(ResponseTemplate::new(502).set_body_string("<html>Bad Gateway</html>"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/metrics"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xff, 0xfe, 0x00]))
        .mount(&server)
        .await;
    let client = client(&server);

    let incomplete = client.health().await.unwrap_err();
    assert!(
        matches!(&incomplete, BrowserError::Unexpected { operation: "health", status: 200, detail } if detail.contains("missing field")),
        "{incomplete:?}"
    );
    let gateway = client
        .open_context(&example::<OpenContextRequest>("OpenContextRequest", 0))
        .await
        .unwrap_err();
    assert!(
        matches!(gateway, BrowserError::Unexpected { status: 502, .. }),
        "{gateway:?}"
    );
    assert_eq!(gateway.code(), None);
    let binary = client.metrics().await.unwrap_err();
    assert!(
        matches!(
            binary,
            BrowserError::Unexpected {
                operation: "metrics",
                ..
            }
        ),
        "{binary:?}"
    );
}

#[tokio::test]
async fn a_sidecar_that_cannot_be_reached_is_tried_again_and_then_reported() {
    // A port nothing listens on: bind, note the address, close.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    let client = SidecarClient::new(&settings(&format!("http://{address}"))).unwrap();
    let started = std::time::Instant::now();
    let error = client.health().await.unwrap_err();
    // One attempt and two retries, with 200 ms and 400 ms between them.
    assert!(
        matches!(&error, BrowserError::Connect { attempts: 3, url, .. } if url.contains(&address.port().to_string())),
        "{error:?}"
    );
    assert!(error.is_unreachable());
    assert!(
        started.elapsed() >= Duration::from_millis(550),
        "{:?}",
        started.elapsed()
    );

    let no_retry = SidecarClient::new(&SidecarSettings {
        connect_retries: 0,
        ..settings(&format!("http://{address}"))
    })
    .unwrap();
    assert!(matches!(
        no_retry.health().await.unwrap_err(),
        BrowserError::Connect { attempts: 1, .. }
    ));
}

#[tokio::test]
async fn a_sidecar_that_comes_up_between_attempts_is_reached() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let client = SidecarClient::new(&SidecarSettings {
        connect_retries: 5,
        ..settings(&format!("http://{address}"))
    })
    .unwrap();

    let call = tokio::spawn(async move { client.health().await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let server = MockServer::builder()
        .listener(std::net::TcpListener::bind(address).unwrap())
        .start()
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200).set_body_json(example_json("HealthResponse", 0)))
        .mount(&server)
        .await;
    assert_eq!(call.await.unwrap().unwrap().max_contexts, 3);
}

#[tokio::test]
async fn a_request_that_reached_the_sidecar_is_never_sent_twice() {
    let server = MockServer::start().await;
    // The sidecar takes longer than the client waits: the navigation may be
    // running, so the client must not start it again.
    Mock::given(method("POST"))
        .and(path(format!("/v1/contexts/{CONTEXT}/navigate")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(example_json("PageResult", 0))
                .set_delay(Duration::from_secs(3)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = SidecarClient::new(&SidecarSettings {
        timeout: Duration::from_millis(400),
        connect_retries: 5,
        ..settings(&server.uri())
    })
    .unwrap();
    let error = client
        .navigate(context(), &example::<NavigateRequest>("NavigateRequest", 0))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            BrowserError::Timeout {
                operation: "navigate"
            }
        ),
        "{error:?}"
    );
    assert!(!error.is_unreachable());
    // `expect(1)` verifies on drop that there was exactly one request.
}

#[tokio::test]
async fn a_navigation_is_checked_against_the_ssrf_guard_afterwards() {
    let server = MockServer::start().await;
    let mut redirected = example_json("PageResult", 0);
    redirected["redirects"] = json!([
        { "url": "https://example.org/listing?page=2", "status": 302, "location": "http://127.0.0.1:8090/v1/health" }
    ]);
    redirected["final_url"] = json!("http://127.0.0.1:8090/v1/health");
    Mock::given(method("POST"))
        .and(path(format!("/v1/contexts/{CONTEXT}/navigate")))
        .respond_with(ResponseTemplate::new(200).set_body_json(example_json("PageResult", 0)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/contexts/{CONTEXT}/navigate")))
        .respond_with(ResponseTemplate::new(200).set_body_json(redirected))
        .mount(&server)
        .await;
    let client = client(&server);
    let request = example::<NavigateRequest>("NavigateRequest", 0);

    let fine = client
        .navigate_checked(
            context(),
            &request,
            SsrfPolicy::default(),
            DEFAULT_MAX_REDIRECTS,
        )
        .await
        .unwrap();
    assert!(
        matches!(&fine, Navigation::Page(page) if page.dom_html.is_some()),
        "{fine:?}"
    );

    let refused = client
        .navigate_checked(
            context(),
            &request,
            SsrfPolicy::default(),
            DEFAULT_MAX_REDIRECTS,
        )
        .await
        .unwrap();
    let Navigation::SsrfDenied { denied, result } = refused else {
        panic!("a redirect to loopback passed the guard: {refused:?}");
    };
    assert_eq!(denied.hop, 1);
    assert_eq!(denied.url, "http://127.0.0.1:8090/v1/health");
    assert!(
        denied
            .evidence()
            .starts_with("redirect:1:http://127.0.0.1:8090/")
    );
    // Nothing the refused navigation captured is passed on.
    assert_eq!(
        (&result.dom_html, &result.raw_body, &result.screenshot),
        (&None, &None, &None)
    );
    assert!(result.xhr.is_empty() && result.headers.is_empty());
    assert_eq!(result.final_url, "http://127.0.0.1:8090/v1/health");
}

#[test]
fn the_settings_come_from_the_configuration_per_engine() {
    let config = Config::load(
        None,
        [
            (
                "KAGE__DATABASE__URL",
                "postgres://kage:kage@localhost:5432/kage",
            ),
            ("KAGE__SIDECAR__TOKEN", TOKEN),
            (
                "KAGE__SECRETS__MASTER_KEY",
                "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
            ),
            ("KAGE__SIDECAR__FIREFOX_URL", "http://firefox.internal:9000"),
            ("KAGE__SIDECAR__CONNECT_RETRIES", "7"),
        ]
        .map(|(name, value)| (name.to_owned(), value.to_owned())),
    )
    .unwrap();
    let chromium = SidecarSettings::from_config(&config, Engine::Chromium);
    assert_eq!(chromium.url, "http://browser-chromium:8090");
    assert_eq!(chromium.token.expose(), TOKEN);
    assert_eq!(chromium.connect_timeout, Duration::from_secs(5));
    // Longer than the longest attempt (spec §8.4).
    assert_eq!(chromium.timeout, Duration::from_secs(330));
    assert_eq!(chromium.connect_retries, 7);
    let firefox = SidecarSettings::from_config(&config, Engine::Firefox);
    assert_eq!(firefox.url, "http://firefox.internal:9000");
    assert_eq!(
        SidecarClient::new(&firefox).unwrap().base_url().as_str(),
        "http://firefox.internal:9000/"
    );
}

#[test]
fn a_client_is_only_built_for_an_http_url() {
    for url in [
        "https://browser-chromium:8090",
        "browser-chromium:8090",
        "",
        "ftp://x/",
    ] {
        let error = SidecarClient::new(&settings(url)).unwrap_err();
        assert!(matches!(error, BrowserError::Setup(_)), "{url}: {error:?}");
    }
}
