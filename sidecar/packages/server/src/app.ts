import { createHash, timingSafeEqual } from 'node:crypto';

import {
  CONTRACT_SEMVER,
  ContextParamsSchema,
  DEFAULT_NAVIGATE_TIMEOUT_MS,
  ENDPOINTS,
  ERROR_STATUS,
  ExportStateResponseSchema,
  FlowEventSchema,
  GeneratePersonaResponseSchema,
  UpgradePersonaResponseSchema,
  HealthResponseSchema,
  ImportStateResponseSchema,
  OpenContextResponseSchema,
  PageResultSchema,
  SelftestResponseSchema,
  TOKEN_HEADER,
  routePath,
  type Endpoint,
  type ErrorCode,
  type ErrorResponse,
  type FlowEvent,
  type FlowRequest,
  type HealthResponse,
  type NavigateRequest,
  type OpenContextResponse,
} from '@kage/contract';
import {
  ArtifactWriter,
  DEFAULT_FLOW_TIMEOUT_MS,
  EngineError,
  type Engine,
  type EngineLog,
  type OperationEnv,
} from '@kage/engine-core';
import Fastify, {
  LogController,
  type FastifyInstance,
  type FastifyReply,
  type FastifyRequest,
} from 'fastify';
import type { DestinationStream } from 'pino';
import type { z } from 'zod';

import type { LogLevel } from './config.js';
import { ContextManager } from './context-manager.js';
import { lineShape } from './log.js';
import { SidecarMetrics } from './metrics.js';
import {
  currentTrace,
  noTracer,
  parseTraceparent,
  traceFields,
  type ActiveSpan,
  type Tracer,
} from './tracing.js';

/** What the server needs to run. */
export interface AppOptions {
  /** The engine selected by the `ENGINE` switch. The app closes it on shutdown. */
  readonly engine: Engine;
  /** The shared secret every request must carry. */
  readonly token: string;
  /** Where the artifacts volume shared with Kage is mounted. */
  readonly artifactsDir: string;
  readonly maxContexts: number;
  readonly contextIdleMs: number;
  readonly maxPagesPerBrowser: number;
  /** Largest request body in bytes; default 16 MiB. */
  readonly bodyLimitBytes?: number;
  /** Log level; omit to disable logging (tests). */
  readonly logLevel?: LogLevel;
  /** Millisecond clock, injectable so tests control uptime and idleness. Defaults to `Date.now`. */
  readonly now?: () => number;
  /** Collect process metrics; default true. Tests that build many apps turn it off. */
  readonly processMetrics?: boolean;
  /** Makes the spans of navigations and flows (spec §20); default: none sent. The app shuts it down. */
  readonly tracer?: Tracer;
  /** Where the log goes; default stdout. */
  readonly logStream?: DestinationStream;
  /**
   * How long the engine may not answer (an operation that outlasted its
   * bound and does not end) before `onStalled` is called (F103); 0 or
   * absent never.
   */
  readonly stallMs?: number;
  /** How long an abandoned operation may go on before the engine counts as stalled; 60 s unless said. */
  readonly abandonedGraceMs?: number;
  /** What is done with an engine that stopped answering; the entry point ends the process. */
  readonly onStalled?: (reason: string) => void;
  /** One bound for every navigation and flow in place of the ones their requests make; for tests. */
  readonly operationBoundMs?: number;
}

/** The span of Kage's request each request's logger was made for. */
const requestSpans = new WeakMap<object, string>();

/** How often the server looks whether its engine still answers. */
const STALL_LOOK_MS = 5000;

/**
 * Runs the work of a request of Kage's in a span of its own, the child of
 * the span Kage named in `traceparent`; every line of the log written
 * meanwhile carries the span's ids.
 */
async function traced<T>(
  tracer: Tracer,
  name: string,
  request: FastifyRequest,
  attributes: Readonly<Record<string, string>>,
  work: () => Promise<T>,
  ended: (result: T, span: ActiveSpan) => boolean,
): Promise<T> {
  const span = tracer.start(name, parseTraceparent(request.headers['traceparent']), attributes);
  try {
    const result = await currentTrace.run({ trace_id: span.traceId, span_id: span.spanId }, work);
    span.end(ended(result, span));
    return result;
  } catch (error) {
    span.set({ outcome: error instanceof EngineError ? error.code : 'error' });
    span.end(true);
    throw error;
  }
}

