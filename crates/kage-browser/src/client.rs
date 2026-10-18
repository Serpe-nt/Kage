//! The client for one sidecar.

use std::sync::OnceLock;
use std::time::Duration;

use http_body_util::BodyExt as _;
use kage_core::config::Config;
use kage_core::domain::Engine;
use kage_core::secret::Secret;
use kage_core::ssrf::SsrfPolicy;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tracing::Instrument;
use url::Url;
use uuid::Uuid;

use crate::contract::{
    DwellMode, ErrorResponse, ExportStateRequest, ExportStateResponse, FlowEvent, FlowRequest,
    GeneratePersonaRequest, GeneratePersonaResponse, HealthResponse, ImportStateRequest,
    ImportStateResponse, NavigateRequest, OpenContextRequest, OpenContextResponse, PageResult,
    SelftestRequest, SelftestResponse, TOKEN_HEADER, UpgradePersonaRequest, UpgradePersonaResponse,
};
use crate::error::BrowserError;
use crate::ssrf::{self, DeniedHop};

/// Where the client finds the trace context of a request it makes: the
/// `traceparent` of the span the request is made in (spec §20). Set once by
/// the binary when it installs its tracer; unset, no header is sent.
static TRACE_CONTEXT: OnceLock<fn() -> Option<String>> = OnceLock::new();

/// Names where the client finds the W3C `traceparent` it sends with every
/// request to a sidecar, so that a sidecar's spans join the trace of the
/// request that asked. The header travels on the compose network to the
/// sidecar alone: a sidecar never hands it to a page or a target. Only the
/// first call counts; whether it did is returned.
pub fn trace_context_from(source: fn() -> Option<String>) -> bool {
    TRACE_CONTEXT.set(source).is_ok()
}

/// What the client sends as `User-Agent`: a sidecar is Kage's own service,
/// not a target.
const USER_AGENT: &str = concat!("kage/", env!("CARGO_PKG_VERSION"));

/// Wait before the first connection retry; it doubles up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_millis(200);

/// Longest wait between two connection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// The most of an answer a sidecar may send. A page result with its
/// document, the responses it captured and its log is a few MiB, a flow a
/// few of those. More comes only from a sidecar that is broken or was
/// taken over, and is not read into memory (security review).
pub const MAX_ANSWER_BYTES: usize = 128 * 1024 * 1024;

/// What a navigation has to reach its page when the request does not say:
/// the contract's default.
const DEFAULT_NAVIGATE_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest dwell a persona draws for itself: the sidecar's own cap
/// (spec §10.4).
const HUMAN_DWELL_CAP: Duration = Duration::from_secs(45);

/// What a navigation may take beyond its own times: the flick that is
/// under way when the patience ends, the captures, and the way there and
/// back.
const NAVIGATION_MARGIN: Duration = Duration::from_secs(30);

/// The longest a navigation takes when everything in it takes as long as
/// it may: reaching the page (`timeout_ms`), the scrolling (the patience
/// the request gives it, when it gives one) and the dwell, and a margin.
///
/// It is derived from the request and from nothing else. A visit of a page
/// is never ended by a time that is the same for every page (follow-up
/// F44): whoever waits for a navigation waits this long, the client itself
/// included.
pub fn navigation_bound(request: &NavigateRequest) -> Duration {
    let reach = request
        .timeout_ms
        .map_or(DEFAULT_NAVIGATE_TIMEOUT, Duration::from_millis);
    let fixed_dwell = Duration::from_millis(request.dwell_ms.unwrap_or(0));
    let on_the_page = request.behavior.as_ref().map_or(fixed_dwell, |behavior| {
        let patience = Duration::from_millis(behavior.patience_ms.unwrap_or(0));
        let dwell = match behavior.dwell {
            Some(DwellMode::Human) => HUMAN_DWELL_CAP,
            Some(DwellMode::Fixed) | None => fixed_dwell,
        };
        patience + dwell
    });
    reach + on_the_page + NAVIGATION_MARGIN
}

