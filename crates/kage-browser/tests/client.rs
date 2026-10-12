//! The sidecar client against a mock sidecar: what it sends, how it reads
//! answers and refusals, when it tries again and when it must not.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::Duration;

use common::BodySays;

use kage_browser::contract::{
    self, ErrorCode, ExportStateRequest, FlowEvent, FlowRequest, GeneratePersonaRequest,
    ImportStateRequest, NavigateRequest, OpenContextRequest, PageResult, SelftestRequest,
};
use kage_browser::{
    BrowserError, Closed, Navigation, SidecarClient, SidecarSettings, navigation_bound,
};
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

/// The media type of a flow's answer, as the sidecar sends it.
const NDJSON: &str = "application/x-ndjson; charset=utf-8";

/// The route of a flow on the test context.
fn flow_route() -> String {
    format!("/v1/contexts/{CONTEXT}/flow")
}

/// The answer of a flow made of the contract's example events, one per
/// line. Examples 0 and 1 are steps (the second one failed), 2 and 3 are
/// final events (of a flow that passed and of one that did not).
fn flow_lines(examples: &[usize], end_of_line: &str) -> String {
    let mut lines = String::new();
    for index in examples {
        lines.push_str(&example_json("FlowEvent", *index).to_string());
        lines.push_str(end_of_line);
    }
    lines
}

/// A sidecar that answers the flows it is asked for with `answers`, in
/// that order, whatever the request says.
async fn sidecar_of_flows(answers: Vec<ResponseTemplate>) -> MockServer {
    let server = MockServer::start().await;
    for answer in answers {
        Mock::given(method("POST"))
            .and(path(flow_route()))
            .respond_with(answer)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
    }
    server
}

fn stream(body: impl Into<Vec<u8>>) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, NDJSON)
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
    // A flow is asked like every other operation and answered line by line.
    Mock::given(method("POST"))
        .and(path(flow_route()))
        .and(authorized())
        .and(header("content-type", "application/json"))
        .and(BodySays(example_json("FlowRequest", 0)))
        .respond_with(stream(flow_lines(&[0, 1, 3], "\n")))
        .expect(1)
        .mount(&server)
        .await;
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

    // Two steps and the final event, in the order the sidecar wrote them.
    let events = client
        .flow(context(), &example::<FlowRequest>("FlowRequest", 0))
        .await
        .unwrap();
    assert_eq!(
        events,
        [
            example::<FlowEvent>("FlowEvent", 0),
            example::<FlowEvent>("FlowEvent", 1),
            example::<FlowEvent>("FlowEvent", 3),
        ]
    );

    assert_eq!(
        client.close_context(context()).await.unwrap(),
        Closed::Closed
    );
    // The mock verifies on drop that every route was called exactly once.
}

