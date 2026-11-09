//! Logs and traces (spec §20). The crates make their spans with `tracing`;
//! here they become OpenTelemetry spans, sent as OTLP over HTTP with
//! protobuf to the collector `telemetry.otlp_endpoint` names, through wreq
//! like every request Kage makes. Every JSON log line carries the
//! `trace_id` and `span_id` of the span it was written in, under the names
//! the sidecar's lines use, and a request to a sidecar carries its
//! `traceparent`. Without an endpoint the ids are made all the same and
//! nothing leaves the process.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use kage_core::config::TelemetryConfig;
use opentelemetry::KeyValue;
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{SpanContext, Status, TraceContextExt, TracerProvider as _};
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter, Tracer};
use tracing::dispatcher::WeakDispatch;
use tracing::{Dispatch, Subscriber};
use tracing_opentelemetry::{OpenTelemetryLayer, OpenTelemetrySpanExt};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::format::{Format, Json, JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, FormatEvent};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

/// The tracer of the process, kept to be shut down at the end: what is
/// still queued is sent then.
pub(crate) struct Telemetry {
    provider: SdkTracerProvider,
}

impl Telemetry {
    /// Sends what is still queued and stops the exporter. Blocking work,
    /// done off the runtime's threads: the export itself needs them.
    pub(crate) async fn shut_down(self) {
        let provider = self.provider;
        let ended = tokio::task::spawn_blocking(move || provider.shutdown()).await;
        if let Ok(Err(error)) = ended {
            tracing::warn!(%error, "the last traces could not be sent");
        }
    }

    /// Shuts down as [`Telemetry::shut_down`], within two seconds when
    /// `bounded`: a server that stopped for its database's lock exits
    /// within its bound (spec §16.6, QA round four lock-01).
    pub(crate) async fn shut_down_within(self, bounded: bool) {
        if bounded {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.shut_down()).await;
        } else {
            self.shut_down().await;
        }
    }
}

/// Installs the log subscriber and the tracer: JSON lines on stdout (or on
/// stderr for a command whose stdout is its answer), and spans to the
/// collector when the configuration names one. Must run inside the Tokio
/// runtime, whose handle the exporter keeps.
///
/// # Errors
///
/// When the exporter cannot be built or a subscriber is installed already.
pub(crate) fn install(to_stderr: bool, telemetry: &TelemetryConfig) -> anyhow::Result<Telemetry> {
    let mut builder = SdkTracerProvider::builder().with_resource(
        Resource::builder_empty()
            .with_service_name(telemetry.service_name.clone())
            .build(),
    );
    if let Some(endpoint) = &telemetry.otlp_endpoint {
        let timeout = telemetry.export_timeout.get();
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(ToCollector::new(timeout)?)
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(endpoint.clone())
            .with_timeout(timeout)
            .build()
            .map_err(|error| anyhow::anyhow!("the trace exporter cannot be built: {error}"))?;
        builder = builder.with_batch_exporter(EventsWithoutFields(exporter));
    }
    let provider = builder.build();
    let tracer = provider.tracer("kage");
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let dispatcher = Dispatcher::default();
    let lines = tracing_subscriber::fmt::layer()
        .json()
        .event_format(WithTraceIds::new(&dispatcher))
        .fmt_fields(JsonFields::new());
    let spans = spans_layer(tracer);
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(spans)
        .with(dispatcher);
    let installed = if to_stderr {
        registry.with(lines.with_writer(std::io::stderr)).try_init()
    } else {
        registry.with(lines).try_init()
    };
    installed.map_err(|error| anyhow::anyhow!("failed to install the log subscriber: {error}"))?;
    kage_browser::trace_context_from(traceparent);
    Ok(Telemetry { provider })
}

/// The layer that turns `tracing`'s spans into OpenTelemetry spans, with
/// the two settings of tracing-opentelemetry that would put a line's text
/// on its span, where [`EventsWithoutFields`] does not look, turned off:
/// `error_events_to_status` makes the text of a message-less line's
/// `error` field (`warn!(%error)`) the span's status description, and
/// `error_records_to_exceptions` adds a `&dyn Error` field's text and its
/// sources to the span as `exception.message` and `exception.stacktrace`
/// (F196). An `ERROR` line still sets the span's error status.
fn spans_layer<S>(tracer: Tracer) -> OpenTelemetryLayer<S, Tracer>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_location(true)
        .with_error_events_to_status(false)
        .with_error_records_to_exceptions(false)
}

