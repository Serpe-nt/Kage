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
use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::{SpanContext, TraceContextExt, TracerProvider as _};
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::dispatcher::WeakDispatch;
use tracing::{Dispatch, Subscriber};
use tracing_opentelemetry::OpenTelemetrySpanExt;
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
        builder = builder.with_batch_exporter(exporter);
    }
    let provider = builder.build();
    let tracer = provider.tracer("kage");
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let dispatcher = Dispatcher::default();
    let lines = tracing_subscriber::fmt::layer()
        .json()
        .event_format(WithTraceIds::new(&dispatcher))
        .fmt_fields(JsonFields::new());
    let spans = tracing_opentelemetry::layer().with_tracer(tracer);
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

    use super::{Dispatcher, ToCollector, WithTraceIds, traceparent};

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