#[tokio::test]
async fn a_flow_is_read_line_by_line_whatever_ends_its_lines() {
    let server = sidecar_of_flows(vec![
        // `\r\n` between the lines and none after the last one.
        stream(flow_lines(&[0, 1, 2], "\r\n").trim_end().to_owned()),
        // An empty line is no event.
        stream(format!(
            "{}\n\n{}\n\n",
            example_json("FlowEvent", 0),
            example_json("FlowEvent", 2)
        )),
        // A flow of no finished step: the final event alone.
        stream(flow_lines(&[3], "\n")),
    ])
    .await;
    let client = client(&server);
    let request = example::<FlowRequest>("FlowRequest", 0);

    let events = client.flow(context(), &request).await.unwrap();
    assert_eq!(events.len(), 3);
    assert!(
        matches!(
            &events[0],
            FlowEvent::Step { index: 0, action, ok: true, error: None, .. } if action == "goto"
        ),
        "{:?}",
        events[0]
    );
    // A step that failed is an event, not an error of the call.
    assert!(
        matches!(
            &events[1],
            FlowEvent::Step { index: 3, ok: false, error: Some(error), .. } if error.contains("#results")
        ),
        "{:?}",
        events[1]
    );
    let FlowEvent::Final {
        ok,
        result,
        behavior,
    } = &events[2]
    else {
        panic!("the last event is not the final one: {:?}", events[2]);
    };
    assert!(*ok);
    assert_eq!(result.status, Some(200));
    assert_eq!(behavior.as_ref().map(|report| report.clicks), Some(1));

    let events = client.flow(context(), &request).await.unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[1], FlowEvent::Final { ok: true, .. }));

    let events = client.flow(context(), &request).await.unwrap();
    assert!(
        matches!(
            &events[..],
            [FlowEvent::Final {
                ok: false,
                behavior: None,
                ..
            }]
        ),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_flow_whose_answer_ends_without_a_final_event_was_interrupted() {
    let server = sidecar_of_flows(vec![
        stream(flow_lines(&[0, 1], "\n")),
        // The stream began and ended at once.
        stream(""),
    ])
    .await;
    let client = client(&server);
    let request = example::<FlowRequest>("FlowRequest", 0);

    let broken = client.flow(context(), &request).await.unwrap_err();
    let BrowserError::FlowInterrupted { steps } = &broken else {
        panic!("{broken:?}");
    };
    // The steps that finished are kept: a site has seen them.
    assert_eq!(
        steps,
        &[
            example::<FlowEvent>("FlowEvent", 0),
            example::<FlowEvent>("FlowEvent", 1),
        ]
    );
    assert!(
        broken
            .to_string()
            .contains("ended after 2 step(s) without a final event"),
        "{broken}"
    );
    // The request was sent and answered: nothing to try again, no code.
    assert!(!broken.is_unreachable() && !broken.is_context_gone());
    assert_eq!(broken.code(), None);

    let empty = client.flow(context(), &request).await.unwrap_err();
    assert!(
        matches!(&empty, BrowserError::FlowInterrupted { steps } if steps.is_empty()),
        "{empty:?}"
    );
}

#[tokio::test]
async fn a_flow_refused_before_its_first_step_is_a_refusal_like_any_other() {
    let server = sidecar_of_flows(vec![
        ResponseTemplate::new(409).set_body_json(json!({
            "code": "context_busy", "message": "the context is running another operation"
        })),
        ResponseTemplate::new(404).set_body_json(json!({
            "code": "context_not_found", "message": "no open context has the id"
        })),
        ResponseTemplate::new(400).set_body_json(json!({
            "code": "invalid_request", "message": "actions: at least one step"
        })),
        ResponseTemplate::new(502).set_body_string("<html>Bad Gateway</html>"),
    ])
    .await;
    let client = client(&server);
    let request = example::<FlowRequest>("FlowRequest", 0);

    let busy = client.flow(context(), &request).await.unwrap_err();
    assert!(
        matches!(&busy, BrowserError::Refused { operation: "flow", status: 409, code: ErrorCode::ContextBusy, message } if message.contains("another operation")),
        "{busy:?}"
    );
    assert!(busy.to_string().contains("`flow` with context_busy (409)"));

    let gone = client.flow(context(), &request).await.unwrap_err();
    assert!(gone.is_context_gone(), "{gone:?}");

    let invalid = client.flow(context(), &request).await.unwrap_err();
    assert_eq!(invalid.code(), Some(ErrorCode::InvalidRequest));

    let gateway = client.flow(context(), &request).await.unwrap_err();
    assert!(
        matches!(
            gateway,
            BrowserError::Unexpected {
                operation: "flow",
                status: 502,
                ..
            }
        ),
        "{gateway:?}"
    );
}

#[tokio::test]
async fn a_line_that_is_no_flow_event_is_an_error_and_never_skipped() {
    let step = example_json("FlowEvent", 0);
    let last = example_json("FlowEvent", 2);
    let server = sidecar_of_flows(vec![
        // A line cut in the middle.
        stream(format!("{step}\n{{\"type\":\"step\",\"index\":1\n{last}\n")),
        // An event of a kind the contract does not have.
        stream(format!("{{\"type\":\"pause\"}}\n{last}\n")),
        // Not lines of JSON at all.
        stream(vec![0xff, 0xfe, b'\n']),
        // Something after the final event: the flow was over.
        stream(format!("{last}\n{step}\n")),
    ])
    .await;
    let client = client(&server);
    let request = example::<FlowRequest>("FlowRequest", 0);

    for (expected, case) in [
        ("line 2 is not a flow event", "a cut line"),
        ("line 1 is not a flow event", "an unknown kind"),
        ("line 1 is not a flow event", "bytes"),
        ("line 2 follows the final event", "a line after the end"),
    ] {
        let error = client.flow(context(), &request).await.unwrap_err();
        assert!(
            matches!(&error, BrowserError::Unexpected { operation: "flow", status: 200, detail } if detail.starts_with(expected)),
            "{case}: {error:?}"
        );
    }
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
    // The sidecar takes longer than the client waits: the import may be
    // running, so the client must not start it again.
    Mock::given(method("POST"))
        .and(path(format!("/v1/contexts/{CONTEXT}/state/import")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(example_json("ImportStateResponse", 0))
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
        .import_state(
            context(),
            &example::<ImportStateRequest>("ImportStateRequest", 0),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            BrowserError::Timeout {
                operation: "import_state"
            }
        ),
        "{error:?}"
    );
    assert!(!error.is_unreachable());
    // `expect(1)` verifies on drop that there was exactly one request.
}

#[test]
fn a_navigation_may_take_what_reaching_the_page_the_patience_and_the_dwell_take() {
    let secs = Duration::from_secs;
    let with = |change: &dyn Fn(&mut Value)| {
        let mut request = example_json("NavigateRequest", 0);
        change(&mut request);
        navigation_bound(&serde_json::from_value::<NavigateRequest>(request).unwrap())
    };
    // A bare load: the contract's own time to reach the page, and a margin.
    assert_eq!(with(&|_| {}), secs(60 + 30));
    assert_eq!(
        with(&|request| request["timeout_ms"] = json!(15_000)),
        secs(15 + 30)
    );
    // A bare load that stays for a fixed time stays that long.
    assert_eq!(
        with(&|request| {
            request["timeout_ms"] = json!(15_000);
            request["dwell_ms"] = json!(4_000);
        }),
        secs(15 + 4 + 30)
    );
    // A persona on the page: its patience with the scrolling and its own
    // dwell, which the sidecar draws and caps at 45 s.
    assert_eq!(
        with(&|request| {
            request["timeout_ms"] = json!(20_000);
            request["behavior"] = json!({ "dwell": "human", "patience_ms": 110_000 });
        }),
        secs(20 + 110 + 45 + 30)
    );
    // A dwell the request fixed is that dwell, not the persona's.
    assert_eq!(
        with(&|request| {
            request["timeout_ms"] = json!(20_000);
            request["dwell_ms"] = json!(7_500);
            request["behavior"] = json!({ "dwell": "fixed", "patience_ms": 38_000 });
        }),
        Duration::from_millis(20_000 + 38_000 + 7_500 + 30_000)
    );
    // No two pages with another patience are waited for as long: the bound
    // is the page's own and never a constant.
    let bound = |patience: u64| {
        with(&|request| request["behavior"] = json!({ "dwell": "human", "patience_ms": patience }))
    };
    assert_eq!(bound(240_000), bound(6_000) + secs(234));
    // Without a patience (a sidecar of 1.2.0 is asked like this) the
    // scrolling shares the time to reach the page, as before.
    assert_eq!(
        with(&|request| request["behavior"] = json!({ "dwell": "human" })),
        secs(60 + 45 + 30)
    );
}

#[tokio::test]
async fn a_navigation_is_waited_for_as_long_as_it_may_take_and_not_as_long_as_every_call() {
    let server = MockServer::start().await;
    // The page takes longer than the client waits for any other call: a
    // page that is read for a while is not cut off at the client's own
    // timeout, which would leave every long page after the same time.
    Mock::given(method("POST"))
        .and(path(format!("/v1/contexts/{CONTEXT}/navigate")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(example_json("PageResult", 0))
                .set_delay(Duration::from_millis(1500)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = SidecarClient::new(&SidecarSettings {
        timeout: Duration::from_millis(400),
        ..settings(&server.uri())
    })
    .unwrap();
    let page = client
        .navigate(context(), &example::<NavigateRequest>("NavigateRequest", 0))
        .await
        .unwrap();
    assert_eq!(page, example::<PageResult>("PageResult", 0));
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
