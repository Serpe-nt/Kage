/**
 * Process entry point of the sidecar: `ENGINE=chromium|firefox node dist/main.js`.
 * Everything testable lives in the modules it wires together.
 */
import { tmpdir } from 'node:os';
import path from 'node:path';

import { startAudioServer } from '@kage/engine-core';

import { buildApp } from './app.js';
import { loadConfig } from './config.js';
import { loadEngine } from './engine-loader.js';

try {
  const config = loadConfig(process.env);
  // The engine logs through the server's logger once that exists; until
  // then its start-up lines go to stderr as JSON, like pino's.
  let sink: ((level: string, fields: Record<string, unknown>, message: string) => void) | null =
    null;
  const early = (level: string) => (fields: Record<string, unknown>, message: string) => {
    if (sink === null) {
      process.stderr.write(`${JSON.stringify({ level, msg: message, ...fields })}
`);
    } else {
      sink(level, fields, message);
    }
  };
  const log = { info: early('info'), warn: early('warn'), error: early('error') };
  // A container has no sound card: the browsers get a server with one
  // output and one input, so that a page finds devices as on a desktop.
  const audio =
    config.audio === 'pulse'
      ? await startAudioServer(path.join(tmpdir(), `kage-audio-${process.pid}`), log)
      : null;
  const engine = await loadEngine(config.engine, {
    ...(audio === null ? {} : { audioServer: audio.address }),
    personasDir: config.personasDir,
    allowCrossOsClaims: config.allowCrossOsClaims,
    gl: config.gl,
    selftestPages: config.selftestPages,
    log,
  });
  const { server } = buildApp({
    engine,
    token: config.token,
    artifactsDir: config.artifactsDir,
    maxContexts: config.maxContexts,
    contextIdleMs: config.contextIdleMs,
    maxPagesPerBrowser: config.maxPagesPerBrowser,
    bodyLimitBytes: config.bodyLimitBytes,
    logLevel: config.logLevel,
  });
  sink = (level, fields, message) => {
    if (level === 'error') {
      server.log.error(fields, message);
    } else if (level === 'warn') {
      server.log.warn(fields, message);
    } else {
      server.log.info(fields, message);
    }
  };

  for (const signal of ['SIGINT', 'SIGTERM'] as const) {
    process.once(signal, () => {
      server.log.info({ signal }, 'shutting down');
      // Closing drains requests, closes every context (which flushes its
      // profile) and the engine; the process then exits by itself.
      server
        .close()
        .catch((error: unknown) => {
          server.log.error({ err: error }, 'shutdown failed');
          process.exitCode = 1;
        })
        .finally(() => {
          void audio?.stop();
        });
    });
  }

  await server.listen({ host: config.host, port: config.port });
} catch (error) {
  const reason = error instanceof Error ? error.message : String(error);
  process.stderr.write(`sidecar failed to start: ${reason}
`);
  process.exitCode = 1;
}
