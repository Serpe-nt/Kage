/**
 * Container healthcheck: `node dist/healthcheck.js`. Exits 0 and prints the
 * health body when the local server is healthy, exits 1 otherwise.
 */
import { probeHealth } from './probe.js';

const port = process.env['SIDECAR_PORT'] ?? '8090';

try {
  const health = await probeHealth(`http://127.0.0.1:${port}`);
  process.stdout.write(`${JSON.stringify(health)}\n`);
} catch (error) {
  const reason = error instanceof Error ? error.message : String(error);
  process.stderr.write(`unhealthy: ${reason}\n`);
  process.exitCode = 1;
}
