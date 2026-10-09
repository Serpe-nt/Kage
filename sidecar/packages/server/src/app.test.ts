import { mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import {
  ErrorResponseSchema,
  HealthResponseSchema,
  OpenContextResponseSchema,
  PageResultSchema,
  type OpenContextRequest,
  type PageResult,
} from '@kage/contract';
import { EngineError, createStubEngine } from '@kage/engine-core';
import { afterEach, describe, expect, it } from 'vitest';

import { buildApp, sameSecret, type App, type AppOptions } from './app.js';
import { TEST_TOKEN, createFakeEngine, exampleOf, type FakeEngine } from './testing/fake-engine.js';

const apps: App[] = [];
const AUTH = { 'x-kage-sidecar-token': TEST_TOKEN };

async function start(
  change: Partial<AppOptions> = {},
): Promise<{ app: App; engine: FakeEngine; clock: { now: number } }> {
  const engine = createFakeEngine();
  const clock = { now: 1_000_000 };
  const app = buildApp({
    engine,
    token: TEST_TOKEN,
    artifactsDir: await mkdtemp(path.join(tmpdir(), 'kage-server-')),
    maxContexts: 2,
    contextIdleMs: 120_000,
    maxPagesPerBrowser: 500,
    now: () => clock.now,
    processMetrics: false,
    ...change,
  });
  apps.push(app);
  return { app, engine, clock };
}

afterEach(async () => {
  await Promise.all(apps.splice(0).map((app) => app.server.close()));
});

const post = (app: App, url: string, payload: unknown, headers: Record<string, string> = AUTH) =>
  app.server.inject({ method: 'POST', url, payload: payload as object, headers });

async function open(app: App, change: Partial<OpenContextRequest> = {}): Promise<string> {
  const response = await post(app, '/v1/contexts', {
    ...exampleOf<OpenContextRequest>('OpenContextRequest'),
    ...change,
  });
  expect(response.statusCode, response.body).toBe(201);
  return OpenContextResponseSchema.parse(response.json()).context_id;
}

const expectError = (
  response: { statusCode: number; json: () => unknown },
  status: number,
  code: string,
) => {
  expect(response.statusCode).toBe(status);
  expect(ErrorResponseSchema.parse(response.json()).code).toBe(code);
};

describe('the shared secret', () => {
  it.each([
    ['no header', {}],
    ['a wrong secret', { 'x-kage-sidecar-token': `${TEST_TOKEN}x` }],
    ['a prefix of the secret', { 'x-kage-sidecar-token': TEST_TOKEN.slice(0, -1) }],
    ['an empty secret', { 'x-kage-sidecar-token': '' }],
    ['the secret in another header', { authorization: `Bearer ${TEST_TOKEN}` }],
  ])('is required on every route: %s is refused', async (_label, headers) => {
    const { app, engine } = await start();
    for (const [method, url] of [
      ['GET', '/v1/health'],
      ['GET', '/v1/metrics'],
      ['POST', '/v1/personas/generate'],
      ['POST', '/v1/contexts'],
      ['DELETE', '/v1/contexts/0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f'],
      ['POST', '/v1/contexts/0199c1f3-0a1b-7d2c-8e3f-4a5b6c7d8e9f/navigate'],
      ['GET', '/v1/nothing-here'],
    ] as const) {
      const response = await app.server.inject({
        method,
        url,
        headers,
        ...(method === 'POST' ? { payload: {} } : {}),
      });
      expectError(response, 401, 'unauthorized');
      expect(response.body).not.toContain(TEST_TOKEN);
    }
    expect(engine.contexts).toHaveLength(0);
  });

  it('is compared whole', () => {
    expect(sameSecret(TEST_TOKEN, TEST_TOKEN)).toBe(true);
    expect(sameSecret('', TEST_TOKEN)).toBe(false);
    expect(sameSecret(`${TEST_TOKEN} `, TEST_TOKEN)).toBe(false);
    expect(sameSecret(TEST_TOKEN.toUpperCase(), TEST_TOKEN)).toBe(false);
  });
});

describe('GET /v1/health', () => {
  it('reports the engine, its browser, the contexts and the contract', async () => {
    const { app, clock } = await start();
    await open(app);
    clock.now += 2500;
    const response = await app.server.inject({ method: 'GET', url: '/v1/health', headers: AUTH });
    expect(response.statusCode).toBe(200);
    expect(HealthResponseSchema.parse(response.json())).toEqual({
      status: 'ok',
      engine: 'chromium',
      browser_version: '155.0.8059.39',
      contexts_in_use: 1,
      max_contexts: 2,
      uptime_seconds: 2.5,
      contract_version: '1.0.0',
    });
  });

  it('reports a stub engine without a browser and never a negative uptime', async () => {
    const clock = { now: 5000 };
    const app = buildApp({
      engine: createStubEngine('firefox'),
      token: TEST_TOKEN,
      artifactsDir: tmpdir(),
      maxContexts: 3,
      contextIdleMs: 1000,
      maxPagesPerBrowser: 1,
      now: () => clock.now,
      processMetrics: false,
    });
    apps.push(app);
    clock.now = 1000;
    const health = (
      await app.server.inject({ method: 'GET', url: '/v1/health', headers: AUTH })
    ).json<Record<string, unknown>>();
    expect(health).toMatchObject({ engine: 'firefox', browser_version: null, uptime_seconds: 0 });
  });

  it('answers not_implemented for everything a stub engine cannot do', async () => {
    const app = buildApp({
      engine: createStubEngine('firefox'),
      token: TEST_TOKEN,
      artifactsDir: tmpdir(),
      maxContexts: 3,
      contextIdleMs: 1000,
      maxPagesPerBrowser: 1,
      processMetrics: false,
    });
    apps.push(app);
    expectError(
      await post(app, '/v1/personas/generate', exampleOf('GeneratePersonaRequest')),
      501,
      'not_implemented',
    );
    expectError(
      await post(app, '/v1/contexts', exampleOf('OpenContextRequest')),
      501,
      'not_implemented',
    );
  });
});

describe('personas and contexts', () => {
  it('generates a persona through the engine', async () => {
    const { app } = await start();
    const response = await post(app, '/v1/personas/generate', exampleOf('GeneratePersonaRequest'));
    expect(response.statusCode).toBe(200);
    expect(response.json()).toEqual(exampleOf('GeneratePersonaResponse'));
  });

  it('opens a context once per persona and returns the open one for the same request', async () => {
    const { app, engine } = await start();
    const first = await post(app, '/v1/contexts', exampleOf('OpenContextRequest'));
    expect(first.statusCode).toBe(201);
    const opened = OpenContextResponseSchema.parse(first.json());
    expect(opened).toMatchObject({
      reused: false,
      browser_version: '155.0.8059.39',
      opened_at: '1970-01-01T00:16:40.000Z',
    });
    expect(engine.contexts[0]?.request.profile_dir).toBe(
      exampleOf<OpenContextRequest>('OpenContextRequest').profile_dir,
    );

    const again = OpenContextResponseSchema.parse(
      (await post(app, '/v1/contexts', exampleOf('OpenContextRequest'))).json(),
    );
    expect(again).toMatchObject({ context_id: opened.context_id, reused: true });
    expect(engine.contexts).toHaveLength(1);
  });

  it('refuses the same persona with other parameters, and more contexts than the cap', async () => {
    const { app, engine } = await start();
    await open(app);
    expectError(
      await post(app, '/v1/contexts', {
        ...exampleOf<OpenContextRequest>('OpenContextRequest'),
        timezone: 'Europe/Paris',
      }),
      409,
      'context_conflict',
    );
    await open(app, { persona_id: '0199c1f2-6b7a-7c3e-9d41-000000000002' });
    expectError(
      await post(app, '/v1/contexts', {
        ...exampleOf<OpenContextRequest>('OpenContextRequest'),
        persona_id: '0199c1f2-6b7a-7c3e-9d41-000000000003',
      }),
      409,
      'context_limit',
    );
    expect(engine.contexts).toHaveLength(2);
  });

  it('closes a context, flushes it, and does not know it afterwards', async () => {
    const { app, engine } = await start();
    const id = await open(app);
    const closed = await app.server.inject({
      method: 'DELETE',
      url: `/v1/contexts/${id}`,
      headers: AUTH,
    });
    expect(closed.statusCode).toBe(204);
    expect(closed.body).toBe('');
    expect(engine.contexts[0]?.closes).toBe(1);
    expectError(
      await app.server.inject({ method: 'DELETE', url: `/v1/contexts/${id}`, headers: AUTH }),
      404,
      'context_not_found',
    );
    expectError(
      await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest')),
      404,
      'context_not_found',
    );
    // The persona can be opened again.
    expect(await open(app)).not.toBe(id);
  });
});

describe('operations on a context', () => {
  it('navigates and hands the engine a place for its artifacts', async () => {
    const { app, engine } = await start();
    const id = await open(app);
    const response = await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'));
    expect(response.statusCode).toBe(200);
    expect(PageResultSchema.parse(response.json()).status).toBe(200);
    const [call] = engine.contexts[0]?.navigated ?? [];
    expect(call?.url).toBe('https://example.org/listing?page=2');
    expect(call?.env.defaultArtifactPrefix).toBe(`sidecar/${id}`);
    const written = await call?.env.artifacts.write(
      `sidecar/${id}/x.txt`,
      Buffer.from('x'),
      'text/plain',
    );
    expect(written?.bytes).toBe(1);
  });

  it('returns a navigation that did not arrive as a 200 with its error', async () => {
    const { app, engine } = await start();
    const id = await open(app);
    engine.onNavigate = () => Promise.resolve(exampleOf<PageResult>('PageResult', 1));
    const response = await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'));
    expect(response.statusCode).toBe(200);
    expect(response.json<PageResult>().error).toMatchObject({ code: 'timeout' });
  });

  it('exports, imports and self-tests through the engine', async () => {
    const { app } = await start();
    const id = await open(app);
    for (const [url, request, answer] of [
      ['state/export', 'ExportStateRequest', 'ExportStateResponse'],
      ['state/import', 'ImportStateRequest', 'ImportStateResponse'],
      ['selftest', 'SelftestRequest', 'SelftestResponse'],
    ] as const) {
      const response = await post(app, `/v1/contexts/${id}/${url}`, exampleOf(request));
      expect(response.statusCode, response.body).toBe(200);
      expect(response.json()).toEqual(exampleOf(answer));
    }
  });

  it('refuses a second operation while one runs, and does not queue it', async () => {
    const { app, engine } = await start();
    const id = await open(app);
    let release: (result: PageResult) => void = () => undefined;
    engine.onNavigate = () => new Promise((resolve) => (release = resolve));
    const running = post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'));
    await new Promise((resolve) => setTimeout(resolve, 20));
    expectError(
      await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest')),
      409,
      'context_busy',
    );
    expectError(
      await post(app, `/v1/contexts/${id}/state/export`, exampleOf('ExportStateRequest')),
      409,
      'context_busy',
    );
    expectError(
      await app.server.inject({ method: 'DELETE', url: `/v1/contexts/${id}`, headers: AUTH }),
      409,
      'context_busy',
    );
    release(exampleOf<PageResult>('PageResult'));
    expect((await running).statusCode).toBe(200);
    // Free again.
    engine.onNavigate = () => Promise.resolve(exampleOf<PageResult>('PageResult'));
    expect(
      (await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'))).statusCode,
    ).toBe(200);
  });

  it('answers not_implemented for a flow, after validating it', async () => {
    const { app } = await start();
    const id = await open(app);
    expectError(
      await post(app, `/v1/contexts/${id}/flow`, exampleOf('FlowRequest')),
      501,
      'not_implemented',
    );
    expectError(
      await post(app, `/v1/contexts/${id}/flow`, { actions: [] }),
      400,
      'invalid_request',
    );
    expectError(
      await post(app, '/v1/contexts/not-a-uuid/flow', exampleOf('FlowRequest')),
      400,
      'invalid_request',
    );
  });
});

describe('what goes wrong', () => {
  it('refuses a body that is not what the contract says, naming the field', async () => {
    const { app, engine } = await start();
    const id = await open(app);
    const bad = await post(app, `/v1/contexts/${id}/navigate`, {
      ...exampleOf<object>('NavigateRequest'),
      url: 'file:///etc/passwd',
    });
    expectError(bad, 400, 'invalid_request');
    expect(ErrorResponseSchema.parse(bad.json()).message).toContain('url');
    expectError(
      await post(app, `/v1/contexts/${id}/navigate`, {
        ...exampleOf<object>('NavigateRequest'),
        extra: 1,
      }),
      400,
      'invalid_request',
    );
    expectError(
      await post(app, '/v1/contexts/42/navigate', exampleOf('NavigateRequest')),
      400,
      'invalid_request',
    );
    expectError(
      await post(app, '/v1/contexts', {
        ...exampleOf<object>('OpenContextRequest'),
        profile_dir: '../../etc',
      }),
      400,
      'invalid_request',
    );
    expect(engine.contexts[0]?.navigated).toHaveLength(0);
  });

  it('refuses a body that is not JSON, a missing body and one over the limit', async () => {
    const { app } = await start({ bodyLimitBytes: 2048 });
    const headers = { ...AUTH, 'content-type': 'application/json' };
    expectError(
      await app.server.inject({
        method: 'POST',
        url: '/v1/contexts',
        headers,
        payload: '{ not json',
      }),
      400,
      'invalid_request',
    );
    expectError(
      await app.server.inject({ method: 'POST', url: '/v1/contexts', headers: AUTH }),
      400,
      'invalid_request',
    );
    expectError(
      await app.server.inject({
        method: 'POST',
        url: '/v1/contexts',
        headers: { ...AUTH, 'content-type': 'text/plain' },
        payload: 'x',
      }),
      400,
      'invalid_request',
    );
    expectError(
      await app.server.inject({
        method: 'POST',
        url: '/v1/contexts',
        headers,
        payload: JSON.stringify({ pad: 'x'.repeat(4096) }),
      }),
      413,
      'payload_too_large',
    );
  });

  it('answers 404 for a route that does not exist', async () => {
    const { app } = await start();
    const response = await app.server.inject({
      method: 'GET',
      url: '/v1/contexts?token=x',
      headers: AUTH,
    });
    expectError(response, 404, 'not_found');
    expect(ErrorResponseSchema.parse(response.json()).message).toBe('no route GET /v1/contexts');
  });

  it("passes an engine's refusal on with its code and status", async () => {
    const { app, engine } = await start();
    engine.beforeOpen = () =>
      Promise.reject(new EngineError('browser_launch_failed', 'Chrome did not start: exit 1'));
    const response = await post(app, '/v1/contexts', exampleOf('OpenContextRequest'));
    expectError(response, 502, 'browser_launch_failed');
    expect(ErrorResponseSchema.parse(response.json()).message).toBe('Chrome did not start: exit 1');
    engine.beforeOpen = () =>
      Promise.reject(new EngineError('cross_os_claim_disabled', 'linux only'));
    expectError(
      await post(app, '/v1/contexts', exampleOf('OpenContextRequest')),
      422,
      'cross_os_claim_disabled',
    );
    // A failed open leaves no context behind and frees the slot.
    engine.beforeOpen = () => Promise.resolve();
    await open(app);
  });

  it('hides an unexpected failure from the caller and closes the context it happened in', async () => {
    const { app, engine } = await start();
    const id = await open(app);
    engine.onNavigate = () =>
      Promise.reject(new Error('Target crashed while loading https://user:secret@example.org/'));
    const response = await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'));
    expectError(response, 500, 'internal');
    expect(response.body).not.toContain('secret');
    expect(response.body).not.toContain('Target crashed');
    expect(engine.contexts[0]?.closes).toBe(1);
    expectError(
      await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest')),
      404,
      'context_not_found',
    );
  });

  it('never sends an answer that is outside the contract', async () => {
    const { app, engine } = await start();
    const id = await open(app);
    engine.onNavigate = () =>
      Promise.resolve({
        ...exampleOf<PageResult>('PageResult'),
        status: 'two hundred',
      } as unknown as PageResult);
    expectError(
      await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest')),
      500,
      'internal',
    );
  });
});

describe('lifecycle and metrics', () => {
  it('closes idle contexts and keeps the ones in use', async () => {
    const { app, engine, clock } = await start({ contextIdleMs: 60_000 });
    const idle = await open(app);
    const used = await open(app, { persona_id: '0199c1f2-6b7a-7c3e-9d41-000000000002' });
    clock.now += 50_000;
    expect(await app.contexts.sweep()).toBe(0);
    await post(app, `/v1/contexts/${used}/navigate`, exampleOf('NavigateRequest'));
    clock.now += 20_000;
    expect(await app.contexts.sweep()).toBe(1);
    expect(engine.contexts[0]?.closes).toBe(1);
    expect(engine.contexts[1]?.closes).toBe(0);
    expectError(
      await post(app, `/v1/contexts/${idle}/navigate`, exampleOf('NavigateRequest')),
      404,
      'context_not_found',
    );
    // The persona's profile is on disk; opening again just works.
    await open(app);
    expect(engine.contexts).toHaveLength(3);
  });

  it("restarts a context's browser after its navigation limit, under the same id", async () => {
    const { app, engine } = await start({ maxPagesPerBrowser: 2 });
    const id = await open(app);
    for (let index = 0; index < 5; index += 1) {
      expect(
        (await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'))).statusCode,
      ).toBe(200);
    }
    // Navigations 1-2 on the first browser, 3-4 on the second, 5 on the third.
    expect(engine.contexts.map((context) => [context.navigated.length, context.closes])).toEqual([
      [2, 1],
      [2, 1],
      [1, 0],
    ]);
    expect(engine.contexts[2]?.request).toEqual(engine.contexts[0]?.request);
    // State calls do not count toward the limit.
    await post(app, `/v1/contexts/${id}/state/export`, exampleOf('ExportStateRequest'));
    expect(engine.contexts).toHaveLength(3);
  });

  it('exposes Prometheus metrics for what happened', async () => {
    const { app, engine } = await start();
    const id = await open(app);
    await open(app);
    await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'));
    engine.onNavigate = () => Promise.resolve(exampleOf<PageResult>('PageResult', 1));
    await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'));
    engine.onNavigate = () => Promise.reject(new EngineError('internal', 'boom'));
    await post(app, `/v1/contexts/${id}/navigate`, exampleOf('NavigateRequest'));
    await post(app, `/v1/contexts/${id}/selftest`, exampleOf('SelftestRequest'));
    await app.server.inject({ method: 'DELETE', url: `/v1/contexts/${id}`, headers: AUTH });

    const response = await app.server.inject({ method: 'GET', url: '/v1/metrics', headers: AUTH });
    expect(response.statusCode).toBe(200);
    expect(response.headers['content-type']).toContain('text/plain');
    const text = response.body;
    for (const line of [
      'sidecar_contexts_open{engine="chromium"} 0',
      'sidecar_context_opens_total{engine="chromium",result="opened"} 1',
      'sidecar_context_opens_total{engine="chromium",result="reused"} 1',
      'sidecar_context_closes_total{engine="chromium",reason="requested"} 1',
      'sidecar_navigation_duration_seconds_count{engine="chromium",outcome="ok"} 1',
      'sidecar_navigation_duration_seconds_count{engine="chromium",outcome="timeout"} 1',
      'sidecar_navigation_duration_seconds_count{engine="chromium",outcome="error"} 1',
      'sidecar_selftests_total{engine="chromium",result="incomplete"} 1',
    ]) {
      expect(text).toContain(line);
    }
    // No label tells which persona went where.
    expect(text).not.toMatch(/persona|example\.org/);
  });

  it('collects the process metrics unless told not to', async () => {
    const { app } = await start({ processMetrics: true });
    expect(await app.metrics.render()).toContain('process_cpu_user_seconds_total');
  });

  it('closes every context and the engine when the server closes', async () => {
    const { app, engine } = await start();
    await open(app);
    await open(app, { persona_id: '0199c1f2-6b7a-7c3e-9d41-000000000002' });
    await app.server.close();
    expect(engine.contexts.map((context) => context.closes)).toEqual([1, 1]);
    expect(engine.closes).toBe(1);
    expect(app.contexts.count).toBe(0);
  });

  it('logs requests when asked to, without the secret', async () => {
    const { app } = await start({ logLevel: 'silent' });
    expect(
      (await app.server.inject({ method: 'GET', url: '/v1/health', headers: AUTH })).statusCode,
    ).toBe(200);
  });
});
