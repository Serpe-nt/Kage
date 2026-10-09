import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

/** The fixture site of the repository: `tests/fixtures/site/server.mjs`. */
const SERVER = fileURLToPath(
  new URL('../../../../../tests/fixtures/site/server.mjs', import.meta.url),
);

/** One request the fixture site received. */
export interface FixtureRequest {
  readonly method: string;
  readonly host: string | null;
  readonly path: string;
  /** Header lines in wire order. */
  readonly headers: readonly (readonly [string, string])[];
}

/** A running fixture site. */
export interface FixtureSite {
  readonly port: number;
  /** Every request the site received since the last reset, oldest first. */
  requests(): Promise<FixtureRequest[]>;
  /** Everything pages reported back since the last reset. */
  reports(): Promise<Record<string, unknown>[]>;
  reset(): Promise<void>;
  stop(): Promise<void>;
}

/**
 * Starts the fixture site as its own process, the way the end-to-end suite
 * and a person would, and waits for the line that names its port.
 */
export async function startFixtureSite(): Promise<FixtureSite> {
  const child = spawn(process.execPath, [SERVER, '--port', '0'], {
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  const lines = createInterface({ input: child.stdout });
  const port = await new Promise<number>((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', (code) => {
      reject(new Error(`the fixture site exited with ${code ?? 'a signal'} before it listened`));
    });
    lines.once('line', (line) => {
      resolve((JSON.parse(line) as { port: number }).port);
    });
  });
  const control = async <T>(path: string): Promise<T> => {
    const response = await fetch(`http://127.0.0.1:${port}/__kage/${path}`);
    return (await response.json()) as T;
  };
  return {
    port,
    requests: () => control<FixtureRequest[]>('requests'),
    reports: () => control<Record<string, unknown>[]>('reports'),
    reset: async () => {
      await control('reset');
    },
    stop: () =>
      new Promise((resolve) => {
        child.once('exit', () => {
          resolve();
        });
        child.kill('SIGTERM');
      }),
  };
}
