/**
 * The whole sidecar through its HTTP contract: the real server, the real
 * Chromium engine, real Chrome, through a local authenticated proxy against
 * the fixture site. Everything a caller sends and receives is checked
 * against the contract's schemas, as Kage's generated types would.
 */
import { mkdtemp, readFile } from 'node:fs/promises';
import { tmpdir, userInfo } from 'node:os';
import path from 'node:path';

import {
  ExportStateResponseSchema,
  FlowEventSchema,
  GeneratePersonaResponseSchema,
  HealthResponseSchema,
  OpenContextResponseSchema,
  PageResultSchema,
  SelftestResponseSchema,
  TOKEN_HEADER,
  type NavigateRequest,
  type OpenContextRequest,
} from '@kage/contract';
import {
  startFixtureSite,
  startTestProxy,
  type FixtureSite,
  type TestProxy,
} from '@kage/engine-chromium/testing';
import { silentLog } from '@kage/engine-core';
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';

import { buildApp, type App } from './app.js';
import { loadEngine } from './engine-loader.js';
import { probeHealth } from './probe.js';

vi.setConfig({ testTimeout: 120_000, hookTimeout: 120_000 });

const TOKEN = 'e2e-token-0123456789abcdef-0123456789';
const SEED = 'a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90';
const PERSONA = '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e';
const LANGUAGES = ['fr-FR', 'fr', 'en-US', 'en'];

let site: FixtureSite;
let proxy: TestProxy;
let app: App;
let origin: string;
let artifactsDir: string;

