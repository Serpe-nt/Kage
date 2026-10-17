import { createServer, type IncomingHttpHeaders } from 'node:http';
import type { AddressInfo } from 'node:net';

import { InMemorySpanExporter } from '@opentelemetry/sdk-trace';
import { describe, expect, it } from 'vitest';

import {
  createTracer,
  currentTrace,
  noTracer,
  otlpTracer,
  parseTraceparent,
  traceFields,
} from './tracing.js';

const TRACE = '4bf92f3577b34da6a3ce929d0e0e4736';
const PARENT = '00f067aa0ba902b7';
const HEADER = `00-${TRACE}-${PARENT}-01`;

describe('a traceparent', () => {
  it('is read when it is W3C version 00', () => {
    expect(parseTraceparent(HEADER)).toEqual({ traceId: TRACE, spanId: PARENT, sampled: true });
    expect(parseTraceparent([` ${HEADER.replace(/-01$/, '-00')} `])).toEqual({
      traceId: TRACE,
      spanId: PARENT,
      sampled: false,
    });
  });

  it.each([
    ['nothing', undefined],
    ['an empty value', ''],
    ['another version', `01-${TRACE}-${PARENT}-01`],
    ['capital letters', HEADER.toUpperCase()],
    ['a short trace id', `00-${TRACE.slice(1)}-${PARENT}-01`],
    ['a trace id of zeros', `00-${'0'.repeat(32)}-${PARENT}-01`],
    ['a span id of zeros', `00-${TRACE}-${'0'.repeat(16)}-01`],
    ['something more', `${HEADER}-x`],
  ])('is no parent when it is %s', (_label, header) => {
    expect(parseTraceparent(header)).toBeNull();
  });
});

describe('without an endpoint', () => {
  it('loads nothing, sends nothing, and keeps the trace of the parent', async () => {
    const tracer = await createTracer(undefined, 'kage-sidecar-chromium');
    expect(tracer).toBe(noTracer);
    const span = tracer.start('sidecar.navigate', parseTraceparent(HEADER), {});
    expect(span.traceId).toBe(TRACE);
    expect(span.spanId).toMatch(/^[0-9a-f]{16}$/);
    expect(span.spanId).not.toBe(PARENT);
    span.set({ outcome: 'ok' });
    span.end();
    // A span without a parent begins a trace of its own.
    expect(tracer.start('sidecar.navigate', null, {}).traceId).toMatch(/^[0-9a-f]{32}$/);
    await tracer.shutdown();
  });
});

describe('with an endpoint', () => {
  it('makes the span a child of the span Kage named, with what it was told', async () => {
    const exporter = new InMemorySpanExporter();
    const tracer = await otlpTracer({
      endpoint: 'http://collector.invalid/v1/traces',
      serviceName: 'kage-sidecar-firefox',
      exporter,
    });
    const span = tracer.start('sidecar.navigate', parseTraceparent(HEADER), {
      engine: 'firefox',
      context: 'c1',
    });
    span.set({ outcome: 'timeout' });
    span.end(true);
    const [finished] = exporter.getFinishedSpans();
    expect(finished?.name).toBe('sidecar.navigate');
    expect(finished?.spanContext().traceId).toBe(TRACE);
    expect(finished?.spanContext().spanId).toBe(span.spanId);
    expect(finished?.parentSpanContext?.spanId).toBe(PARENT);
    expect(finished?.attributes).toEqual({ engine: 'firefox', context: 'c1', outcome: 'timeout' });
    // SpanStatusCode.ERROR
    expect(finished?.status.code).toBe(2);
    expect(finished?.resource.attributes['service.name']).toBe('kage-sidecar-firefox');
    await tracer.shutdown();
  });

  it('sends the spans to the collector as protobuf over HTTP when it shuts down', async () => {
    const seen: { path: string | undefined; headers: IncomingHttpHeaders; bytes: number }[] = [];
    const collector = createServer((request, response) => {
      let bytes = 0;
      request.on('data', (chunk: Buffer) => (bytes += chunk.length));
      request.on('end', () => {
        seen.push({ path: request.url, headers: request.headers, bytes });
        response.writeHead(200, { 'content-type': 'application/x-protobuf' }).end();
      });
    });
    await new Promise<void>((resolve) => collector.listen(0, '127.0.0.1', resolve));
    const { port } = collector.address() as AddressInfo;
    try {
      const tracer = await createTracer(
        `http://127.0.0.1:${port}/v1/traces`,
        'kage-sidecar-chromium',
      );
      tracer.start('sidecar.flow', parseTraceparent(HEADER), { engine: 'chromium' }).end();
      await tracer.shutdown();
      expect(seen).toHaveLength(1);
      expect(seen[0]?.path).toBe('/v1/traces');
      expect(seen[0]?.headers['content-type']).toBe('application/x-protobuf');
      expect(seen[0]?.bytes).toBeGreaterThan(0);
      // Kage's trace context is no header of the export.
      expect(seen[0]?.headers['traceparent']).toBeUndefined();
    } finally {
      await new Promise((resolve) => collector.close(resolve));
    }
  });
});

describe('a line of the log', () => {
  it('carries the ids of the span it is written in, once', () => {
    expect(traceFields({})).toEqual({});
    currentTrace.run({ trace_id: TRACE, span_id: 'aaaaaaaaaaaaaaaa' }, () => {
      expect(traceFields({})).toEqual({ trace_id: TRACE, span_id: 'aaaaaaaaaaaaaaaa' });
      // A request's logger has the trace id already.
      expect(traceFields({ trace_id: TRACE, reqId: 'r1' })).toEqual({
        span_id: 'aaaaaaaaaaaaaaaa',
      });
    });
  });
});
