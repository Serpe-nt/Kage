/**
 * Container healthcheck: `node dist/healthcheck.js`. Exits 0 and prints the
 * health body when the local server is healthy, exits 1 otherwise. It reads
 * the port and the shared secret from the container's own environment.
 */
import { loadConfig } from './config.js';
import { probeHealth } from './probe.js';

try {
  const config = loadConfig(process.env);
  const health = await probeHealth(`http://127.0.0.1:${config.port}`, config.token);
  process.stdout.write(`${JSON.stringify(health)}\n`);
} catch (error) {
  const reason = error instanceof Error ? error.message : String(error);
  process.stderr.write(`unhealthy: ${reason}\n`);
  process.exitCode = 1;
}
