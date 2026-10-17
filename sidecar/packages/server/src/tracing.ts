/**
 * Traces of the sidecar (spec §20). Kage's request to the sidecar carries
 * the W3C `traceparent` of its own span; the sidecar's spans for a
 * navigation or a flow are its children, so that one trace runs from Kage's
 * API to the browser. The header is read from Kage's request on the compose
 * network and from nowhere else; nothing here touches what the browsers
 * send, and nothing is patched: the spans are made by hand.
 *
 * Without an endpoint no OpenTelemetry package is loaded and nothing is
 * sent; the lines of a request still carry Kage's trace id.
 */
import { AsyncLocalStorage } from 'node:async_hooks';
import { randomBytes } from 'node:crypto';

/** A span of Kage's that a request names as its parent. */
export interface TraceParent {
  readonly traceId: string;
  readonly spanId: string;
  readonly sampled: boolean;
}

const TRACEPARENT = /^00-([0-9a-f]{32})-([0-9a-f]{16})-([0-9a-f]{2})$/;

/**
 * Reads a `traceparent` header of version 00. Anything else, an id of
 * zeros included, is no parent: the span begins a trace of its own.
 */
export function parseTraceparent(header: string | string[] | undefined): TraceParent | null {
  const value = Array.isArray(header) ? header[0] : header;
  const match = TRACEPARENT.exec((value ?? '').trim());
  if (match === null) {
    return null;
  }
  const [, traceId = '', spanId = '', flags = '00'] = match;
  if (/^0+$/.test(traceId) || /^0+$/.test(spanId)) {
    return null;
  }
  return { traceId, spanId, sampled: (Number.parseInt(flags, 16) & 1) === 1 };
}

/** What a span may be told: no address, no URL, no secret. */
export type SpanAttributes = Readonly<Record<string, string | number | boolean>>;

/** A span under way. */
export interface ActiveSpan {
  /** The trace it belongs to: its parent's, or a new one. */
  readonly traceId: string;
  /** Its own id. */
  readonly spanId: string;
  /** Adds to what the span says. */
  set(attributes: SpanAttributes): void;
  /** Ends it; `failed` marks it as an error. */
  end(failed?: boolean): void;
}

/** Makes the sidecar's spans. */
export interface Tracer {
  /** A server span for a request of Kage's, a child of `parent` when one came. */
  start(name: string, parent: TraceParent | null, attributes: SpanAttributes): ActiveSpan;
  /** Sends what is still queued; the tracer makes no span after it. */
  shutdown(): Promise<void>;
}

const newId = (bytes: number) => randomBytes(bytes).toString('hex');

/**
 * The tracer when nothing is sent. A span takes the trace of its parent,
 * so that the lines of a request share Kage's trace id, and has an id of
 * its own, which nobody records.
 */
export const noTracer: Tracer = {
  start(_name, parent) {
    return {
      traceId: parent?.traceId ?? newId(16),
      spanId: newId(8),
      set() {
        // Nobody records the span.
      },
      end() {
        // Nothing to send.
      },
    };
  },
  shutdown: () => Promise.resolve(),
};

/** What the OTLP tracer needs. */
export interface OtlpOptions {
  /** The collector's address with its path, as `http://otel-collector:4318/v1/traces`. */
  readonly endpoint: string;
  /** The `service.name` of the spans. */
  readonly serviceName: string;
  /** Collects the finished spans in place of the exporter (tests). */
  readonly exporter?: unknown;
}

/**
 * The tracer that sends spans as OTLP over HTTP with protobuf, like Kage.
 * The five packages are loaded here and only here.
 */
export async function otlpTracer(options: OtlpOptions): Promise<Tracer> {
  const [api, core, resources, sdk, otlp] = await Promise.all([
    import('@opentelemetry/api'),
    import('@opentelemetry/core'),
    import('@opentelemetry/resources'),
    import('@opentelemetry/sdk-trace'),
    import('@opentelemetry/exporter-trace-otlp-proto'),
  ]);
  const exporter =
    (options.exporter as InstanceType<typeof sdk.InMemorySpanExporter> | undefined) ??
    new otlp.OTLPTraceExporter({ url: options.endpoint });
  const provider = new sdk.TracerProvider({
    // Literal attributes only: a detector would run processes to learn
    // about the host.
    resource: resources.resourceFromAttributes({ 'service.name': options.serviceName }),
    spanProcessors: [
      options.exporter === undefined
        ? new sdk.BatchSpanProcessor({ exporter })
        : new sdk.SimpleSpanProcessor({ exporter }),
    ],
  });
  const tracer = provider.getTracer('kage-sidecar');
  const propagator = new core.W3CTraceContextPropagator();
  return {
    start(name, parent, attributes) {
      const context =
        parent === null
          ? api.ROOT_CONTEXT
          : propagator.extract(
              api.ROOT_CONTEXT,
              {
                traceparent: `00-${parent.traceId}-${parent.spanId}-${parent.sampled ? '01' : '00'}`,
              },
              api.defaultTextMapGetter,
            );
      const span = tracer.startSpan(name, { kind: api.SpanKind.SERVER, attributes }, context);
      const ids = span.spanContext();
      return {
        traceId: ids.traceId,
        spanId: ids.spanId,
        set(more) {
          span.setAttributes(more);
        },
        end(failed = false) {
          if (failed) {
            span.setStatus({ code: api.SpanStatusCode.ERROR });
          }
          span.end();
        },
      };
    },
    async shutdown() {
      await provider.shutdown();
    },
  };
}

/** The tracer the configuration asks for. */
export function createTracer(endpoint: string | undefined, serviceName: string): Promise<Tracer> {
  return endpoint === undefined ? Promise.resolve(noTracer) : otlpTracer({ endpoint, serviceName });
}

/** The ids of the span a line of the log is written in. */
export interface TraceIds {
  readonly trace_id: string;
  readonly span_id: string;
}

/** The span the code that runs now is part of, for the log's lines. */
export const currentTrace = new AsyncLocalStorage<TraceIds>();

/**
 * What a log line adds: the ids of the span it is written in, unless the
 * logger has them already.
 */
export function traceFields(bound: Readonly<Record<string, unknown>>): Partial<TraceIds> {
  const ids = currentTrace.getStore();
  if (ids === undefined) {
    return {};
  }
  return 'trace_id' in bound ? { span_id: ids.span_id } : ids;
}
