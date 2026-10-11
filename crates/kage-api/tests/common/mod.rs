//! What the API's integration tests share: the API over the scheduler's
//! own lab.
//!
//! [`Api`] is the router of `kage serve` over everything real that can run
//! on one machine: Postgres, the pool with a list provider, the persona
//! manager, the forwarder, the scheduler, the real attempt executor and
//! the pipeline, mock sidecars that answer the contract, and a site behind
//! the provider's proxies (`kage-scheduler/tests/common/lab.rs`). Requests
//! are sent to the router directly; the tests of the listener's own limits
//! bind a socket.
//!
//! Nothing leaves the machine: the proxy health check reads its echo from
//! the lab's site through the provider's proxies ([`ECHO_PATH`]) and the
//! host's own address from a server of this harness.

// Each test binary uses a different subset of these helpers.
#![allow(
    dead_code,
    unreachable_pub,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::must_use_candidate
)]

#[path = "../../../kage-scheduler/tests/common/mod.rs"]
pub mod sched;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use bytes::Bytes;
use chrono::Utc;
use http_body_util::BodyExt;
use kage_api::keys::{NewKey, mint};
use kage_api::serve::{ServeSettings, serve};
use kage_api::{ApiParts, ApiState, EngineParts};
use kage_core::alert::AlertSink;
use kage_core::config::Config;
use kage_core::domain::{KeyLimits, Scope};
use kage_core::ids::ApiKeyId;
use kage_core::schedule::budget::BudgetRules;
use kage_proxy::HealthChecker;
use kage_scheduler::{Bandwidth, Ledger, StoreArchive, StoreLedger};
use kage_store::ArtifactStore;
use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub use sched::lab::{Lab, SITE};

/// Where the proxy health check reads a proxy's exit address: a path of
/// the lab's site, which answers 404 until a test serves it.
pub const ECHO_PATH: &str = "/health/ip";

/// The address the host's own echo answers with.
pub const HOST_ADDRESS: &str = "203.0.113.9";

/// Limits no test runs into by accident.
pub const ROOMY: KeyLimits = KeyLimits {
    requests_per_minute: 10_000,
    max_concurrent: 100,
    daily_page_budget: None,
};

/// What the API answered.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "the body is not JSON ({error}): {}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    /// The `code` of a problem document.
    pub fn code(&self) -> String {
        let json = self.json();
        json["code"]
            .as_str()
            .unwrap_or_else(|| panic!("no code in {json}"))
            .to_owned()
    }

    /// A header's value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Checks that the answer is a problem document with this status and
    /// code, and returns it.
    #[track_caller]
    pub fn problem(&self, status: StatusCode, code: &str) -> Value {
        assert_eq!(
            self.status,
            status,
            "{}",
            String::from_utf8_lossy(&self.body)
        );
        assert_eq!(
            self.header("content-type"),
            Some("application/problem+json")
        );
        let json = self.json();
        assert_eq!(json["code"], code, "{json}");
        assert_eq!(json["status"], status.as_u16(), "{json}");
        assert_eq!(json["type"], format!("urn:kage:problem:{code}"), "{json}");
        // Every problem names the request it answers.
        assert_eq!(
            json["request_id"].as_str(),
            self.header("x-request-id"),
            "{json}"
        );
        json
    }
}

/// A request with an optional key and an optional JSON body.
pub fn request(
    method: Method,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Request<Body> {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(json) => {
            request = request.header(CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(json).unwrap())
        }
        None => Body::empty(),
    };
    request.body(body).unwrap()
}

/// Sends a request to a router and reads the whole answer.
pub async fn ask(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes(),
    }
}

/// The API behind a socket of this machine, with the limits of the
/// configuration: what `kage serve` runs.
pub struct Served {
    pub addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Served {
    /// Tells the listener to stop and waits until it has.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.await.unwrap().unwrap();
    }

    /// Tells the listener to stop without waiting for it.
    pub fn signal_stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }

    /// Whether the listener has ended.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    /// Opens a connection.
    pub async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr).await.unwrap()
    }

    /// Sends `bytes` on a new connection and returns everything that
    /// comes back until the server closes it, or until `patience` is over.
    pub async fn exchange(&self, bytes: &[u8], patience: Duration) -> String {
        let mut stream = self.connect().await;
        stream.write_all(bytes).await.unwrap();
        read_all(&mut stream, patience).await
    }
}

/// Everything a connection sends until it is closed or `patience` is over.
pub async fn read_all(stream: &mut TcpStream, patience: Duration) -> String {
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(patience, async {
        let mut chunk = [0_u8; 4096];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(read) => answer.extend_from_slice(&chunk[..read]),
            }
        }
    })
    .await;
    String::from_utf8_lossy(&answer).into_owned()
}

/// See the module documentation.
pub struct Api {
    pub lab: Lab,
    pub parts: ApiParts,
    pub state: ApiState,
    pub router: Router,
    pub artifacts: ArtifactStore,
    /// What the health check asks for the host's own address.
    pub host_echo: MockServer,
    _artifact_dir: TempDir,
}

impl Api {
    /// The API over a lab with `personas` chromium personas at `major`
    /// (149 has an http path, 155 has none) under the domain rules
    /// `domains`.
    pub async fn start(personas: usize, major: u32, domains: &str) -> Self {
        Self::start_with(personas, major, domains, |_| {}).await
    }