/// The operations of the contract this client calls: all of them. The
/// contract test compares the list with the document, so an endpoint added
/// to the contract is noticed here.
pub const IMPLEMENTED_OPERATIONS: &[&str] = &[
    "health",
    "metrics",
    "generate_persona",
    "upgrade_persona",
    "open_context",
    "close_context",
    "navigate",
    "flow",
    "export_state",
    "import_state",
    "selftest",
];

/// How to reach one sidecar.
#[derive(Debug, Clone)]
pub struct SidecarSettings {
    /// The sidecar's base URL on the compose network.
    pub url: String,
    /// The shared secret.
    pub token: Secret,
    /// How long a connection may take.
    pub connect_timeout: Duration,
    /// How long one call may take, the navigation it runs included.
    pub timeout: Duration,
    /// How often a call is tried again when no connection could be made.
    pub connect_retries: u32,
}

impl SidecarSettings {
    /// The settings of an engine's sidecar from the configuration.
    pub fn from_config(config: &Config, engine: Engine) -> Self {
        let sidecar = &config.sidecar;
        Self {
            url: match engine {
                Engine::Chromium => sidecar.chromium_url.clone(),
                Engine::Firefox => sidecar.firefox_url.clone(),
            },
            token: sidecar.token.clone(),
            connect_timeout: sidecar.connect_timeout.get(),
            timeout: sidecar.timeout.get(),
            connect_retries: sidecar.connect_retries,
        }
    }
}

/// What closing a context found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// The context was open and is closed now.
    Closed,
    /// The sidecar did not know the context: it was closed before, or it
    /// closed itself after being idle. The same result for the caller.
    AlreadyGone,
}

/// A navigation after Kage's own check of its redirect chain.
#[derive(Debug, Clone, PartialEq)]
pub enum Navigation {
    /// Every hop passed the SSRF guard.
    Page(Box<PageResult>),
    /// A hop was refused. The attempt's outcome is `ssrf_denied`; the
    /// result is kept for the record with everything it captured removed.
    SsrfDenied {
        /// The refused hop.
        denied: DeniedHop,
        /// The page result without its captures.
        result: Box<PageResult>,
    },
}

/// A client for one sidecar container.
#[derive(Clone)]
pub struct SidecarClient {
    http: wreq::Client,
    base: Url,
    token: Secret,
    connect_retries: u32,
    max_answer: usize,
}

impl std::fmt::Debug for SidecarClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarClient")
            .field("base", &self.base.as_str())
            .field("token", &self.token)
            .field("connect_retries", &self.connect_retries)
            .field("max_answer", &self.max_answer)
            .finish_non_exhaustive()
    }
}

impl SidecarClient {
    /// Builds a client.
    ///
    /// # Errors
    ///
    /// [`BrowserError::Setup`] when the URL is not an `http` URL or the
    /// HTTP client cannot be built.
    pub fn new(settings: &SidecarSettings) -> Result<Self, BrowserError> {
        let base = Url::parse(&settings.url)
            .ok()
            .filter(|url| url.scheme() == "http" && url.host_str().is_some())
            .ok_or_else(|| {
                BrowserError::Setup(format!("{:?} is not an http:// URL", settings.url))
            })?;
        let http = wreq::Client::builder()
            // A sidecar is reached directly on the compose network, never
            // through a proxy, and never through one named in the environment.
            .no_proxy()
            .user_agent(USER_AGENT)
            .connect_timeout(settings.connect_timeout)
            .timeout(settings.timeout)
            .build()
            .map_err(|error| {
                BrowserError::Setup(format!("cannot build the HTTP client: {error}"))
            })?;
        Ok(Self {
            http,
            base,
            token: settings.token.clone(),
            connect_retries: settings.connect_retries,
            max_answer: MAX_ANSWER_BYTES,
        })
    }