/** The longest a persona dwells on a page with a human dwell, as Kage bounds it. */
const HUMAN_DWELL_CAP_MS = 45_000;
/**
 * What the server's bound of a navigation leaves beyond the navigation's
 * own times: less than the margin Kage's client waits beyond them (30 s,
 * `navigation_bound` in kage-browser), so that Kage hears the typed answer
 * before it gives up itself (F103).
 */
const OPERATION_SLACK_MS = 25_000;
/**
 * What the server's bound of a flow leaves beyond the flow's budget: less
 * than what Kage waits beyond that budget (20 s, `BROWSER_GRACE` in
 * kage-scheduler's executor), with room for the answer to travel and the
 * context to close, so that Kage hears the typed answer before it gives up
 * itself and a retry never finds the context busy (F103). A test on each
 * side reads the other side's number.
 */
export const FLOW_SLACK_MS = 10_000;

/**
 * The longest the server waits for a navigation (F103): reaching the page,
 * the patience and the dwell, as Kage's own bound counts them, and a slack.
 * The session's reads are bounded on their own; this is the backstop for
 * whatever else hangs.
 */
export function navigateBoundMs(request: NavigateRequest): number {
  const reach = request.timeout_ms ?? DEFAULT_NAVIGATE_TIMEOUT_MS;
  const fixed = request.dwell_ms ?? 0;
  const behavior = request.behavior;
  const onPage =
    behavior === undefined
      ? fixed
      : (behavior.patience_ms ?? 0) + (behavior.dwell === 'human' ? HUMAN_DWELL_CAP_MS : fixed);
  return reach + onPage + OPERATION_SLACK_MS;
}

/** The longest the server waits for a flow (F103): its budget and a slack. */
export function flowBoundMs(request: FlowRequest): number {
  return (request.timeout_ms ?? DEFAULT_FLOW_TIMEOUT_MS) + FLOW_SLACK_MS;
}

/** The server and the parts tests and the entry point reach into. */
export interface App {
  readonly server: FastifyInstance;
  readonly contexts: ContextManager;
  readonly metrics: SidecarMetrics;
}

const digest = (value: string) => createHash('sha256').update(value).digest();

/**
 * Compares two secrets in constant time. Both are hashed first, so neither
 * the content nor the length of the expected one shows in the timing.
 */
export function sameSecret(given: string, expected: string): boolean {
  return timingSafeEqual(digest(given), digest(expected));
}

function failure(code: ErrorCode, message: string): { status: number; body: ErrorResponse } {
  return { status: ERROR_STATUS[code], body: { code, message } };
}

/** The first problems of a failed validation, as one sentence. */
function describeIssues(error: z.ZodError): string {
  return error.issues
    .slice(0, 5)
    .map((issue) => `${issue.path.join('.') || 'body'}: ${issue.message}`)
    .join('; ');
}

function parse<Schema extends z.ZodType>(schema: Schema, value: unknown): z.infer<Schema> {
  const parsed = schema.safeParse(value);
  if (!parsed.success) {
    throw new EngineError('invalid_request', describeIssues(parsed.error));
  }
  return parsed.data;
}

/**
 * Builds the sidecar HTTP server (spec §10.1, §10.2).
 *
 * Every route needs the shared secret, health and metrics included: the
 * server listens on the compose network only, and nothing on it has a
 * reason to ask without the secret. Every body is validated against the
 * contract on the way in, and every answer on the way out, so the server
 * cannot drift from the document Kage was built against.
 */