/// The W3C `traceparent` of the current span, when it has a trace.
pub(crate) fn traceparent() -> Option<String> {
    let context = tracing::Span::current().context();
    let mut carrier = HashMap::new();
    TraceContextPropagator::new().inject_context(&context, &mut carrier);
    carrier.remove("traceparent")
}

/// The dispatcher the subscriber is installed in, caught when it is: a
/// formatter runs inside it and cannot ask for the default one, which
/// tracing hides while it dispatches.
#[derive(Clone, Default)]
struct Dispatcher(Arc<OnceLock<WeakDispatch>>);

impl<S: Subscriber> Layer<S> for Dispatcher {
    fn on_register_dispatch(&self, dispatch: &Dispatch) {
        // A subscriber is registered once.
        let _ = self.0.set(dispatch.downgrade());
    }
}

/// Tracing's JSON lines with the trace and span ids of the event's span in
/// front: `{"trace_id":"…","span_id":"…",…}`.
struct WithTraceIds {
    format: Format<Json>,
    dispatcher: Dispatcher,
}

impl WithTraceIds {
    fn new(dispatcher: &Dispatcher) -> Self {
        Self {
            format: tracing_subscriber::fmt::format().json(),
            dispatcher: dispatcher.clone(),
        }
    }

    /// The ids of the span an event was written in, when it has a trace.
    fn ids_of<S>(&self, ctx: &FmtContext<'_, S, JsonFields>) -> Option<SpanContext>
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let span = ctx.parent_span()?;
        let dispatch = self.dispatcher.0.get()?.upgrade()?;
        let context = tracing_opentelemetry::get_otel_context(&span.id(), &dispatch)?;
        let ids = context.span().span_context().clone();
        ids.is_valid().then_some(ids)
    }
}

impl<S> FormatEvent<S, JsonFields> for WithTraceIds
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, JsonFields>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> fmt::Result {
        let Some(ids) = self.ids_of(ctx) else {
            return self.format.format_event(ctx, writer, event);
        };
        let mut line = String::new();
        self.format
            .format_event(ctx, Writer::new(&mut line), event)?;
        match line.strip_prefix('{') {
            Some(rest) => write!(
                writer,
                "{{\"trace_id\":\"{}\",\"span_id\":\"{}\",{rest}",
                ids.trace_id(),
                ids.span_id()
            ),
            None => writer.write_str(&line),
        }
    }
}

/// The attributes tracing-opentelemetry writes first on every span event,
/// before any field of the line: how loud it was and where it came from.
const HEAD_ATTRIBUTES: [&str; 2] = ["level", "target"];

/// The attributes tracing-opentelemetry writes last on every span event,
/// after every field of the line: where it was written.
const TAIL_ATTRIBUTES: [&str; 3] = ["code.file.path", "code.module.name", "code.line.number"];

/// The levels a span event's `level` can name.
const LEVELS: [&str; 5] = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];

/// The name every span event leaves under.
const EVENT_NAME: &str = "log";

/// The guard of spec §20 for span events (F196): tracing-opentelemetry
/// turns every log line written inside a span into an event of that span,
/// with the line's message as its name and its fields as attributes, so a
/// line that names a seed, a URL's path, an address or a token would leave
/// the process in a trace. This exporter strips each event to the
/// attributes tracing-opentelemetry writes itself under [`EVENT_NAME`]
/// before the spans are sent, whatever the line held; the line itself, with
/// the span's `trace_id` and `span_id`, is in the process's log. A line's
/// own field may carry one of those names (`target = %url`), so a key is
/// kept only where the layer writes it: [`HEAD_ATTRIBUTES`] at its first
/// occurrence (the layer writes them before the fields), `level` only when
/// it names a level, and [`TAIL_ATTRIBUTES`] at its last (the layer writes
/// them after the fields, [`spans_layer`] keeps the location on); a field of
/// the same name is dropped. Spans keep their own fields and the error
/// status an `ERROR` line sets, without a description: whatever wrote one
/// wrote a line's or an error's text.
#[derive(Debug)]
struct EventsWithoutFields<E>(E);