    /// The same, with `change` applied to the configuration.
    pub async fn start_with(
        personas: usize,
        major: u32,
        domains: &str,
        change: impl FnOnce(&mut Config),
    ) -> Self {
        let artifact_dir = tempfile::tempdir().unwrap();
        let dir = artifact_dir.path().to_path_buf();
        let host_echo = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/host"))
            .respond_with(ResponseTemplate::new(200).set_body_string(HOST_ADDRESS))
            .mount(&host_echo)
            .await;
        let host_ip_url = format!("{}/host", host_echo.uri());
        let lab = Lab::start_with(personas, major, domains, |config| {
            config.artifacts.dir = dir;
            let health = &mut config.proxies.health;
            health.echo_url = format!("http://{SITE}{ECHO_PATH}");
            health.confirm_url = None;
            health.geo_url = None;
            health.host_ip_url = host_ip_url;
            health.timeout = kage_core::duration::HumanDuration::from_secs(3);
            change(config);
        })
        .await;
        let store = lab.db.store.clone();
        let artifacts = ArtifactStore::new(store.pool().clone(), &lab.config.artifacts);
        let clock = Arc::clone(lab.pool.clock());
        let alerts: Arc<dyn AlertSink> = lab.alerts.clone();
        let ledger: Arc<dyn Ledger> =
            Arc::new(StoreLedger::new(store.clone(), Arc::clone(&lab.pool)));
        let bandwidth = Arc::new(Bandwidth::new(
            Arc::clone(lab.forwarder.meter()),
            ledger,
            BudgetRules {
                cap_bytes: lab.config.bandwidth.cap_bytes(),
                warn_at: lab.config.bandwidth.warn_at,
                hard_stop: lab.config.bandwidth.hard_stop,
            },
            alerts,
            Arc::clone(&clock),
        ));
        let health =
            Arc::new(HealthChecker::from_config(&lab.config, Arc::clone(&lab.pool)).unwrap());
        let parts = ApiParts {
            config: Arc::new(lab.config.clone()),
            store,
            artifacts: artifacts.clone(),
            policies: Arc::clone(&lab.policies),
            clock,
            engine: Some(EngineParts {
                pipeline: lab
                    .pipeline
                    .clone()
                    .with_archive(Arc::new(StoreArchive::new(artifacts.clone()))),
                personas: Arc::clone(&lab.manager),
                pool: Arc::clone(&lab.pool),
                health,
                bandwidth,
            }),
            metrics: None,
        };
        let state = ApiState::new(parts.clone()).unwrap();
        let router = kage_api::router(state.clone());
        Self {
            lab,
            parts,
            state,
            router,
            artifacts,
            host_echo,
            _artifact_dir: artifact_dir,
        }
    }

    /// Serves the API on a socket of this machine.
    pub async fn listen(&self) -> Served {
        self.listen_with(self.router.clone()).await
    }

    /// Serves a router on a socket of this machine, with the limits of
    /// the configuration.
    pub async fn listen_with(&self, router: Router) -> Served {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let settings = ServeSettings::from_config(&self.lab.config.server);
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(serve(listener, router, settings, async move {
            let _ = stopped.await;
        }));
        Served {
            addr,
            stop: Some(stop),
            task,
        }
    }

    /// The router of a server that starts again over the same services:
    /// what the API keeps in memory is new, what the store keeps is not.
    pub fn restarted(&self) -> Router {
        kage_api::router(ApiState::new(self.parts.clone()).unwrap())
    }

    /// The router of a server with other parts.
    pub fn with_parts(&self, change: impl FnOnce(&mut ApiParts)) -> Router {
        let mut parts = self.parts.clone();
        change(&mut parts);
        kage_api::router(ApiState::new(parts).unwrap())
    }

    /// Makes a key with these scopes and roomy limits; returns its token.
    pub async fn key(&self, scopes: &[Scope]) -> String {
        self.key_with(scopes, ROOMY).await.1
    }

    /// Makes a key with these scopes and limits.
    pub async fn key_with(&self, scopes: &[Scope], limits: KeyLimits) -> (ApiKeyId, String) {
        let new = NewKey {
            name: format!("test-{}", uuid::Uuid::now_v7().simple()),
            scopes: scopes.iter().copied().collect::<BTreeSet<_>>(),
            limits,
        };
        let (key, token) = mint(&new, Utc::now()).unwrap();
        self.lab.db.store.api_keys().insert(&key).await.unwrap();
        (key.id, token.reveal().to_owned())
    }

    /// Sends a request to the router.
    pub async fn send(&self, request: Request<Body>) -> Reply {
        ask(&self.router, request).await
    }

    /// Sends a request with an optional key and an optional JSON body.
    pub async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Reply {
        self.send(request(method, path, token, body)).await
    }

    pub async fn get(&self, token: &str, path: &str) -> Reply {
        self.call(Method::GET, path, Some(token), None).await
    }

    pub async fn post(&self, token: &str, path: &str, body: &Value) -> Reply {
        self.call(Method::POST, path, Some(token), Some(body)).await
    }

    pub async fn delete(&self, token: &str, path: &str) -> Reply {
        self.call(Method::DELETE, path, Some(token), None).await
    }

    /// A URL of the lab's site.
    pub fn url(path: &str) -> String {
        format!("http://{SITE}{path}")
    }

    /// Stops what the lab started.
    pub async fn shut_down(&mut self) {
        self.lab.shut_down().await;
    }
}
