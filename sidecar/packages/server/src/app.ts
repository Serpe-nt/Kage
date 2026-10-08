import { HEALTH_PATH, HealthResponseSchema, type HealthResponse } from '@kage/contract';
import type { Engine } from '@kage/engine-core';
import Fastify, { type FastifyInstance } from 'fastify';

import type { LogLevel } from './config.js';

/** What the server needs to run. */
export interface AppOptions {
  /** The engine selected by the `ENGINE` switch. The app closes it on shutdown. */
  readonly engine: Engine;
  /** Log level; omit to disable logging (tests). */
  readonly logLevel?: LogLevel;
  /** Millisecond clock, injectable so tests control uptime. Defaults to `Date.now`. */
  readonly now?: () => number;
}

/**
 * Builds the sidecar HTTP server.
 *
 * Scaffold: the health endpoint is the only route. Token auth, metrics and
 * the persona context endpoints of the v1 contract land in M4.
 */
export function buildApp(options: AppOptions): FastifyInstance {
  const { engine, logLevel, now = Date.now } = options;
  const app = Fastify({ logger: logLevel === undefined ? false : { level: logLevel } });
  const startedAt = now();

  app.get(HEALTH_PATH, (): HealthResponse => {
    const status = engine.status();
    // Parsing the outgoing body keeps the server honest about its own contract.
    return HealthResponseSchema.parse({
      status: 'ok',
      engine: engine.name,
      browser_version: status.browserVersion,
      contexts_in_use: status.contextsInUse,
      uptime_seconds: Math.max(0, now() - startedAt) / 1000,
    });
  });

  app.addHook('onClose', async () => {
    await engine.close();
  });

  return app;
}
