/**
 * The shape of the sidecar's log lines (spec §20, F144): Kage's own, so that
 * one query reads both services.
 *
 * ```json
 * {"level":"INFO","timestamp":"2026-11-05T20:00:00.000Z","trace_id":"…","span_id":"…",
 *  "fields":{"message":"request completed","persona":"…","status":200}}
 * ```
 *
 * - `timestamp` in ISO 8601 (UTC), `level` in upper case, as Kage's
 *   `tracing` lines have them;
 * - the text and everything the line was written with under `fields`, the
 *   text as `fields.message`;
 * - `trace_id` and `span_id` at the top, as Kage writes them;
 * - a request's own id as `request` (Fastify's id of the request, bound to
 *   its lines), a persona as `persona`.
 *
 * What a line was written with is serialized here, not by pino's
 * serializers, which see only the top level: a request is its method and
 * path (never its headers, which hold the shared secret), an answer its
 * status, an error its type, message and stack.
 */
import { stdSerializers, type LoggerOptions } from 'pino';

/** The level names of the log, as Kage writes them. */
export type LineLevel = 'TRACE' | 'DEBUG' | 'INFO' | 'WARN' | 'ERROR' | 'FATAL';

/** A value of a line's fields as it is written. */
function serialized(key: string, value: unknown): unknown {
  if (key === 'err' || value instanceof Error) {
    return value instanceof Error ? stdSerializers.err(value) : value;
  }
  if (key === 'req' && typeof value === 'object' && value !== null) {
    const request = value as { method?: unknown; url?: unknown };
    // The path only: Kage's routes carry no query, and none is written.
    const url = typeof request.url === 'string' ? request.url.split('?')[0] : undefined;
    return { method: request.method, url };
  }
  if (key === 'res' && typeof value === 'object' && value !== null) {
    return { status: (value as { statusCode?: unknown }).statusCode };
  }
  return value;
}

/** The ids of the span a line was written in, which stay at the top. */
const TOP = new Set(['trace_id', 'span_id']);

/**
 * A line's own object in Kage's shape: the trace's ids at the top, the
 * rest under `fields`, `err` as `error`.
 */
export function shapeFields(object: Readonly<Record<string, unknown>>): Record<string, unknown> {
  const top: Record<string, unknown> = {};
  const fields: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(object)) {
    if (value === undefined) {
      continue;
    }
    if (TOP.has(key)) {
      top[key] = value;
    } else {
      fields[key === 'err' ? 'error' : key] = serialized(key, value);
    }
  }
  return { ...top, fields };
}

/**
 * The options that give pino Kage's line shape. The text a line is written
 * with is moved into its object as `message` before pino sees it, so that
 * it lands in `fields` with the rest.
 */
export function lineShape(): Pick<LoggerOptions, 'timestamp' | 'formatters' | 'hooks'> {
  return {
    timestamp: () => `,"timestamp":"${new Date().toISOString()}"`,
    formatters: {
      level: (label) => ({ level: label.toUpperCase() }),
      // No pid and host name: Kage's lines have neither.
      bindings: (bindings) =>
        Object.fromEntries(
          Object.entries(bindings).filter(([key]) => key !== 'pid' && key !== 'hostname'),
        ),
      log: shapeFields,
    },
    hooks: {
      logMethod(args, method) {
        const [first, second, ...rest] = args as unknown[];
        // The sidecar writes no format strings: a text is the whole text.
        if (typeof first === 'string') {
          method.apply(this, [{ message: first }] as Parameters<typeof method>);
          return;
        }
        if (first instanceof Error) {
          const message = typeof second === 'string' ? second : first.message;
          method.apply(this, [{ err: first, message }, ...rest] as Parameters<typeof method>);
          return;
        }
        if (typeof first === 'object' && first !== null && typeof second === 'string') {
          method.apply(this, [{ ...first, message: second }, ...rest] as Parameters<typeof method>);
          return;
        }
        method.apply(this, args);
      },
    },
  };
}

/**
 * One line in the same shape, for what writes before the logger exists or
 * outside it (the start-up lines, the watchdog's last word).
 */
export function line(
  level: LineLevel,
  fields: Readonly<Record<string, unknown>>,
  message: string,
): string {
  return JSON.stringify({
    level,
    timestamp: new Date().toISOString(),
    ...shapeFields({ ...fields, message }),
  });
}
