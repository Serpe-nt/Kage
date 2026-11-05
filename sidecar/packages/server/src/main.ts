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
import { line } from './log.js';
import { createTracer } from './tracing.js';
import { startWatchdog } from './watchdog.js';

try {
  const config = loadConfig(process.env);
  // A main thread that stops answering ends the process, so that the
  // container is restarted (F103).
  startWatchdog(config.stallMs);
  // The engine logs through the server's logger once that exists; until
  // then its start-up lines go to stderr in the log's shape (F144).
  let sink: ((level: string, fields: Record<string, unknown>, message: string) => void) | null =
    null;
  const early = (level: string) => (fields: Record<string, unknown>, message: string) => {
    if (sink === null) {
      const name = level === 'error' ? 'ERROR' : level === 'warn' ? 'WARN' : 'INFO';
      process.stderr.write(`${line(name, fields, message)}\n`);
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
  // The OpenTelemetry packages are loaded only when traces are sent.
  const tracer = await createTracer(config.otlpEndpoint, config.serviceName);
  const { server } = buildApp({
    engine,
    tracer,
    token: config.token,
    artifactsDir: config.artifactsDir,
    maxContexts: config.maxContexts,
    contextIdleMs: config.contextIdleMs,
    maxPagesPerBrowser: config.maxPagesPerBrowser,
    bodyLimitBytes: config.bodyLimitBytes,
    logLevel: config.logLevel,
    stallMs: config.stallMs,
    // An engine that stopped answering is not repaired here: the process
    // ends, and the container's restart policy starts it again (F103).
    onStalled: () => {
      process.exit(70);
    },
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
