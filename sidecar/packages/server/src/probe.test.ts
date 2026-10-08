import { createServer, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';

import { createStubEngine } from '@kage/engine-core';
import { afterEach, describe, expect, it } from 'vitest';

import { buildApp } from './app.js';
import { probeHealth } from './probe.js';

const servers: Server[] = [];

/** Starts a plain HTTP server that answers every request the same way. */
async function answerWith(status: number, body: string): Promise<string> {
  const server = createServer((_request, response) => {
    response.writeHead(status, { 'content-type': 'application/json' }).end(body);
  });
  servers.push(server);
  await new Promise<void>((resolve) => {
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address() as AddressInfo;
  return `http://127.0.0.1:${port}`;
}

afterEach(async () => {
  await Promise.all(
    servers.splice(0).map(
      (server) =>
        new Promise<void>((resolve) => {
          server.close(() => {
            resolve();
          });
        }),
    ),
  );
});

describe('probeHealth', () => {
  it('returns the health of a running sidecar', async () => {
    const app = buildApp({ engine: createStubEngine('firefox') });
    const origin = await app.listen({ host: '127.0.0.1', port: 0 });

    await expect(probeHealth(origin)).resolves.toMatchObject({ status: 'ok', engine: 'firefox' });

    await app.close();
  });

  it('fails when nothing listens', async () => {
    const origin = await answerWith(200, '{}');
    await new Promise<void>((resolve) => {
      servers.splice(0)[0]?.close(() => {
        resolve();
      });
    });

    await expect(probeHealth(origin)).rejects.toThrow();
  });

  it('fails on a non-2xx answer', async () => {
    const origin = await answerWith(503, '{}');

    await expect(probeHealth(origin)).rejects.toThrow(/503/);
  });

  it('fails on a body outside the contract', async () => {
    const origin = await answerWith(200, JSON.stringify({ status: 'ok' }));

    await expect(probeHealth(origin)).rejects.toThrow();
  });
});