async function call(
  method: string,
  pathname: string,
  body?: unknown,
): Promise<{ status: number; json: unknown }> {
  const response = await fetch(`${origin}${pathname}`, {
    method,
    headers: {
      [TOKEN_HEADER]: TOKEN,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const text = await response.text();
  return { status: response.status, json: text === '' ? null : (JSON.parse(text) as unknown) };
}

beforeAll(async () => {
  if (userInfo().uid === 0) {
    throw new Error(
      'these tests start Chrome with its sandbox, which does not run as root: run them as another user',
    );
  }
  site = await startFixtureSite();
  proxy = await startTestProxy({ httpPort: site.port });
  const root = await mkdtemp(path.join(tmpdir(), 'kage-e2e-'));
  artifactsDir = path.join(root, 'artifacts');
  const engine = await loadEngine('chromium', {
    personasDir: path.join(root, 'personas'),
    allowCrossOsClaims: false,
    gl: 'llvmpipe',
    selftestPages: [
      { id: 'probe', url: `http://fixture.localhost:${site.port}/probe/`, kind: 'probe' },
    ],
    log: silentLog,
  });
  app = buildApp({
    engine,
    token: TOKEN,
    artifactsDir,
    maxContexts: 1,
    contextIdleMs: 120_000,
    maxPagesPerBrowser: 2,
    processMetrics: false,
  });
  origin = await app.server.listen({ host: '127.0.0.1', port: 0 });
});

afterAll(async () => {
  await app.server.close();
  await proxy.stop();
  await site.stop();
});

describe('the sidecar through its HTTP contract', () => {
  it('generates a persona, opens it, navigates, exports state, tests itself and closes', async () => {
    const health = await probeHealth(origin, TOKEN);
    expect(health).toMatchObject({ engine: 'chromium', contexts_in_use: 0, max_contexts: 1 });
    expect(health.browser_version).toMatch(/^\d+\.\d+\.\d+\.\d+$/);
    const major = Number(health.browser_version?.split('.')[0]);

    const generated = await call('POST', '/v1/personas/generate', {
      seed: SEED,
      claimed_os: 'linux',
      languages: LANGUAGES,
      browser_major: major,
      screen: { min_width: 1920, max_width: 1920, min_height: 1080, max_height: 1080 },
    });
    expect(generated.status).toBe(200);
    const persona = GeneratePersonaResponseSchema.parse(generated.json);
    expect(persona.summary).toMatchObject({
      claimed_os: 'linux',
      browser_major: major,
      inject: 'none',
      seed: SEED,
    });
    // The same seed over HTTP gives the same document.
    expect(
      (
        await call('POST', '/v1/personas/generate', {
          seed: SEED,
          claimed_os: 'linux',
          languages: LANGUAGES,
          screen: { min_width: 1920, max_width: 1920, min_height: 1080, max_height: 1080 },
        })
      ).json,
    ).toEqual(generated.json);
    // A Windows claim is refused while such claims are off.
    const refused = await call('POST', '/v1/personas/generate', {
      seed: SEED,
      claimed_os: 'windows',
      languages: LANGUAGES,
    });
    expect([refused.status, (refused.json as { code: string }).code]).toEqual([
      422,
      'cross_os_claim_disabled',
    ]);

    const openRequest: OpenContextRequest = {
      persona_id: PERSONA,
      fingerprint: persona.fingerprint,
      proxy: { server: proxy.server, username: proxy.username, password: proxy.password },
      locale: 'fr-FR',
      languages: LANGUAGES,
      timezone: 'Europe/Paris',
      geolocation: null,
      screen: persona.summary.screen,
      profile_dir: `personas/${PERSONA}`,
    };
    const openedResponse = await call('POST', '/v1/contexts', openRequest);
    expect(openedResponse.status).toBe(201);
    const opened = OpenContextResponseSchema.parse(openedResponse.json);
    expect(opened).toMatchObject({
      persona_id: PERSONA,
      reused: false,
      browser_version: health.browser_version,
      applied: {
        claimed_os: 'linux',
        timezone: 'Europe/Paris',
        languages: LANGUAGES,
        inject: 'none',
        gl: 'llvmpipe',
      },
    });
    const id = opened.context_id;
    expect(
      OpenContextResponseSchema.parse((await call('POST', '/v1/contexts', openRequest)).json),
    ).toMatchObject({
      context_id: id,
      reused: true,
    });
    // The cap of one context holds for another persona.
    const second = await call('POST', '/v1/contexts', {
      ...openRequest,
      persona_id: '0199c1f2-6b7a-7c3e-9d41-000000000009',
      profile_dir: 'personas/other',
    });
    expect([second.status, (second.json as { code: string }).code]).toEqual([409, 'context_limit']);

    const navigate: NavigateRequest = {
      url: 'http://fixture.kage.test/',
      wait: { until: 'network_idle', selector: 'body[data-items]' },
      capture: {
        dom: true,
        raw: false,
        xhr: { content_types: ['application/json'], max_bytes: 100_000 },
        console: true,
        screenshot: { full_page: false, format: 'png' },
        pdf: null,
      },
    };
    const navigated = await call('POST', `/v1/contexts/${id}/navigate`, navigate);
    expect(navigated.status).toBe(200);
    const page = PageResultSchema.parse(navigated.json);
    expect(page).toMatchObject({
      status: 200,
      title: 'Fixture home',
      error: null,
      side_effects: [],
    });
    expect(page.dom_html).toContain('visits: 1');
    expect(page.xhr.map((entry) => new URL(entry.url).pathname).sort()).toEqual([
      '/api/items',
      '/api/time',
    ]);
    // The screenshot is on the volume the sidecar shares with Kage, below the context's directory.
    expect(page.screenshot?.path.startsWith(`sidecar/${id}/`)).toBe(true);
    const image = await readFile(path.join(artifactsDir, page.screenshot?.path ?? ''));
    expect(image.subarray(1, 4).toString('latin1')).toBe('PNG');
    // The target saw the persona's languages and the browser's own headers.
    const request = (await site.requests()).find((entry) => entry.path === '/');
    expect(request?.headers.find(([name]) => name === 'Accept-Language')?.[1]).toBe(
      'fr-FR,fr;q=0.9,en-US;q=0.8,en;q=0.7',
    );

    const exported = ExportStateResponseSchema.parse(
      (
        await call('POST', `/v1/contexts/${id}/state/export`, {
          origin: 'http://fixture.kage.test',
        })
      ).json,
    );
    expect(exported.cookies.map((cookie) => [cookie.name, cookie.value, cookie.same_site])).toEqual(
      [['visits', '1', 'lax']],
    );

    const selftest = SelftestResponseSchema.parse(
      (
        await call('POST', `/v1/contexts/${id}/selftest`, {
          expected: {
            claimed_os: 'linux',
            browser_major: major,
            timezone: 'Europe/Paris',
            languages: LANGUAGES,
          },
        })
      ).json,
    );
    expect(
      selftest.findings.filter(
        (finding) => finding.status !== 'pass' && !finding.check.startsWith('environment.'),
      ),
    ).toEqual([]);
    expect(selftest).toMatchObject({ passed: true, complete: true });
    expect(selftest.score).toBeGreaterThanOrEqual(98);
    expect(selftest.pages[0]?.text?.path).toBe(`sidecar/${id}/selftest/probe.txt`);

    // Two navigations were the limit of this browser: the next one runs on
    // a new browser with the same profile, so the visit counter goes on.
    const third = PageResultSchema.parse(
      (await call('POST', `/v1/contexts/${id}/navigate`, navigate)).json,
    );
    expect(third.dom_html).toContain('visits: 2');

    // A flow answers line by line: one per step, then the page as it was
    // left. It starts in a fresh tab (F96), so it loads its page itself.
    const flow = await fetch(`${origin}/v1/contexts/${id}/flow`, {
      method: 'POST',
      headers: { [TOKEN_HEADER]: TOKEN, 'content-type': 'application/json' },
      body: JSON.stringify({
        actions: [
          { action: 'goto', url: navigate.url },
          { action: 'click', selector: '#next' },
          { action: 'wait_for', selector: '#referer' },
          { action: 'assert', text_contains: 'Listing' },
        ],
        capture: navigate.capture,
      }),
    });
    expect(flow.status).toBe(200);
    expect(flow.headers.get('content-type')).toBe('application/x-ndjson; charset=utf-8');
    const events = (await flow.text())
      .trimEnd()
      .split('\n')
      .map((line) => FlowEventSchema.parse(JSON.parse(line)));
    expect(
      events.map((event) => (event.type === 'step' ? [event.action, event.ok] : event.ok)),
    ).toEqual([['goto', true], ['click', true], ['wait_for', true], ['assert', true], true]);
    const final = events.at(-1);
    expect(final?.type === 'final' && final.result.final_url).toMatch(/\/listing\?page=2$/);
    expect(final?.type === 'final' && final.behavior?.clicks).toBe(1);

    const metrics = await (
      await fetch(`${origin}/v1/metrics`, { headers: { [TOKEN_HEADER]: TOKEN } })
    ).text();
    expect(metrics).toContain('sidecar_contexts_open{engine="chromium"} 1');
    expect(metrics).toContain(
      'sidecar_navigation_duration_seconds_count{engine="chromium",outcome="ok"} 2',
    );
    expect(metrics).toContain('sidecar_browser_recycles_total{engine="chromium"} 1');
    expect(metrics).toContain('sidecar_selftests_total{engine="chromium",result="passed"} 1');

    expect((await call('DELETE', `/v1/contexts/${id}`)).status).toBe(204);
    expect(HealthResponseSchema.parse((await call('GET', '/v1/health')).json).contexts_in_use).toBe(
      0,
    );
    expect((await call('DELETE', `/v1/contexts/${id}`)).status).toBe(404);
    // Nothing answers without the secret.
    expect((await fetch(`${origin}/v1/health`)).status).toBe(401);
  });
});
