import { HealthResponseSchema } from '@kage/contract';
import { createStubEngine, type Engine } from '@kage/engine-core';
import { describe, expect, it } from 'vitest';

import { buildApp } from './app.js';

describe('GET /v1/health', () => {
  it.each(['chromium', 'firefox'] as const)('reports the %s engine', async (name) => {
    const app = buildApp({ engine: createStubEngine(name) });

    const response = await app.inject({ method: 'GET', url: '/v1/health' });

    expect(response.statusCode).toBe(200);
    expect(response.headers['content-type']).toMatch(/^application\/json/);
    expect(HealthResponseSchema.parse(response.json())).toMatchObject({
      status: 'ok',
      engine: name,
      browser_version: null,
      contexts_in_use: 0,
    });
    await app.close();
  });

  it('reports what the engine reports', async () => {
    const engine: Engine = {
      name: 'chromium',
      status: () => ({ browserVersion: '141.0.7390.55', contextsInUse: 2 }),
      close: () => Promise.resolve(),
    };
    const app = buildApp({ engine });

    const response = await app.inject({ method: 'GET', url: '/v1/health' });

    expect(response.json()).toMatchObject({
      browser_version: '141.0.7390.55',
      contexts_in_use: 2,
    });
    await app.close();
  });

  it('measures uptime in seconds from server start', async () => {
    let clock = 1_000_000;
    const app = buildApp({ engine: createStubEngine('firefox'), now: () => clock });

    clock += 2500;
    const response = await app.inject({ method: 'GET', url: '/v1/health' });

    expect(response.json()).toMatchObject({ uptime_seconds: 2.5 });
    await app.close();
  });

  it('never reports a negative uptime when the clock steps back', async () => {
    let clock = 1_000_000;
    const app = buildApp({ engine: createStubEngine('firefox'), now: () => clock });

    clock -= 5000;
    const response = await app.inject({ method: 'GET', url: '/v1/health' });

    expect(response.json()).toMatchObject({ uptime_seconds: 0 });
    await app.close();
  });
});

describe('buildApp', () => {
  it('answers 404 for routes outside the contract', async () => {
    const app = buildApp({ engine: createStubEngine('chromium') });

    const response = await app.inject({ method: 'GET', url: '/v1/nope' });

    expect(response.statusCode).toBe(404);
    await app.close();
  });

  it('closes the engine when the server closes', async () => {
    let closed = 0;
    const engine: Engine = {
      name: 'chromium',
      status: () => ({ browserVersion: null, contextsInUse: 0 }),
      close: () => {
        closed += 1;
        return Promise.resolve();
      },
    };
    const app = buildApp({ engine });

    await app.close();

    expect(closed).toBe(1);
  });
});
