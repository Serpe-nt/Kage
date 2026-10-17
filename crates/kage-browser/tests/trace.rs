//! The trace context a request to a sidecar carries (spec §20). A file of
//! its own: the source is set once per process.

// A source is a `fn() -> Option<String>`, whatever these two return.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unnecessary_wraps
)]

use std::time::Duration;

use kage_browser::{SidecarClient, SidecarSettings, trace_context_from};
use kage_core::secret::Secret;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

fn current() -> Option<String> {
    Some(TRACEPARENT.to_owned())
}

fn other() -> Option<String> {
    Some("00-00000000000000000000000000000001-0000000000000001-01".to_owned())
}

#[tokio::test]
async fn a_request_to_a_sidecar_carries_the_traceparent_of_its_span_once_a_source_is_named() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client = SidecarClient::new(&SidecarSettings {
        url: server.uri(),
        token: Secret::new("t".repeat(32)),
        connect_timeout: Duration::from_secs(2),
        timeout: Duration::from_secs(5),
        connect_retries: 0,
    })
    .unwrap();

    // Nothing named, nothing sent.
    let _ = client.health().await;
    // Named once; a second source changes nothing.
    assert!(trace_context_from(current));
    assert!(!trace_context_from(other));
    let _ = client.health().await;

    let asked = server.received_requests().await.unwrap();
    assert_eq!(asked.len(), 2);
    assert!(asked[0].headers.get("traceparent").is_none());
    assert_eq!(
        asked[1]
            .headers
            .get("traceparent")
            .unwrap()
            .to_str()
            .unwrap(),
        TRACEPARENT
    );
}