impl<E> EventsWithoutFields<E> {
    fn strip(batch: &mut [SpanData]) {
        for span in batch {
            if let Status::Error { description } = &span.status
                && !description.is_empty()
            {
                span.status = Status::error("");
            }
            for event in &mut span.events.events {
                event.name = std::borrow::Cow::Borrowed(EVENT_NAME);
                let before = event.attributes.len();
                event.attributes = Self::kept(std::mem::take(&mut event.attributes));
                let dropped = before - event.attributes.len();
                event.dropped_attributes_count = event
                    .dropped_attributes_count
                    .saturating_add(u32::try_from(dropped).unwrap_or(u32::MAX));
            }
        }
    }

    /// The attributes of one event that the layer wrote itself.
    fn kept(attributes: Vec<KeyValue>) -> Vec<KeyValue> {
        let last_of = |key: &str| {
            attributes
                .iter()
                .rposition(|attribute| attribute.key.as_str() == key)
        };
        let tail: Vec<Option<usize>> = TAIL_ATTRIBUTES.iter().map(|key| last_of(key)).collect();
        let mut head_seen = [false; HEAD_ATTRIBUTES.len()];
        attributes
            .into_iter()
            .enumerate()
            .filter(|(index, attribute)| {
                let key = attribute.key.as_str();
                if let Some(at) = HEAD_ATTRIBUTES.iter().position(|head| *head == key) {
                    let first = !head_seen[at];
                    head_seen[at] = true;
                    return first
                        && (key != "level" || LEVELS.contains(&attribute.value.as_str().as_ref()));
                }
                TAIL_ATTRIBUTES
                    .iter()
                    .position(|tail_key| *tail_key == key)
                    .is_some_and(|at| tail[at] == Some(*index))
            })
            .map(|(_, attribute)| attribute)
            .collect()
    }
}

impl<E: SpanExporter> SpanExporter for EventsWithoutFields<E> {
    fn export(&self, mut batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        Self::strip(&mut batch);
        self.0.export(batch)
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

/// The exporter's HTTP client: wreq, directly, never through a proxy. The
/// exporter runs on a thread of its own outside the runtime, and wreq
/// needs the runtime, so every request is spawned on it.
struct ToCollector {
    client: wreq::Client,
    runtime: tokio::runtime::Handle,
    timeout: Duration,
}

impl fmt::Debug for ToCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToCollector")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl ToCollector {
    fn new(timeout: Duration) -> anyhow::Result<Self> {
        let client = wreq::Client::builder()
            .no_proxy()
            .timeout(timeout)
            .build()
            .map_err(|error| {
                anyhow::anyhow!("the client of the collector cannot be built: {error}")
            })?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| anyhow::anyhow!("the trace exporter needs the Tokio runtime"))?;
        Ok(Self {
            client,
            runtime,
            timeout,
        })
    }
}

