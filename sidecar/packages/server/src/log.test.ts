import { pino } from 'pino';
import { describe, expect, it } from 'vitest';

import { line, lineShape, shapeFields } from './log.js';

/** A pino logger in Kage's shape whose lines are kept, parsed. */
function logger(): { log: pino.Logger; lines: Record<string, unknown>[] } {
  const lines: Record<string, unknown>[] = [];
  const log = pino(
    { level: 'debug', ...lineShape() },
    {
      write: (text: string) => {
        lines.push(JSON.parse(text) as Record<string, unknown>);
      },
    },
  );
  return { log, lines };
}

describe("the log's shape (F144)", () => {
  it('writes the level in upper case, an ISO time and the text under fields', () => {
    const { log, lines } = logger();
    log.info({ persona: 'p-1', context_id: 'c-1' }, 'closing an idle context');
    log.warn('a text alone');
    expect(lines).toHaveLength(2);
    const [first, second] = lines;
    expect(first).toMatchObject({
      level: 'INFO',
      fields: { message: 'closing an idle context', persona: 'p-1', context_id: 'c-1' },
    });
    expect(String(first?.['timestamp'])).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/);
    expect(second).toMatchObject({ level: 'WARN', fields: { message: 'a text alone' } });
    for (const written of lines) {
      for (const key of ['msg', 'time', 'pid', 'hostname']) {
        expect(written).not.toHaveProperty(key);
      }
    }
  });

  it('keeps the trace ids at the top and binds a child the same way', () => {
    const { log, lines } = logger();
    log
      .child({ request: 'req-1', trace_id: 'a'.repeat(32) })
      .info({ span_id: 'b'.repeat(16) }, 'x');
    expect(lines[0]).toMatchObject({
      request: 'req-1',
      trace_id: 'a'.repeat(32),
      span_id: 'b'.repeat(16),
      fields: { message: 'x' },
    });
    expect(lines[0]?.['fields']).not.toHaveProperty('span_id');
  });

  it('writes an error by its type and message, a request by method and path, an answer by status', () => {
    const { log, lines } = logger();
    log.error(new TypeError('boom'));
    log.error(new TypeError('boom'), 'it broke');
    log.error({ err: new Error('inner'), code: 'internal' }, 'request failed');
    log.info(
      {
        req: { method: 'GET', url: '/v1/health?x=1', headers: { 'x-kage-sidecar-token': 's' } },
        res: { statusCode: 200, headers: {} },
      },
      'request completed',
    );
    log.info({ count: 1 });
    const fields = lines.map((written) => written['fields'] as Record<string, unknown>);
    expect(fields[0]).toMatchObject({
      message: 'boom',
      error: { type: 'TypeError', message: 'boom' },
    });
    expect(fields[1]).toMatchObject({ message: 'it broke', error: { message: 'boom' } });
    expect(fields[2]).toMatchObject({
      message: 'request failed',
      code: 'internal',
      error: { message: 'inner' },
    });
    expect(fields[3]).toEqual({
      message: 'request completed',
      req: { method: 'GET', url: '/v1/health' },
      res: { status: 200 },
    });
    expect(fields[4]).toEqual({ count: 1 });
    expect(JSON.stringify(lines)).not.toContain('x-kage-sidecar-token');
  });

  it('writes a line by hand in the same shape', () => {
    const written = JSON.parse(line('ERROR', { code: 1, trace_id: 't' }, 'early')) as Record<
      string,
      unknown
    >;
    expect(written).toMatchObject({
      level: 'ERROR',
      trace_id: 't',
      fields: { code: 1, message: 'early' },
    });
    expect(typeof written['timestamp']).toBe('string');
    expect(shapeFields({ gone: undefined, err: 'text' })).toEqual({ fields: { error: 'text' } });
  });
});