    /// The same client, reading at most `bytes` of an answer instead of
    /// [`MAX_ANSWER_BYTES`].
    #[must_use]
    pub const fn with_answer_limit(mut self, bytes: usize) -> Self {
        self.max_answer = bytes;
        self
    }

    /// Reads an answer up to the client's limit.
    async fn read_answer(
        &self,
        operation: &'static str,
        response: wreq::Response,
    ) -> Result<(u16, Vec<u8>), BrowserError> {
        let status = response.status().as_u16();
        let too_long = || BrowserError::Unexpected {
            operation,
            status,
            detail: format!("the answer is longer than {} bytes", self.max_answer),
        };
        let declared = response.content_length().unwrap_or(0);
        if usize::try_from(declared).map_or(true, |declared| declared > self.max_answer) {
            return Err(too_long());
        }
        let mut body = Box::pin(response);
        let mut bytes = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = match frame {
                Ok(frame) => frame,
                Err(error) if error.is_timeout() => {
                    return Err(BrowserError::Timeout { operation });
                }
                Err(source) => return Err(BrowserError::Transport { operation, source }),
            };
            // Trailers carry no content.
            let Ok(data) = frame.into_data() else {
                continue;
            };
            if data.len() > self.max_answer - bytes.len() {
                // Dropping the response abandons the rest of it.
                return Err(too_long());
            }
            bytes.extend_from_slice(&data);
        }
        Ok((status, bytes))
    }

    /// The sidecar's base URL.
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    fn url(&self, path: &str) -> Result<Url, BrowserError> {
        self.base.join(path).map_err(|error| {
            BrowserError::Setup(format!("cannot build the URL of {path}: {error}"))
        })
    }

    /// Sends one request, trying again only while no connection could be
    /// made. Returns the status and the body.
    async fn send(
        &self,
        operation: &'static str,
        method: wreq::Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<(u16, Vec<u8>), BrowserError> {
        self.send_within(operation, method, path, body, None).await
    }

    /// [`Self::send`] with a time of its own for this one call in place of
    /// the client's.
    async fn send_within(
        &self,
        operation: &'static str,
        method: wreq::Method,
        path: &str,
        body: Option<Vec<u8>>,
        within: Option<Duration>,
    ) -> Result<(u16, Vec<u8>), BrowserError> {
        let url = self.url(path)?;
        let mut backoff = FIRST_BACKOFF;
        let mut attempts = 0;
        loop {
            attempts += 1;
            let mut request = self
                .http
                .request(method.clone(), url.as_str())
                .header(TOKEN_HEADER, self.token.expose());
            if let Some(traceparent) = TRACE_CONTEXT.get().and_then(|source| source()) {
                request = request.header("traceparent", traceparent);
            }
            if let Some(within) = within {
                request = request.timeout(within);
            }
            if let Some(body) = &body {
                request = request
                    .header("content-type", "application/json")
                    .body(body.clone());
            }
            let failure = match request.send().await {
                Ok(response) => return self.read_answer(operation, response).await,
                Err(error) => error,
            };
            if failure.is_connect() {
                // Nothing was sent: the call may be made again.
                if attempts > self.connect_retries {
                    return Err(BrowserError::Connect {
                        url: self.base.to_string(),
                        attempts,
                        source: failure,
                    });
                }
                tracing::warn!(
                    operation,
                    attempt = attempts,
                    error = %failure,
                    "the sidecar could not be reached; trying again"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
            return Err(if failure.is_timeout() {
                BrowserError::Timeout { operation }
            } else {
                BrowserError::Transport {
                    operation,
                    source: failure,
                }
            });
        }
    }

    /// Turns an answer that is not a success into the error it carries.
    fn refusal(operation: &'static str, status: u16, body: &[u8]) -> BrowserError {
        match serde_json::from_slice::<ErrorResponse>(body) {
            Ok(error) => BrowserError::Refused {
                operation,
                status,
                code: error.code,
                message: error.message,
            },
            Err(error) => BrowserError::Unexpected {
                operation,
                status,
                detail: error.to_string(),
            },
        }
    }

    /// The JSON body of a request.
    fn body<Request: Serialize>(
        operation: &'static str,
        request: &Request,
    ) -> Result<Vec<u8>, BrowserError> {
        serde_json::to_vec(request).map_err(|error| {
            BrowserError::Setup(format!("cannot write the body of {operation}: {error}"))
        })
    }

    async fn call<Request: Serialize + Sync, Response: DeserializeOwned>(
        &self,
        operation: &'static str,
        method: wreq::Method,
        path: &str,
        request: Option<&Request>,
    ) -> Result<Response, BrowserError> {
        let body = request
            .map(|request| Self::body(operation, request))
            .transpose()?;
        let (status, bytes) = self.send(operation, method, path, body).await?;
        if !(200..300).contains(&status) {
            return Err(Self::refusal(operation, status, &bytes));
        }
        serde_json::from_slice(&bytes).map_err(|error| BrowserError::Unexpected {
            operation,
            status,
            detail: error.to_string(),
        })
    }

    async fn post<Request: Serialize + Sync, Response: DeserializeOwned>(
        &self,
        operation: &'static str,
        path: &str,
        request: &Request,
    ) -> Result<Response, BrowserError> {
        self.call(operation, wreq::Method::POST, path, Some(request))
            .await
    }

    /// `GET /v1/health`: engine, browser version, contexts in use, uptime.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`].
    pub async fn health(&self) -> Result<HealthResponse, BrowserError> {
        self.call::<(), _>("health", wreq::Method::GET, "/v1/health", None)
            .await
    }

    /// `GET /v1/metrics`: the sidecar's Prometheus metrics as text.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`].
    pub async fn metrics(&self) -> Result<String, BrowserError> {
        let (status, bytes) = self
            .send("metrics", wreq::Method::GET, "/v1/metrics", None)
            .await?;
        if !(200..300).contains(&status) {
            return Err(Self::refusal("metrics", status, &bytes));
        }
        String::from_utf8(bytes).map_err(|error| BrowserError::Unexpected {
            operation: "metrics",
            status,
            detail: error.to_string(),
        })
    }

    /// `POST /v1/personas/generate`: a fingerprint from a seed and
    /// constraints. The same seed gives the same fingerprint.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`]; `cross_os_claim_disabled` when the engine may
    /// only claim the operating system it runs on.
    pub async fn generate_persona(
        &self,
        request: &GeneratePersonaRequest,
    ) -> Result<GeneratePersonaResponse, BrowserError> {
        self.post("generate_persona", "/v1/personas/generate", request)
            .await
    }

    /// `POST /v1/personas/upgrade` (contract 1.4.0): moves a stored
    /// fingerprint to the browser major the sidecar runs, as a browser's own
    /// update moves it; a document of that major comes back unchanged with
    /// `upgraded` false.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`]; `browser_major_mismatch` when the document is of
    /// a newer major than the sidecar's browser.
    pub async fn upgrade_persona(
        &self,
        request: &UpgradePersonaRequest,
    ) -> Result<UpgradePersonaResponse, BrowserError> {
        self.post("upgrade_persona", "/v1/personas/upgrade", request)
            .await
    }

    /// `POST /v1/contexts`: opens the persona's context, or returns the one
    /// that is open.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`]; `context_limit` when the container is full.
    pub async fn open_context(
        &self,
        request: &OpenContextRequest,
    ) -> Result<OpenContextResponse, BrowserError> {
        self.post("open_context", "/v1/contexts", request).await
    }

    /// `DELETE /v1/contexts/{id}`: closes a context and flushes its profile.
    /// A context the sidecar no longer knows counts as closed.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`]; `context_busy` while an operation runs on it.
    pub async fn close_context(&self, context: Uuid) -> Result<Closed, BrowserError> {
        let path = format!("/v1/contexts/{context}");
        let (status, bytes) = self
            .send("close_context", wreq::Method::DELETE, &path, None)
            .await?;
        if (200..300).contains(&status) {
            return Ok(Closed::Closed);
        }
        match Self::refusal("close_context", status, &bytes) {
            error if error.is_context_gone() => Ok(Closed::AlreadyGone),
            error => Err(error),
        }
    }

    /// `POST /v1/contexts/{id}/navigate`: loads a page and captures it.
    ///
    /// The result is what the browser did, unchecked. Use
    /// [`Self::navigate_checked`] unless the redirect chain is checked
    /// elsewhere.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`]. A navigation that did not arrive is not an
    /// error: the page result says why.
    pub async fn navigate(
        &self,
        context: Uuid,
        request: &NavigateRequest,
    ) -> Result<PageResult, BrowserError> {
        const OPERATION: &str = "navigate";
        let span = tracing::info_span!(
            "sidecar.navigate",
            otel.kind = "client",
            context = %context,
            outcome = tracing::field::Empty,
            status = tracing::field::Empty,
        );
        let answered = async {
            // A navigation is waited for as long as this one may take, not
            // as long as every call may: a page a persona has the patience
            // to read for minutes is not cut off at the client's own timeout.
            let path = format!("/v1/contexts/{context}/navigate");
            let body = Self::body(OPERATION, request)?;
            let (status, bytes) = self
                .send_within(
                    OPERATION,
                    wreq::Method::POST,
                    &path,
                    Some(body),
                    Some(navigation_bound(request)),
                )
                .await?;
            if !(200..300).contains(&status) {
                return Err(Self::refusal(OPERATION, status, &bytes));
            }
            serde_json::from_slice::<PageResult>(&bytes).map_err(|error| BrowserError::Unexpected {
                operation: OPERATION,
                status,
                detail: error.to_string(),
            })
        }
        .instrument(span.clone())
        .await;
        match &answered {
            Ok(page) => {
                span.record(
                    "outcome",
                    page.error
                        .as_ref()
                        .map_or("ok", |error| error.code.as_str()),
                );
                if let Some(status) = page.status {
                    span.record("status", status);
                }
            }
            Err(error) => {
                span.record("outcome", error.kind());
            }
        }
        answered
    }

    /// Navigates, then checks every hop the browser followed against the
    /// SSRF guard (spec §19). A browser follows redirects by itself, so the
    /// check can only come afterwards; a refused hop turns the result into
    /// [`Navigation::SsrfDenied`] and removes everything it captured.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`].
    pub async fn navigate_checked(
        &self,
        context: Uuid,
        request: &NavigateRequest,
        policy: SsrfPolicy,
        max_redirects: usize,
    ) -> Result<Navigation, BrowserError> {
        let mut result = self.navigate(context, request).await?;
        Ok(match ssrf::check_chain(&result, policy, max_redirects) {
            Ok(()) => Navigation::Page(Box::new(result)),
            Err(denied) => {
                tracing::warn!(
                    context = %context,
                    hop = denied.hop,
                    reason = %denied.reason,
                    "a browser navigation followed a hop the SSRF guard refuses"
                );
                ssrf::drop_captures(&mut result);
                Navigation::SsrfDenied {
                    denied,
                    result: Box::new(result),
                }
            }
        })
    }

    /// `POST /v1/contexts/{id}/flow`: runs a scripted flow and returns its
    /// events in order, one [`FlowEvent::Step`] per finished step and then
    /// exactly one [`FlowEvent::Final`].
    ///
    /// The sidecar answers with one line of JSON per event, written as the
    /// steps finish; the whole answer is read before anything is returned.
    /// Like [`Self::navigate`], the page results in the events are what the
    /// browser did, unchecked: [`ssrf::check_chain`] reads each of them as
    /// it reads a navigation's.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`]. A flow refused before its first step is
    /// [`BrowserError::Refused`] like any other call, for example with
    /// `context_busy`. [`BrowserError::FlowInterrupted`] when the answer
    /// ends without its final event: the flow broke after the sidecar had
    /// begun to answer, and the error carries the steps that did finish.
    /// A step that failed is not an error: its event and the final one say
    /// so.
    pub async fn flow(
        &self,
        context: Uuid,
        request: &FlowRequest,
    ) -> Result<Vec<FlowEvent>, BrowserError> {
        let span = tracing::info_span!(
            "sidecar.flow",
            otel.kind = "client",
            context = %context,
            outcome = tracing::field::Empty,
        );
        let answered = async {
            let body = Self::body("flow", request)?;
            let (status, bytes) = self
                .send(
                    "flow",
                    wreq::Method::POST,
                    &format!("/v1/contexts/{context}/flow"),
                    Some(body),
                )
                .await?;
            if !(200..300).contains(&status) {
                return Err(Self::refusal("flow", status, &bytes));
            }
            flow_events(status, &bytes)
        }
        .instrument(span.clone())
        .await;
        let outcome = match &answered {
            Ok(events) => match events.last() {
                Some(FlowEvent::Final { ok: true, .. }) => "ok",
                _ => "failed",
            },
            Err(error) => error.kind(),
        };
        span.record("outcome", outcome);
        answered
    }

    /// `POST /v1/contexts/{id}/state/export`: cookies and storage of an origin.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`].
    pub async fn export_state(
        &self,
        context: Uuid,
        request: &ExportStateRequest,
    ) -> Result<ExportStateResponse, BrowserError> {
        self.post(
            "export_state",
            &format!("/v1/contexts/{context}/state/export"),
            request,
        )
        .await
    }

    /// `POST /v1/contexts/{id}/state/import`: puts cookies of an origin
    /// into the context.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`].
    pub async fn import_state(
        &self,
        context: Uuid,
        request: &ImportStateRequest,
    ) -> Result<ImportStateResponse, BrowserError> {
        self.post(
            "import_state",
            &format!("/v1/contexts/{context}/state/import"),
            request,
        )
        .await
    }

    /// `POST /v1/contexts/{id}/selftest`: loads the detection page set and
    /// returns findings and a score.
    ///
    /// # Errors
    ///
    /// See [`BrowserError`].
    pub async fn selftest(
        &self,
        context: Uuid,
        request: &SelftestRequest,
    ) -> Result<SelftestResponse, BrowserError> {
        self.post(
            "selftest",
            &format!("/v1/contexts/{context}/selftest"),
            request,
        )
        .await
    }
}

/// Reads the answer of a flow: one event per line, the steps first and one
/// final event last.
fn flow_events(status: u16, body: &[u8]) -> Result<Vec<FlowEvent>, BrowserError> {
    let unexpected = |detail: String| BrowserError::Unexpected {
        operation: "flow",
        status,
        detail,
    };
    let mut events = Vec::new();
    let mut finished = false;
    for (index, line) in body.split(|byte| *byte == b'\n').enumerate() {
        // A line may end in `\r\n`, and the body may end with a newline or
        // without one.
        let line = line.trim_ascii();
        if line.is_empty() {
            continue;
        }
        let number = index + 1;
        if finished {
            return Err(unexpected(format!("line {number} follows the final event")));
        }
        let event: FlowEvent = serde_json::from_slice(line)
            .map_err(|error| unexpected(format!("line {number} is not a flow event: {error}")))?;
        finished = matches!(event, FlowEvent::Final { .. });
        events.push(event);
    }
    if finished {
        Ok(events)
    } else {
        Err(BrowserError::FlowInterrupted { steps: events })
    }
}