export function buildApp(options: AppOptions): App {
  const { engine, now = Date.now, tracer = noTracer } = options;
  const server = Fastify({
    bodyLimit: options.bodyLimitBytes ?? 16_777_216,
    logger:
      options.logLevel === undefined
        ? false
        : {
            level: options.logLevel,
            // Kage's line shape (spec §20, F144).
            ...lineShape(),
            // Requests are logged by method and path (`lineShape`). The
            // secret header and request bodies (which hold proxy passwords)
            // never are; the redaction stays as a second guard.
            redact: [`fields.req.headers["${TOKEN_HEADER}"]`, 'fields.req.headers.authorization'],
            // A line written in a span carries its ids (spec §20): the
            // sidecar's span, or outside it the span of Kage's request.
            mixin: (_merge, _level, logger) =>
              traceFields(logger.bindings(), requestSpans.get(logger)),
            ...(options.logStream === undefined ? {} : { stream: options.logStream }),
          },
    // A request's lines carry its id under Kage's name for it.
    logController: new LogController({ requestIdLogLabel: 'request' }),
    // Every line of a request of Kage's carries Kage's trace id, and the
    // span of Kage's request to the sidecar where no span of the sidecar's
    // own is under way (F144).
    childLoggerFactory: (logger, bindings, childOptions, raw) => {
      const parent = parseTraceparent(raw.headers['traceparent']);
      const child = logger.child(
        parent === null ? bindings : { ...bindings, trace_id: parent.traceId },
        childOptions,
      );
      if (parent !== null) {
        requestSpans.set(child, parent.spanId);
      }
      return child;
    },
  });
  const log: EngineLog = {
    info: (fields, message) => {
      server.log.info(fields, message);
    },
    warn: (fields, message) => {
      server.log.warn(fields, message);
    },
    error: (fields, message) => {
      server.log.error(fields, message);
    },
  };
  const startedAt = now();
  const artifacts = new ArtifactWriter(options.artifactsDir);
  let contexts: ContextManager | null = null;
  const metrics = new SidecarMetrics(
    engine.name,
    () => contexts?.count ?? 0,
    options.processMetrics ?? true,
  );
  contexts = new ContextManager(
    engine,
    {
      maxContexts: options.maxContexts,
      idleMs: options.contextIdleMs,
      maxPagesPerBrowser: options.maxPagesPerBrowser,
      ...(options.abandonedGraceMs === undefined
        ? {}
        : { abandonedGraceMs: options.abandonedGraceMs }),
    },
    metrics,
    log,
    now,
  );
  const manager = contexts;
  manager.start();
  // An engine that stopped answering for long is ended, so that it is
  // started again (F103): the health check fails meanwhile.
  let stalledSince: number | null = null;
  const stallMs = options.stallMs ?? 0;
  const stallTimer =
    stallMs > 0 && options.onStalled !== undefined
      ? setInterval(
          () => {
            const stalled = manager.stalled();
            if (stalled === null) {
              stalledSince = null;
              return;
            }
            stalledSince ??= now();
            if (now() - stalledSince >= stallMs) {
              log.error(
                { stalled_ms: now() - stalledSince },
                `the engine does not answer: ${stalled}`,
              );
              options.onStalled?.(stalled);
            }
          },
          Math.min(STALL_LOOK_MS, stallMs),
        )
      : null;
  stallTimer?.unref();

  server.addHook('onRequest', async (request, reply) => {
    const given = request.headers[TOKEN_HEADER];
    if (typeof given !== 'string' || !sameSecret(given, options.token)) {
      const { status, body } = failure(
        'unauthorized',
        `the ${TOKEN_HEADER} header is missing or wrong`,
      );
      // Returning the reply ends the request here: no handler runs.
      return reply.code(status).send(body);
    }
    return undefined;
  });

  server.setNotFoundHandler(async (request, reply) => {
    const { status, body } = failure(
      'not_found',
      `no route ${request.method} ${request.url.split('?')[0] ?? ''}`,
    );
    await reply.code(status).send(body);
  });

  server.setErrorHandler(async (error: unknown, request, reply) => {
    if (error instanceof EngineError) {
      const { status, body } = failure(error.code, error.message);
      if (status >= 500) {
        request.log.error({ err: error, code: error.code }, 'request failed');
      }
      await reply.code(status).send(body);
      return;
    }
    const fastifyCode = (error as { code?: unknown } | null)?.code;
    if (fastifyCode === 'FST_ERR_CTP_BODY_TOO_LARGE') {
      const { status, body } = failure('payload_too_large', 'the request body is over the limit');
      await reply.code(status).send(body);
      return;
    }
    if (typeof fastifyCode === 'string' && fastifyCode.startsWith('FST_ERR_CTP_')) {
      // Not JSON, an empty body where one is needed, or another media type.
      const { status, body } = failure('invalid_request', 'the request body is not valid JSON');
      await reply.code(status).send(body);
      return;
    }
    request.log.error({ err: error }, 'request failed unexpectedly');
    // What went wrong may quote a request: it goes to the log, not to the caller.
    const { status, body } = failure('internal', 'an unexpected failure; see the sidecar log');
    await reply.code(status).send(body);
  });

  const contextId = (request: FastifyRequest): string =>
    parse(ContextParamsSchema, request.params).id;
  const operationEnv = (id: string): OperationEnv => ({
    artifacts,
    defaultArtifactPrefix: `sidecar/${id}`,
  });

  /** Registers a JSON endpoint: validates the body, runs the handler, validates the answer. */
  function json<Request extends z.ZodType | null, Response extends z.ZodType>(
    endpoint: Endpoint,
    response: Response,
    handler: (
      body: Request extends z.ZodType ? z.infer<Request> : null,
      request: FastifyRequest,
    ) => Promise<z.infer<Response>> | z.infer<Response>,
  ): void {
    server.route({
      method: endpoint.method,
      url: routePath(endpoint),
      handler: async (request: FastifyRequest, reply: FastifyReply) => {
        const body = (
          endpoint.request === null ? null : parse(endpoint.request, request.body)
        ) as Request extends z.ZodType ? z.infer<Request> : null;
        const result = await handler(body, request);
        // An answer that is not what the contract says is a bug of this
        // server; it must not reach Kage looking like a valid one.
        const checked = response.safeParse(result);
        if (!checked.success) {
          throw new Error(
            `${endpoint.operation} produced an answer outside the contract: ${describeIssues(checked.error)}`,
          );
        }
        await reply.code(endpoint.response.status).send(checked.data);
      },
    });
  }

  /** What the health check answers while the engine answers. */
  const healthy = (): HealthResponse => ({
    status: 'ok',
    engine: engine.name,
    browser_version: engine.status().browserVersion,
    contexts_in_use: manager.count,
    max_contexts: options.maxContexts,
    uptime_seconds: Math.max(0, now() - startedAt) / 1000,
    contract_version: CONTRACT_SEMVER,
  });

  json<null, typeof HealthResponseSchema>(
    ENDPOINTS.health,
    HealthResponseSchema,
    (): HealthResponse => {
      // An engine that does not answer fails the health check (F103).
      const stalled = manager.stalled();
      if (stalled !== null) {
        throw new EngineError('internal', `the engine does not answer: ${stalled}`);
      }
      return healthy();
    },
  );

  server.get(routePath(ENDPOINTS.metrics), async (_request, reply) => {
    await reply.header('content-type', metrics.contentType).send(await metrics.render());
  });

  json<typeof ENDPOINTS.generatePersona.request, typeof GeneratePersonaResponseSchema>(
    ENDPOINTS.generatePersona,
    GeneratePersonaResponseSchema,
    (body) => engine.generatePersona(body),
  );

  json<typeof ENDPOINTS.upgradePersona.request, typeof UpgradePersonaResponseSchema>(
    ENDPOINTS.upgradePersona,
    UpgradePersonaResponseSchema,
    (body) => engine.upgradePersona(body),
  );

  json<typeof ENDPOINTS.openContext.request, typeof OpenContextResponseSchema>(
    ENDPOINTS.openContext,
    OpenContextResponseSchema,
    async (body): Promise<OpenContextResponse> => {
      const { managed, reused } = await manager.open(body);
      return {
        context_id: managed.id,
        persona_id: managed.personaId,
        reused,
        browser_version: managed.context.browserVersion,
        applied: managed.context.applied,
        opened_at: managed.openedAt.toISOString(),
      };
    },
  );

  server.delete(routePath(ENDPOINTS.closeContext), async (request, reply) => {
    await manager.close(contextId(request));
    await reply.code(ENDPOINTS.closeContext.response.status).send();
  });

  json<typeof ENDPOINTS.navigate.request, typeof PageResultSchema>(
    ENDPOINTS.navigate,
    PageResultSchema,
    async (body, request) => {
      const id = contextId(request);
      const began = now();
      try {
        const result = await traced(
          tracer,
          'sidecar.navigate',
          request,
          { engine: engine.name, context: id },
          () =>
            manager.run(
              id,
              true,
              (context) => context.navigate(body, operationEnv(id)),
              options.operationBoundMs ?? navigateBoundMs(body),
            ),
          (page, span) => {
            span.set({
              outcome: page.error?.code ?? 'ok',
              ...(page.status === null ? {} : { status: page.status }),
            });
            return page.error !== null;
          },
        );
        metrics.navigated(result.error?.code ?? 'ok', (now() - began) / 1000);
        return result;
      } catch (error) {
        metrics.navigated('error', (now() - began) / 1000);
        throw error;
      }
    },
  );

  // A flow answers with one line of JSON per finished step and a final
  // line (spec §10.2), written as the steps finish. Whatever refuses the
  // flow before its first step is an ordinary error answer; once lines are
  // on the wire the status cannot change, so a flow that breaks afterwards
  // ends without its final line, which is how a reader knows.
  server.post(routePath(ENDPOINTS.flow), async (request, reply) => {
    const body = parse(ENDPOINTS.flow.request, request.body);
    const id = contextId(request);
    const stream = { started: false, ok: false };
    const abandoned = { flow: false };
    const emit = (event: FlowEvent) => {
      const checked = FlowEventSchema.safeParse(event);
      if (!checked.success) {
        throw new Error(
          `flow produced an event outside the contract: ${describeIssues(checked.error)}`,
        );
      }
      if (!stream.started) {
        stream.started = true;
        reply.hijack();
        reply.raw.writeHead(ENDPOINTS.flow.response.status, {
          'content-type': `${ENDPOINTS.flow.response.media}; charset=utf-8`,
          'cache-control': 'no-store',
        });
      }
      if (checked.data.type === 'final') {
        stream.ok = checked.data.ok;
      }
      reply.raw.write(`${JSON.stringify(checked.data)}\n`);
    };
    try {
      await traced(
        tracer,
        'sidecar.flow',
        request,
        { engine: engine.name, context: id },
        () =>
          manager.run(
            id,
            true,
            (context) =>
              context.flow(body, operationEnv(id), (event) => {
                // A flow that outlasted its bound sends nothing more.
                if (!abandoned.flow) {
                  emit(event);
                }
              }),
            options.operationBoundMs ?? flowBoundMs(body),
          ),
        (_done, span) => {
          span.set({ outcome: stream.ok ? 'ok' : 'failed' });
          return !stream.ok;
        },
      );
      metrics.flowed(stream.ok ? 'ok' : 'failed');
    } catch (error) {
      abandoned.flow = true;
      metrics.flowed('error');
      if (!stream.started) {
        throw error;
      }
      request.log.error({ err: error, context_id: id }, 'a flow broke after its stream began');
    } finally {
      if (stream.started) {
        reply.raw.end();
      }
    }
  });

  json<typeof ENDPOINTS.exportState.request, typeof ExportStateResponseSchema>(
    ENDPOINTS.exportState,
    ExportStateResponseSchema,
    (body, request) =>
      manager.run(contextId(request), false, (context) => context.exportState(body)),
  );

  json<typeof ENDPOINTS.importState.request, typeof ImportStateResponseSchema>(
    ENDPOINTS.importState,
    ImportStateResponseSchema,
    (body, request) =>
      manager.run(contextId(request), false, (context) => context.importState(body)),
  );

  json<typeof ENDPOINTS.selftest.request, typeof SelftestResponseSchema>(
    ENDPOINTS.selftest,
    SelftestResponseSchema,
    async (body, request) => {
      const id = contextId(request);
      const result = await manager.run(id, true, (context) =>
        context.selftest(body, operationEnv(id)),
      );
      metrics.selftested(!result.passed ? 'failed' : result.complete ? 'passed' : 'incomplete');
      return result;
    },
  );

  server.addHook('onClose', async () => {
    if (stallTimer !== null) {
      clearInterval(stallTimer);
    }
    await manager.closeAll();
    await engine.close();
    // What is still queued is sent once the last navigation has ended.
    await tracer.shutdown();
  });

  return { server, contexts: manager, metrics };
}
