/**
 * Process entry point of the sidecar: `ENGINE=chromium|firefox node dist/main.js`.
 * Everything testable lives in the modules it wires together.
 */
import { buildApp } from './app.js';
import { loadConfig } from './config.js';
import { loadEngine } from './engine-loader.js';

try {
  const config = loadConfig(process.env);
  const engine = await loadEngine(config.engine);
  const app = buildApp({ engine, logLevel: config.logLevel });

  for (const signal of ['SIGINT', 'SIGTERM'] as const) {
    process.once(signal, () => {
      app.log.info({ signal }, 'shutting down');
      // Closing drains requests and closes the engine; the process then exits by itself.
      app.close().catch((error: unknown) => {
        app.log.error({ err: error }, 'shutdown failed');
        process.exitCode = 1;
      });
    });
  }

  await app.listen({ host: config.host, port: config.port });
} catch (error) {
  const reason = error instanceof Error ? error.message : String(error);
  process.stderr.write(`sidecar failed to start: ${reason}\n`);
  process.exitCode = 1;
}