#[async_trait]
impl HttpClient for ToCollector {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let client = self.client.clone();
        let timeout = self.timeout;
        let sent = self.runtime.spawn(async move {
            let response =
                tokio::time::timeout(timeout, client.execute(wreq::Request::from(request)))
                    .await??;
            let mut answer = Response::builder().status(response.status());
            for (name, value) in response.headers() {
                answer = answer.header(name, value);
            }
            let body = tokio::time::timeout(timeout, response.bytes()).await??;
            Ok::<_, HttpError>(answer.body(body)?)
        });
        sent.await?
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::fmt::format::JsonFields;
    use tracing_subscriber::layer::SubscriberExt;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        Dispatcher, EventsWithoutFields, ToCollector, WithTraceIds, spans_layer, traceparent,
    };

    /// The lines a test subscriber wrote.
    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);

    impl Lines {
        fn read(&self) -> Vec<serde_json::Value> {
            let bytes = self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            String::from_utf8(bytes)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    impl io::Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Lines {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// The spans a test tracer finished.
    #[derive(Debug, Clone, Default)]
    struct Spans(Arc<Mutex<Vec<SpanData>>>);

    impl SpanExporter for Spans {
        fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(batch);
            std::future::ready(Ok(()))
        }
    }

    #[test]
    fn outside_a_trace_there_is_no_traceparent() {
        assert_eq!(traceparent(), None);
    }

    #[test]
    fn a_line_carries_the_ids_of_its_span_and_a_sidecar_is_told_the_same_trace() {
        let spans = Spans::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(spans.clone())
            .build();
        let lines = Lines::default();
        let dispatcher = Dispatcher::default();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")))
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .event_format(WithTraceIds::new(&dispatcher))
                    .fmt_fields(JsonFields::new())
                    .with_writer(lines.clone()),
            )
            .with(dispatcher);
        let header = tracing::subscriber::with_default(subscriber, || {
            tracing::info!("before any request");
            let request = tracing::info_span!("api.request", route = "/v1/fetch");
            let _request = request.enter();
            tracing::info!(answer = 42, "in the request");
            let navigate = tracing::info_span!("sidecar.navigate", otel.kind = "client");
            let _navigate = navigate.enter();
            traceparent()
        })
        .unwrap();

        let written = lines.read();
        assert_eq!(written.len(), 2);
        // Outside a span: tracing's line as it was.
        assert!(written[0].get("trace_id").is_none());
        assert_eq!(written[0]["fields"]["message"], "before any request");
        // Inside: the ids in front, everything else as before.
        let inside = &written[1];
        assert_eq!(inside["fields"]["answer"], 42);
        let trace_id = inside["trace_id"].as_str().unwrap();
        let span_id = inside["span_id"].as_str().unwrap();
        assert_eq!((trace_id.len(), span_id.len()), (32, 16));

        // The sidecar is told the request's trace, with the client's span as
        // the parent of its own.
        let parts: Vec<&str> = header.split('-').collect();
        assert_eq!(parts.len(), 4, "{header}");
        assert_eq!((parts[0], parts[1], parts[3]), ("00", trace_id, "01"));
        assert_ne!(parts[2], span_id);

        let finished = spans
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let named = |name: &str| {
            finished
                .iter()
                .find(|span| span.name == name)
                .unwrap_or_else(|| panic!("no span {name}: {finished:?}"))
        };
        let (request, navigate) = (named("api.request"), named("sidecar.navigate"));
        assert_eq!(request.span_context.span_id().to_string(), span_id);
        assert_eq!(navigate.span_context.span_id().to_string(), parts[2]);
        assert_eq!(navigate.parent_span_id, request.span_context.span_id());
        assert_eq!(
            navigate.span_context.trace_id(),
            request.span_context.trace_id()
        );
    }

    /// An error whose text and source name what no trace may hold.
    #[derive(Debug)]
    struct Refused(io::Error);

    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "refused at /private/inbox")
        }
    }

    impl std::error::Error for Refused {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    /// What the lines of a careless request below say that no trace may
    /// hold.
    const SECRETS: [&str; 8] = [
        "9f2c41d07be1a3c5",
        "/account",
        "kage_secret",
        "203.0.113.9",
        "198.51.100.7",
        "/private/inbox",
        "192.0.2.44",
        "field-named-target",
    ];

    /// The spans one request makes when its lines name a seed, a path, an
    /// address and a token, as a message, as fields, as a message-less
    /// line's error, as a `&dyn Error` and as fields named like the ones
    /// the layer writes itself; exported through `exporter` from the
    /// server's layer when `guarded`, else from tracing-opentelemetry's
    /// default one.
    fn spans_of_a_careless_request<E: SpanExporter + 'static>(exporter: E, guarded: bool) {
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();
        let tracer = provider.tracer("test");
        let run = || {
            let span = tracing::info_span!("attempt", attempt = 1, level = 3);
            let _span = span.enter();
            tracing::info!(
                seed = "9f2c41d07be1a3c5",
                path = "/account/settings?id=7",
                token = "kage_secret",
                "reached 203.0.113.9:8080 for /account/settings"
            );
            let error = io::Error::other("no route to 198.51.100.7");
            tracing::warn!(%error);
            let refused = Refused(io::Error::other("reset by 192.0.2.44"));
            let refused: &(dyn std::error::Error + 'static) = &refused;
            tracing::warn!(error = refused, "the proxy refused");
            tracing::info!(
                target = "field-named-target",
                level = "kage_secret",
                code.file.path = "/account/settings",
                code.line.number = "203.0.113.9",
                "fields named like the layer's own"
            );
            tracing::error!(error = "refused by 203.0.113.9", "the attempt failed");
        };
        if guarded {
            let subscriber = tracing_subscriber::registry().with(spans_layer(tracer));
            tracing::subscriber::with_default(subscriber, run);
        } else {
            let subscriber = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(tracer));
            tracing::subscriber::with_default(subscriber, run);
        }
    }

    #[test]
    fn a_span_event_leaves_with_its_level_and_target_only() {
        // F196: the rule of spec §20 holds whatever a line in a span says.
        let spans = Spans::default();
        spans_of_a_careless_request(EventsWithoutFields(spans.clone()), true);
        let finished = spans
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_eq!(finished.len(), 1);
        let attempt = &finished[0];
        let sent = format!("{attempt:?}");
        for secret in SECRETS {
            assert!(!sent.contains(secret), "{secret} left in {sent}");
        }
        assert_eq!(attempt.events.events.len(), 5);
        for event in &attempt.events.events {
            assert_eq!(event.name, "log");
            let keys: Vec<&str> = event
                .attributes
                .iter()
                .map(|attribute| attribute.key.as_str())
                .collect();
            assert_eq!(
                keys,
                [
                    "level",
                    "target",
                    "code.file.path",
                    "code.module.name",
                    "code.line.number"
                ],
                "{event:?}"
            );
            let value = |at: usize| event.attributes[at].value.as_str().into_owned();
            assert!(super::LEVELS.contains(&value(0).as_str()), "{event:?}");
            assert_eq!(value(1), module_path!());
            assert!(value(2).ends_with("telemetry.rs"), "{event:?}");
            assert!(event.dropped_attributes_count > 0);
        }
        // Nothing was added to the span from a line's error.
        assert!(
            attempt
                .attributes
                .iter()
                .all(|attribute| !attribute.key.as_str().starts_with("exception.")),
            "{attempt:?}"
        );
        // The span keeps its own fields and the status an ERROR line set.
        assert!(
            attempt
                .attributes
                .iter()
                .any(|attribute| attribute.key.as_str() == "attempt")
        );
        assert_eq!(attempt.status, opentelemetry::trace::Status::error(""));
    }

    #[test]
    fn a_status_description_never_leaves() {
        // F196: whatever wrote it, the guard sends an error status bare.
        let spans = Spans::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(EventsWithoutFields(spans.clone()))
            .build();
        let subscriber = tracing_subscriber::registry().with(spans_layer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::info_span!(
                "attempt",
                otel.status_code = "ERROR",
                otel.status_description = "refused by 203.0.113.9"
            )
            .entered();
        });
        let finished = spans
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].status, opentelemetry::trace::Status::error(""));
    }

    #[test]
    fn without_the_guard_a_line_in_a_span_would_leave_with_it() {
        // The negative control of the test above: tracing-opentelemetry as
        // it is sends a line's message and fields as a span event, a
        // message-less line's error as the span's status description and a
        // `&dyn Error` field as the span's `exception.message`.
        let spans = Spans::default();
        spans_of_a_careless_request(spans.clone(), false);
        let finished = spans
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let sent = format!("{finished:?}");
        for secret in SECRETS {
            assert!(sent.contains(secret), "{secret} not in {sent}");
        }
        let attempt = &finished[0];
        let opentelemetry::trace::Status::Error { description } = &attempt.status else {
            panic!("{attempt:?}");
        };
        assert!(description.contains("198.51.100.7"), "{description}");
        assert!(attempt.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "exception.message"
                && attribute.value.as_str().contains("/private/inbox")
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spans_reach_the_collector_as_protobuf_through_wreq() {
        let collector = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/traces"))
            .and(header("content-type", "application/x-protobuf"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&collector)
            .await;
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(ToCollector::new(Duration::from_secs(5)).unwrap())
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(format!("{}/v1/traces", collector.uri()))
            .with_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        // The batch processor runs on a thread of its own, outside the
        // runtime, as in the server.
        let provider = SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .build();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_opentelemetry::layer().with_tracer(provider.tracer("test")));
        tracing::subscriber::with_default(subscriber, || {
            let _span = tracing::info_span!("api.request").entered();
        });
        let shut = tokio::task::spawn_blocking(move || provider.shutdown())
            .await
            .unwrap();
        assert!(shut.is_ok(), "{shut:?}");
        collector.verify().await;
        assert!(format!("{:?}", ToCollector::new(Duration::from_secs(1)).unwrap()).contains("1s"));
    }
}
