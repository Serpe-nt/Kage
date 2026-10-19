/**
 * Integration tests of the Chromium engine (spec §21): real Chrome, driven
 * by patchright, on its own X display, through a local authenticated proxy,
 * against the fixture site. Nothing here leaves the machine: the test proxy
 * refuses every tunnel, which is where the browser's own background traffic
 * would go.
 *
 * They need the pinned Chrome and Xvfb (`just setup-chrome`) and a user
 * that is not root, because Chrome's sandbox does not run as root.
 */
import { execFileSync } from 'node:child_process';
import dgram from 'node:dgram';
import { mkdtemp, readFile, readdir } from 'node:fs/promises';
import { networkInterfaces, tmpdir, userInfo } from 'node:os';
import path from 'node:path';

import type {
  CaptureOptions,
  NavigateRequest,
  OpenContextRequest,
  PageResult,
  SelftestResponse,
} from '@kage/contract';
import {
  ArtifactWriter,
  EngineError,
  parseProbeReport,
  silentLog,
  type EngineContext,
  type FingerprintDocument,
  type OperationEnv,
  type ProbeReport,
} from '@kage/engine-core';
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';

import {
  CHROME_SWITCHES,
  ChromiumEngine,
  LOOPBACK_THROUGH_PROXY,
  chromeUserAgentOf,
  detectHost,
  launchPlan,
  profilePaths,
  type HostFacts,
} from './index.js';
import {
  startFixtureSite,
  startTestProxy,
  type FixtureRequest,
  type FixtureSite,
  type TestProxy,
} from '@kage/engine-core/testing';

vi.setConfig({ testTimeout: 120_000, hookTimeout: 120_000 });

const SEED = '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac';
const LANGUAGES = ['de-DE', 'de', 'en-US', 'en'];
const SCREEN = { min_width: 1920, max_width: 1920, min_height: 1080, max_height: 1080 };
const NO_CAPTURE: CaptureOptions = {
  dom: false,
  raw: false,
  xhr: null,
  console: false,
  screenshot: null,
  pdf: null,
};

let site: FixtureSite;
let proxy: TestProxy;
let host: HostFacts;
let root: string;
let engine: ChromiumEngine;
let crossOsEngine: ChromiumEngine;
let env: OperationEnv;
let linux: FingerprintDocument;

/** A name only the test proxy can resolve: plain http, not a secure context. */
const named = (pathname: string) => `http://fixture.kage.test${pathname}`;
/** A loopback name: a secure context, so client hints are sent, and still through the proxy. */
const secure = (pathname: string) => `http://fixture.localhost:${site.port}${pathname}`;

function openRequest(
  document: FingerprintDocument,
  profile: string,
  change: Partial<OpenContextRequest> = {},
): OpenContextRequest {
  return {
    persona_id: '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e',
    fingerprint: document,
    proxy: { server: proxy.server, username: proxy.username, password: proxy.password },
    locale: 'de-DE',
    languages: LANGUAGES,
    timezone: 'Europe/Berlin',
    geolocation: { latitude: 52.52, longitude: 13.405, accuracy_m: 35 },
    screen: document.native.screen,
    profile_dir: `personas/${profile}`,
    ...change,
  };
}

const go = (
  context: EngineContext,
  url: string,
  change: Partial<NavigateRequest> = {},
): Promise<PageResult> =>
  context.navigate({ url, wait: { until: 'load' }, capture: NO_CAPTURE, ...change }, env);

async function withContext<T>(
  profile: string,
  run: (context: EngineContext) => Promise<T>,
  options: {
    engine?: ChromiumEngine;
    document?: FingerprintDocument;
    change?: Partial<OpenContextRequest>;
  } = {},
): Promise<T> {
  const context = await (options.engine ?? engine).openContext(
    openRequest(options.document ?? linux, profile, options.change),
  );
  try {
    return await run(context);
  } finally {
    await context.close();
  }
}

const headerNames = (request: FixtureRequest | undefined) =>
  request?.headers.map(([name]) => name) ?? [];
const headerOf = (request: FixtureRequest | undefined, name: string) =>
  request?.headers.find(([key]) => key.toLowerCase() === name)?.[1] ?? null;
const lastRequest = async (pathname: string) =>
  (await site.requests()).filter((request) => request.path === pathname).at(-1);

/** The report the probe page published, read from a captured DOM. */
function probeReport(result: PageResult): ProbeReport {
  const json =
    /<script type="application\/json" id="kage-probe-report"[^>]*>([\s\S]*?)<\/script>/.exec(
      result.dom_html ?? '',
    )?.[1];
  const report = parseProbeReport(json ?? '');
  if (report === null) {
    throw new Error(
      `the probe published no report: ${result.error?.message ?? result.dom_html?.slice(0, 200) ?? ''}`,
    );
  }
  return report;
}

const probe = (context: EngineContext) =>
  go(context, secure('/probe/'), {
    wait: { until: 'load', selector: '#kage-probe-report[data-done]' },
    capture: { ...NO_CAPTURE, dom: true },
  }).then(probeReport);

beforeAll(async () => {
  if (userInfo().uid === 0) {
    throw new Error(
      'these tests start Chrome with its sandbox, which does not run as root: run them as another user',
    );
  }
  host = await detectHost();
  site = await startFixtureSite();
  proxy = await startTestProxy({ httpPort: site.port });
  root = await mkdtemp(path.join(tmpdir(), 'kage-chromium-'));
  const options = {
    personasDir: path.join(root, 'personas'),
    gl: 'llvmpipe' as const,
    selftestPages: [{ id: 'probe', url: secure('/probe/'), kind: 'probe' as const }],
    log: silentLog,
  };
  engine = new ChromiumEngine({ ...options, allowCrossOsClaims: false }, host);
  crossOsEngine = new ChromiumEngine({ ...options, allowCrossOsClaims: true }, host);
  env = {
    artifacts: new ArtifactWriter(path.join(root, 'artifacts')),
    defaultArtifactPrefix: 'sidecar/test',
  };
  linux = (
    await engine.generatePersona({
      seed: SEED,
      claimed_os: 'linux',
      languages: LANGUAGES,
      screen: SCREEN,
    })
  ).fingerprint as FingerprintDocument;
});

afterAll(async () => {
  await engine.close();
  await crossOsEngine.close();
  await proxy.stop();
  await site.stop();
});

describe('navigate through an authenticated proxy', () => {
  it('captures the page, its XHR, a screenshot and a PDF, and writes the files', async () => {
    await site.reset();
    const result = await withContext('capture', (context) =>
      go(context, named('/'), {
        wait: { until: 'network_idle', selector: 'body[data-items]' },
        capture: {
          dom: true,
          raw: true,
          xhr: { content_types: ['application/json'], max_bytes: 1_000_000 },
          console: true,
          screenshot: { full_page: false, format: 'webp', quality: 70 },
          pdf: {},
        },
        artifact_prefix: 'requests/r1',
      }),
    );

    expect(result.error).toBeNull();
    expect(result).toMatchObject({
      url: named('/'),
      final_url: named('/'),
      status: 200,
      http_version: 'http/1.1',
      title: 'Fixture home',
      redirects: [],
      dom_truncated: false,
      xhr_dropped: 0,
      console_capture: 'browser_log',
      challenge: {
        interstitial: false,
        marker: null,
        resolved: null,
        vendors: [],
        script_hosts: ['fixture.kage.test'],
      },
    });
    expect(result.headers.find((header) => header.name === 'content-type')?.value).toBe(
      'text/html; charset=utf-8',
    );
    expect(
      result.headers.some(
        (header) => header.name === 'set-cookie' && header.value.startsWith('visits=1'),
      ),
    ).toBe(true);

    // The rendered DOM has what the page's script added; the raw body does not.
    expect(result.dom_html).toContain('<li>alpha</li>');
    expect(result.dom_html).toContain('visits: 1');
    expect(result.raw_body).toMatchObject({ truncated: false, encoding: 'utf8' });
    expect(result.raw_body?.data).toContain('<ul id="items"></ul>');
    expect(result.raw_body?.data).not.toContain('<li>');

    // XHR and fetch() responses that match the filter, with their bodies.
    const xhr = Object.fromEntries(result.xhr.map((entry) => [new URL(entry.url).pathname, entry]));
    expect(Object.keys(xhr).sort()).toEqual(['/api/items', '/api/time']);
    expect(xhr['/api/items']).toMatchObject({
      method: 'GET',
      kind: 'fetch',
      status: 200,
      content_type: 'application/json',
    });
    expect(JSON.parse(xhr['/api/items']?.body?.data ?? '')).toEqual({
      items: [{ name: 'alpha' }, { name: 'beta' }, { name: 'gamma' }],
    });
    expect(xhr['/api/time']).toMatchObject({ kind: 'xhr', content_type: 'application/json' });
    expect(JSON.parse(xhr['/api/time']?.body?.data ?? '')).toEqual({ now: '2026-10-09T12:00:00Z' });

    // The files are on the artifacts volume and are what they say they are.
    expect(result.screenshot).toMatchObject({ content_type: 'image/webp' });
    expect(result.screenshot?.path).toMatch(/^requests\/r1\/[0-9a-z]+-screenshot\.webp$/);
    const image = await readFile(path.join(root, 'artifacts', result.screenshot?.path ?? ''));
    expect(image.subarray(0, 4).toString('latin1')).toBe('RIFF');
    expect(image.subarray(8, 12).toString('latin1')).toBe('WEBP');
    expect(image.byteLength).toBe(result.screenshot?.bytes);
    expect(result.pdf).toMatchObject({ content_type: 'application/pdf' });
    const pdf = await readFile(path.join(root, 'artifacts', result.pdf?.path ?? ''));
    expect(pdf.subarray(0, 5).toString('latin1')).toBe('%PDF-');
    expect(pdf.byteLength).toBe(result.pdf?.bytes);
    // A viewport screenshot is invisible to the page; printing is not.
    expect(result.side_effects).toEqual(['beforeprint', 'afterprint']);

    expect(result.timing.ttfb_ms).toBeGreaterThanOrEqual(0);
    expect(result.timing.dom_content_loaded_ms).toBeGreaterThanOrEqual(result.timing.ttfb_ms ?? 0);
    expect(result.timing.load_ms).toBeGreaterThanOrEqual(result.timing.dom_content_loaded_ms ?? 0);
    expect(result.timing.total_ms).toBeGreaterThanOrEqual(result.timing.load_ms ?? 0);

    // Everything went through the proxy with the persona's credentials: the
    // site's name exists nowhere else.
    const relayed = proxy.log.filter(
      (entry) => entry.target === 'fixture.kage.test' && entry.relayed,
    );
    expect(relayed.map((entry) => entry.path)).toEqual(
      expect.arrayContaining(['/', '/app.js', '/api/items?source=fetch', '/api/time']),
    );
    expect(relayed.every((entry) => entry.authorized)).toBe(true);
    // The target never saw the proxy credentials.
    for (const request of await site.requests()) {
      expect(headerOf(request, 'proxy-authorization')).toBeNull();
    }
  });

  it('follows redirects and reports every hop', async () => {
    const result = await withContext('redirects', (context) =>
      go(context, named('/redirect/a'), { capture: { ...NO_CAPTURE, dom: true } }),
    );
    expect(result).toMatchObject({
      url: named('/redirect/a'),
      final_url: named('/landing?from=redirect'),
      status: 200,
      title: 'Landing',
      redirects: [
        { url: named('/redirect/a'), status: 302, location: '/redirect/b' },
        { url: named('/redirect/b'), status: 301, location: '/landing?from=redirect' },
      ],
    });
    expect(result.headers.find((header) => header.name === 'x-fixture')?.value).toBe('landing');
  });

  it('sends loopback targets through the proxy too', async () => {
    const before = proxy.log.length;
    const result = await withContext('loopback', (context) =>
      go(context, `http://127.0.0.1:${site.port}/landing`),
    );
    expect(result.status).toBe(200);
    const asked = proxy.log
      .slice(before)
      .filter((entry) => entry.target === `127.0.0.1:${site.port}`);
    expect(asked.map((entry) => entry.path)).toContain('/landing');
  });

  it('navigates the way a typed address does: no referer, no site', async () => {
    await withContext('typed', async (context) => {
      await go(context, secure('/listing?page=1'));
      const request = await lastRequest('/listing?page=1');
      expect(headerOf(request, 'referer')).toBeNull();
      expect(headerOf(request, 'sec-fetch-site')).toBe('none');
      expect(headerOf(request, 'sec-fetch-user')).toBe('?1');
    });
  });
});

describe('what captures leave behind in the page', () => {
  it('leaves nothing for a DOM read and a viewport screenshot, a resize for a full page, two events for a PDF', async () => {
    // What one load of the events page noticed while it was the page. Its
    // own farewell (it is navigated away from later) does not count.
    const events = async (run: string) =>
      (await site.reports())
        .filter((report) => report['page'] === 'events' && report['run'] === run)
        .map((report) => String(report['what']))
        .filter((event) => event !== 'pagehide' && event !== 'visibilitychange');

    await withContext('events', async (context) => {
      await site.reset();
      const plain = await go(context, named('/events.html?plain'), {
        wait: { until: 'load', selector: 'body[data-ready]', extra_ms: 300 },
        capture: {
          ...NO_CAPTURE,
          dom: true,
          raw: true,
          console: true,
          screenshot: { full_page: false, format: 'png' },
        },
      });
      expect(plain.side_effects).toEqual([]);
      const image = await readFile(path.join(root, 'artifacts', plain.screenshot?.path ?? ''));
      expect(image.subarray(1, 4).toString('latin1')).toBe('PNG');
      await new Promise((resolve) => setTimeout(resolve, 300));
      // The page saw itself load and nothing else: no resize, no DOM change.
      expect(await events('plain')).toEqual(['ready']);

      const full = await go(context, named('/events.html?full'), {
        wait: { until: 'load', selector: 'body[data-ready]', extra_ms: 300 },
        capture: { ...NO_CAPTURE, screenshot: { full_page: true, format: 'jpeg' } },
      });
      expect(full.side_effects).toEqual(['resize']);
      expect(full.screenshot?.path).toMatch(/-screenshot\.jpg$/);
      await new Promise((resolve) => setTimeout(resolve, 300));
      // The whole document cannot be captured unnoticed: the page sees its
      // viewport change, once or twice, and nothing else.
      const noticed = (await events('full')).slice(1);
      expect(noticed.length).toBeGreaterThanOrEqual(1);
      expect(noticed.length).toBeLessThanOrEqual(2);
      expect(new Set(noticed)).toEqual(new Set(['resize']));

      const printed = await go(context, named('/events.html?printed'), {
        wait: { until: 'load', selector: 'body[data-ready]', extra_ms: 300 },
        capture: {
          ...NO_CAPTURE,
          pdf: { format: 'letter', landscape: true, print_background: false },
        },
      });
      expect(printed.side_effects).toEqual(['beforeprint', 'afterprint']);
      await new Promise((resolve) => setTimeout(resolve, 300));
      expect(await events('printed')).toEqual(['ready', 'beforeprint', 'afterprint']);
      // Nothing happened to the first load after its capture either.
      expect(await events('plain')).toEqual(['ready']);
    });
  });

  it("reads the browser's own log without touching the console API", async () => {
    await site.reset();
    const result = await withContext('console', (context) =>
      go(context, named('/console.html'), {
        wait: { until: 'load', extra_ms: 800 },
        capture: { ...NO_CAPTURE, console: true },
      }),
    );
    expect(result.console_capture).toBe('browser_log');
    expect(result.console).toEqual([
      expect.objectContaining({
        level: 'error',
        source: 'network',
        url: named('/missing-image.png'),
      }),
    ]);
    // The page logged an object whose getter counts reads: a debugger that
    // serializes console arguments would have read it.
    const reports = (await site.reports()).filter((report) => report['page'] === 'console');
    expect(reports).toEqual([{ page: 'console', stackReads: 0 }]);

    const silent = await withContext('console-off', (context) =>
      go(context, named('/console.html')),
    );
    expect(silent).toMatchObject({ console: [], console_capture: 'not_requested' });
  });
});

describe('a persona keeps its state and its values', () => {
  it('reuses cookies across two opens of the same persona and not across personas', async () => {
    const visit = (profile: string) =>
      withContext(profile, (context) =>
        go(context, named('/'), {
          wait: { until: 'load', selector: 'body[data-items]' },
          capture: { ...NO_CAPTURE, dom: true },
        }),
      );
    await site.reset();
    expect((await visit('returning')).dom_html).toContain('visits: 1');
    expect(headerOf(await lastRequest('/'), 'cookie')).toBeNull();
    // The browser was closed in between: the cookie came from the profile on disk.
    expect((await visit('returning')).dom_html).toContain('visits: 2');
    expect(headerOf(await lastRequest('/'), 'cookie')).toBe('visits=1');
    expect((await visit('somebody-else')).dom_html).toContain('visits: 1');
  });

  it('generates the same fingerprint for the same seed', async () => {
    const again = await engine.generatePersona({
      seed: SEED,
      claimed_os: 'linux',
      languages: LANGUAGES,
      screen: SCREEN,
    });
    expect(JSON.stringify(again.fingerprint)).toBe(JSON.stringify(linux));
    expect(again.summary).toMatchObject({
      engine: 'chromium',
      claimed_os: 'linux',
      browser_version: host.chromeVersion,
      seed: SEED,
      inject: 'none',
      screen: { width: 1920, height: 1080, dpr: 1, color_depth: 24 },
    });
  });

  it('shows identical navigator and screen values across context restarts', async () => {
    const read = () =>
      withContext('determinism', async (context) => ({
        applied: context.applied,
        scopes: (await probe(context)).scopes,
      }));
    const first = await read();
    const second = await read();
    expect(second).toEqual(first);
    expect(first.applied).toMatchObject({
      claimed_os: 'linux',
      screen: { width: 1920, height: 1080, dpr: 1, color_depth: 24 },
      timezone: 'Europe/Berlin',
      languages: LANGUAGES,
      gl: 'llvmpipe',
      inject: 'none',
      script_sha256: null,
    });
    // What the persona says is what the launch parameters made true, in
    // the window and in a worker alike.
    const expectedCpus = Math.min(linux.native.hardware_concurrency, host.cpus.length);
    expect(first.applied.hardware_concurrency).toBe(expectedCpus);
    for (const scope of [first.scopes.window, first.scopes.worker, first.scopes.blob_worker]) {
      expect(scope).toMatchObject({
        user_agent: first.applied.user_agent,
        platform: 'Linux x86_64',
        languages: LANGUAGES,
        hardware_concurrency: expectedCpus,
        timezone: 'Europe/Berlin',
        uad_platform: 'Linux',
        error: null,
      });
      expect(scope?.webgl?.renderer).toContain('llvmpipe');
    }
    expect(first.applied.user_agent).toBe(linux.user_agent);
  });

  it("exports and imports cookies as rows of http_cookies, and storage of the page's origin", async () => {
    await withContext('state', async (context) => {
      await go(context, named('/set-cookies'));
      const exported = await context.exportState({ origin: 'http://fixture.kage.test' });
      expect(exported.local_storage).toBeNull();
      const byName = Object.fromEntries(exported.cookies.map((cookie) => [cookie.name, cookie]));
      expect(Object.keys(byName).sort()).toEqual(['persistent', 'scoped', 'session_only']);
      expect(byName['session_only']).toEqual({
        domain: 'fixture.kage.test',
        host_only: true,
        path: '/',
        name: 'session_only',
        value: 's1',
        secure: false,
        http_only: false,
        same_site: null,
        expires_at: null,
      });
      expect(byName['persistent']).toMatchObject({
        http_only: true,
        same_site: 'strict',
        path: '/',
      });
      expect(new Date(byName['persistent']?.expires_at ?? 0).getTime()).toBeGreaterThan(Date.now());
      expect(byName['scoped']).toMatchObject({ path: '/account' });
      // Another origin has none of them.
      expect((await context.exportState({ origin: 'http://other.kage.test' })).cookies).toEqual([]);

      const sessionOnly = byName['session_only'];
      if (sessionOnly === undefined) {
        throw new Error('the export has no session_only cookie');
      }
      const imported = await context.importState({
        origin: 'http://fixture.kage.test',
        cookies: [
          {
            ...sessionOnly,
            name: 'from_http_path',
            value: 'h1',
            expires_at: new Date(Date.now() + 3_600_000).toISOString(),
          },
          {
            ...sessionOnly,
            name: 'wide',
            value: 'w1',
            domain: 'kage.test',
            host_only: false,
          },
          { ...sessionOnly, name: 'foreign', domain: 'example.org' },
          { ...sessionOnly, name: 'stale', expires_at: '2020-01-01T00:00:00.000Z' },
          { ...sessionOnly, name: '__Host-bad', domain: 'kage.test', host_only: false },
        ],
      });
      expect(imported).toEqual({
        imported: 2,
        rejected: [
          { name: 'foreign', domain: 'example.org', reason: 'foreign_domain' },
          { name: 'stale', domain: 'fixture.kage.test', reason: 'expired' },
          { name: '__Host-bad', domain: 'kage.test', reason: 'refused_by_browser' },
        ],
      });
      // The browser now sends them.
      const seen = await go(context, named('/cookies'), { capture: { ...NO_CAPTURE, raw: true } });
      const sent = (JSON.parse(seen.raw_body?.data ?? '{}') as { cookie: string }).cookie
        .split('; ')
        .sort();
      expect(sent).toEqual(['from_http_path=h1', 'persistent=p1', 'session_only=s1', 'wide=w1']);
      // An import and an export agree with each other.
      const roundTrip = (await context.exportState({ origin: 'http://fixture.kage.test' })).cookies;
      expect(roundTrip.find((cookie) => cookie.name === 'wide')).toMatchObject({
        domain: 'kage.test',
        host_only: false,
      });
      expect(roundTrip.find((cookie) => cookie.name === 'from_http_path')).toMatchObject({
        host_only: true,
      });

      await go(context, named('/storage.html'), {
        wait: { until: 'load', selector: 'body[data-ready]' },
      });
      const withStorage = await context.exportState({
        origin: 'http://fixture.kage.test',
        include_storage: true,
      });
      expect(withStorage.local_storage?.sort((a, b) => a.name.localeCompare(b.name))).toEqual([
        { name: 'seen', value: '1' },
        { name: 'theme', value: 'dark' },
      ]);
      expect(withStorage.session_storage).toEqual([{ name: 'step', value: '2' }]);
      // The page is not on that origin: nothing is loaded to get at its storage.
      const elsewhere = await context.exportState({
        origin: 'http://other.kage.test',
        include_storage: true,
      });
      expect(elsewhere).toMatchObject({ local_storage: null, session_storage: null });
    });
  });
});

describe('coherence (spec §6.2)', () => {
  let honestNavigation: string[] = [];

  it('passes for an honest Linux persona in window, frames and workers, with native headers and a working cache', async () => {
    await site.reset();
    const { selftest, cached } = await withContext('coherent', async (context) => {
      const result = await context.selftest(
        {
          expected: {
            claimed_os: 'linux',
            browser_major: Number(host.chromeVersion.split('.')[0]),
            timezone: 'Europe/Berlin',
            languages: LANGUAGES,
          },
        },
        env,
      );
      await go(context, named('/cached.html'));
      await go(context, named('/cached.html'));
      return {
        selftest: result,
        cached: (await site.requests()).filter((request) => request.path === '/cached.html'),
      };
    });

    // What the machine around the browser lacks (a sound card on a test
    // runner) is the image's to supply; the stack smoke test checks it there.
    const problems = selftest.findings.filter(
      (finding) => finding.status !== 'pass' && !finding.check.startsWith('environment.'),
    );
    expect(problems).toEqual([]);
    expect(selftest).toMatchObject({ passed: true, complete: true });
    // A warning about the test machine costs a little; nothing else does.
    expect(selftest.score).toBeGreaterThanOrEqual(98);
    const checks = selftest.findings.map((finding) => finding.check);
    for (const scope of [
      'window',
      'iframe_same_origin',
      'iframe_srcdoc',
      'iframe_blank',
      'worker',
      'blob_worker',
    ]) {
      expect(checks).toEqual(
        expect.arrayContaining([
          `os.${scope}`,
          `browser_major.${scope}`,
          `timezone.${scope}`,
          `languages.${scope}`,
        ]),
      );
    }
    expect(checks).toEqual(
      expect.arrayContaining([
        'automation.webdriver',
        'automation.console_getter',
        'automation.native_getters',
        'coherence.user_agent',
        'coherence.platform',
        'coherence.hint_platform',
        'coherence.hardware_concurrency',
        'coherence.webgl',
        'headers.navigation.user_agent',
        'headers.navigation.platform',
        'headers.navigation.cache',
        'headers.iframe.platform',
        'headers.fetch.platform',
        'headers.worker_script.user_agent',
        'headers.worker_fetch.user_agent',
      ]),
    );
    expect(selftest.pages).toEqual([
      expect.objectContaining({ id: 'probe', kind: 'probe', status: 'ok', http_status: 200 }),
    ]);
    const evidence = await readdir(path.join(root, 'artifacts', 'sidecar', 'test', 'selftest'));
    expect(evidence.sort()).toEqual(['probe.png', 'probe.txt']);

    // Chrome's own header order on a typed navigation, with the persona's languages.
    const navigation = (await site.requests()).find((request) => request.path === '/probe/');
    honestNavigation = headerNames(navigation);
    // (Through a proxy the browser's connection header is for the proxy.)
    expect(honestNavigation).toEqual([
      'Host',
      'sec-ch-ua',
      'sec-ch-ua-mobile',
      'sec-ch-ua-platform',
      'Upgrade-Insecure-Requests',
      'User-Agent',
      'Accept',
      'Sec-Fetch-Site',
      'Sec-Fetch-Mode',
      'Sec-Fetch-User',
      'Sec-Fetch-Dest',
      'Accept-Encoding',
      'Accept-Language',
      'Connection',
    ]);
    expect(headerOf(navigation, 'accept-language')).toBe('de-DE,de;q=0.9,en-US;q=0.8,en;q=0.7');
    expect(headerOf(navigation, 'sec-ch-ua-platform')).toBe('"Linux"');
    // A second visit within the freshness lifetime is served from the cache.
    expect(cached).toHaveLength(1);
  });

  it('fails for a Windows claim: its workers still say Linux (why allow_cross_os_claims is false)', async () => {
    const windows = (
      await crossOsEngine.generatePersona({
        seed: SEED,
        claimed_os: 'windows',
        languages: LANGUAGES,
        screen: SCREEN,
      })
    ).fingerprint as FingerprintDocument;
    await site.reset();
    const run = () =>
      withContext(
        'windows-claim',
        async (context) => ({
          applied: context.applied,
          selftest: await context.selftest(
            {
              expected: {
                claimed_os: 'windows',
                browser_major: Number(host.chromeVersion.split('.')[0]),
                timezone: 'Europe/Berlin',
                languages: LANGUAGES,
              },
            },
            env,
          ),
        }),
        { engine: crossOsEngine, document: windows },
      );
    const { applied, selftest } = await run();

    expect(applied).toMatchObject({ claimed_os: 'windows', inject: 'window' });
    expect(applied.user_agent).toContain('Windows NT 10.0');
    expect(applied.script_sha256).toMatch(/^[0-9a-f]{64}$/);

    const status = Object.fromEntries(
      selftest.findings.map((finding) => [finding.check, finding.status]),
    );
    // In the window and in every frame the claim holds, headers included:
    // the four values the plan names agree there.
    for (const scope of ['window', 'iframe_same_origin', 'iframe_srcdoc', 'iframe_blank']) {
      expect(status[`os.${scope}`], `os.${scope}`).toBe('pass');
    }
    for (const check of [
      'headers.navigation.user_agent',
      'headers.navigation.platform',
      'headers.fetch.platform',
      'headers.navigation.cache',
      'automation.webdriver',
      'automation.console_getter',
    ]) {
      expect(status[check], check).toBe('pass');
    }
    // A worker contradicts it.
    expect(status['os.worker']).toBe('fail');
    expect(status['os.blob_worker']).toBe('fail');
    expect(status['coherence.platform']).toBe('fail');
    expect(
      selftest.findings.find((finding) => finding.check === 'coherence.platform')?.observed,
    ).toMatch(/window=Win32.*worker=Linux x86_64/);
    expect(selftest.passed).toBe(false);

    // The override keeps Chrome's own header order, and the script is
    // delivered without switching the cache off.
    const navigation = (await site.requests()).find((request) => request.path === '/probe/');
    expect(headerNames(navigation)).toEqual(honestNavigation);
    expect(headerOf(navigation, 'sec-ch-ua-platform')).toBe('"Windows"');
    expect(headerNames(navigation)).not.toContain('Pragma');

    // The same document gives the same script on the next open.
    expect((await run()).applied.script_sha256).toBe(applied.script_sha256);
  });

  it('refuses another operating system while such claims are off', async () => {
    const refused: unknown = await engine
      .generatePersona({ seed: SEED, claimed_os: 'windows', languages: LANGUAGES })
      .catch((error: unknown) => error);
    expect(refused).toBeInstanceOf(EngineError);
    expect((refused as EngineError).code).toBe('cross_os_claim_disabled');

    const windows = (
      await crossOsEngine.generatePersona({
        seed: SEED,
        claimed_os: 'windows',
        languages: LANGUAGES,
        screen: SCREEN,
      })
    ).fingerprint as FingerprintDocument;
    const opened: unknown = await engine
      .openContext(openRequest(windows, 'refused'))
      .catch((error: unknown) => error);
    expect((opened as EngineError).code).toBe('cross_os_claim_disabled');
  });

  it('reports a self-test page it cannot reach as unknown, and fails nothing for it', async () => {
    const result: SelftestResponse = await withContext('selftest-down', (context) =>
      context.selftest(
        {
          pages: [
            { id: 'gone', url: 'https://detector.kage.test/', kind: 'creepjs' },
            {
              id: 'text',
              url: named('/landing'),
              kind: 'generic',
              rules: [{ check: 'landed', pattern: 'Landed', expect: 'match', hard: true }],
            },
            { id: 'notprobe', url: named('/landing'), kind: 'probe' },
          ],
          expected: {
            claimed_os: 'linux',
            browser_major: 155,
            timezone: 'Europe/Berlin',
            languages: LANGUAGES,
          },
          timeout_ms: 3000,
          artifact_prefix: 'selftests/s1',
        },
        env,
      ),
    );
    expect(result.pages.map((page) => [page.id, page.status])).toEqual([
      ['gone', 'unreachable'],
      ['text', 'ok'],
      ['notprobe', 'unparsed'],
    ]);
    expect(result).toMatchObject({ passed: true, complete: false, score: 100 });
    expect(result.findings.find((finding) => finding.page === 'gone')?.observed).toContain(
      'ERR_TUNNEL_CONNECTION_FAILED',
    );
    expect(result.pages[1]?.text?.path).toBe('selftests/s1/text.txt');
  });
});

/** The command line of a persona's browser process, as Chrome took it, without the binary. */
async function browserSwitches(userData: string): Promise<string[]> {
  for (const pid of (await readdir('/proc')).filter((entry) => /^\d+$/.test(entry))) {
    let line: string;
    try {
      line = await readFile(`/proc/${pid}/cmdline`, 'utf8');
    } catch {
      continue;
    }
    // The browser process was started by exec: its arguments are separated
    // by NULs (the processes its zygote forks rewrite theirs with spaces).
    const [binary, ...rest] = line.split('\0').filter((argument) => argument !== '');
    if (
      binary === host.chromePath &&
      rest.includes(`--user-data-dir=${userData}`) &&
      !rest.some((argument) => argument.startsWith('--type='))
    ) {
      return rest;
    }
  }
  throw new Error(`no browser process for ${userData}`);
}

describe('how Chrome is started (ADR 0014)', () => {
  it("gets the switches Kage names and the driver's connection, and nothing else", async () => {
    await withContext('switches', async (context) => {
      await go(context, named('/landing'));
      const userData = profilePaths(path.join(root, 'personas', 'personas', 'switches')).userData;
      const switches = await browserSwitches(userData);
      const plan = launchPlan(linux, openRequest(linux, 'switches'), {
        cpus: host.cpus,
        gl: 'llvmpipe',
      });
      // A driver bump that adds or respells a default of its own fails here:
      // every switch Chrome got is one Kage names or the driver's connection.
      expect(new Set(switches)).toEqual(
        new Set([
          ...CHROME_SWITCHES,
          ...plan.args,
          `--proxy-server=${proxy.server}`,
          `--proxy-bypass-list=${LOOPBACK_THROUGH_PROXY}`,
          `--user-data-dir=${userData}`,
          '--remote-debugging-pipe',
          'about:blank',
        ]),
      );
      // One list of features each way: Chrome takes the last of a switch given twice.
      expect(switches.filter((argument) => argument.startsWith('--disable-features='))).toEqual([
        '--disable-features=MediaRouter,DialMediaRouteProvider,OptimizationHints',
      ]);
      expect(switches.filter((argument) => argument.startsWith('--enable-features='))).toEqual([
        '--enable-features=CDPScreenshotNewSurface',
      ]);
    });
  });

  it('asks a public host over https first, and comes back to http by itself when it has none', async () => {
    const before = proxy.log.length;
    const result = await withContext('upgrade', (context) =>
      go(context, 'http://upgrade.example.com/landing'),
    );
    const asked = proxy.log.slice(before).filter((entry) => entry.target.startsWith('upgrade.'));
    // As a Chrome nobody configured does: the tunnel to 443 first, refused
    // here, then the page over http, with no interstitial in between.
    expect(asked[0]).toMatchObject({ kind: 'connect', target: 'upgrade.example.com:443' });
    expect(asked.some((entry) => entry.kind === 'connect' && !entry.relayed)).toBe(true);
    const page = asked.findIndex((entry) => entry.kind === 'http' && entry.path === '/landing');
    expect(page).toBeGreaterThan(0);
    expect(asked[page]).toMatchObject({ relayed: true });
    expect(result).toMatchObject({
      url: 'http://upgrade.example.com/landing',
      final_url: 'http://upgrade.example.com/landing',
      status: 200,
      title: 'Landing',
    });
    // Chrome's own way back from the refused https is the one hop of the
    // chain: an internal 307 from the https address, which no server sent.
    expect(result.redirects).toEqual([
      {
        url: 'https://upgrade.example.com/landing',
        status: 307,
        location: 'http://upgrade.example.com/landing',
      },
    ]);
    // The names of the test world are never upgraded: Chrome leaves
    // non-unique names alone.
    expect(proxy.log.some((entry) => entry.target === 'fixture.kage.test:443')).toBe(false);
  });

  it('shows the page no bar over it, says nobody drives it, and has no AI models', async () => {
    await site.reset();
    const result = await withContext('launch', (context) =>
      go(context, secure('/launch.html'), {
        wait: { until: 'load', selector: 'body[data-done]' },
        capture: { ...NO_CAPTURE, dom: true },
      }),
    );
    const report = (await site.reports()).find((entry) => entry['page'] === 'launch');
    expect(
      report,
      `the page reported nothing: ${result.error?.message ?? 'no error'}; ${result.dom_html?.slice(0, 400) ?? ''}`,
    ).toMatchObject({ webdriver: false, secure: true });
    // The tab strip and the toolbar, 87 px at a scale of 1 (measured); the
    // bar Chrome shows for an unsupported switch adds 56 without --test-type.
    expect(report?.['chrome']).toBeLessThan(100);
    // A known tell kept on purpose (ADR 0014): no model is ever downloaded
    // through the persona's proxy, so the APIs say they have none.
    expect(report?.['ai']).toEqual({
      LanguageDetector: 'unavailable',
      Summarizer: 'unavailable',
      LanguageModel: 'unavailable',
    });
  });
});

describe('the browser stays inside its limits', () => {
  it('is the Chrome the image is built with', async () => {
    // One pin for the image, for CI and for a developer's machine: the two
    // ARG lines of the Dockerfile, which scripts/setup-chrome.sh reads too.
    const dockerfile = await readFile(
      new URL('../../../../deploy/docker/sidecar.Dockerfile', import.meta.url),
      'utf8',
    );
    const pinned = /^ARG CHROME_VERSION=(\S+)$/m.exec(dockerfile)?.[1];
    expect(pinned, 'ARG CHROME_VERSION in the Dockerfile').toBeDefined();
    expect(host.chromeVersion).toBe(pinned);
  });

  it("runs Chrome with its sandbox, on its own display, without the sidecar's environment", async () => {
    await withContext('process', async (context) => {
      await go(context, named('/landing'));
      const userData = profilePaths(path.join(root, 'personas', 'personas', 'process')).userData;
      const processes = execFileSync('ps', ['-eo', 'pid=,args='], { encoding: 'utf8' })
        .split('\n')
        .filter((line) => line.includes('/chrome') && line.includes(userData));
      const browser = processes.find((line) => !line.includes('--type='));
      expect(browser, 'the browser process').toBeDefined();
      const args = browser ?? '';
      for (const forbidden of [
        '--no-sandbox',
        '--headless',
        '--user-agent',
        '--enable-automation',
        '--disable-web-security',
        '--remote-debugging-port',
      ]) {
        expect(args).not.toContain(forbidden);
      }
      expect(args).toContain('--proxy-bypass-list=<-loopback>');
      expect(args).toContain('--remote-debugging-pipe');
      // A renderer exists and is sandboxed: its own PID namespace and a seccomp filter.
      const renderer = processes.find((line) => line.includes('--type=renderer'));
      const pid = /^\s*(\d+)/.exec(renderer ?? '')?.[1];
      const status = await readFile(`/proc/${pid ?? '0'}/status`, 'utf8');
      expect(/^Seccomp:\s*2$/m.test(status)).toBe(true);
      expect(
        (/^NSpid:\s*(.+)$/m.exec(status)?.[1] ?? '').trim().split(/\s+/).length,
      ).toBeGreaterThan(1);
      // The browser's environment is the one the engine built: its own
      // display and time zone, and nothing of the sidecar's.
      const browserPid = /^\s*(\d+)/.exec(browser ?? '')?.[1];
      const environment = (await readFile(`/proc/${browserPid ?? '0'}/environ`, 'utf8')).split(
        '\0',
      );
      expect(environment).toContain('TZ=Europe/Berlin');
      expect(environment.some((entry) => /^DISPLAY=:1\d\d$/.test(entry))).toBe(true);
      expect(
        environment.some((entry) => entry.startsWith('SIDECAR_') || entry.startsWith('KAGE__')),
      ).toBe(false);
    });
  });

  it('never sends WebRTC traffic around the proxy', async () => {
    const lan = Object.values(networkInterfaces())
      .flat()
      .find((address) => address?.family === 'IPv4' && !address.internal)?.address;
    expect(lan, 'a non-loopback address to listen on').toBeDefined();
    const packets: number[] = [];
    const listener = dgram.createSocket('udp4');
    listener.on('message', (message) => packets.push(message.length));
    await new Promise<void>((resolve) => {
      listener.bind(0, lan, resolve);
    });
    try {
      await site.reset();
      await withContext('webrtc', (context) =>
        go(context, named(`/webrtc.html?stun=${lan ?? ''}:${listener.address().port}`), {
          wait: { until: 'load', selector: 'body[data-done]' },
        }),
      );
      const report = (await site.reports()).find((entry) => entry['page'] === 'webrtc');
      // No STUN packet left the browser, so no candidate tells a page the host's address.
      expect(packets).toEqual([]);
      expect(report).toEqual({ page: 'webrtc', candidates: [] });
    } finally {
      listener.close();
    }
  });

  it('waits for a challenge to resolve by itself and reports one that does not', async () => {
    const passed = await withContext('challenge', (context) =>
      go(context, named('/challenge'), {
        capture: { ...NO_CAPTURE, dom: true },
        challenge_wait_ms: 10_000,
      }),
    );
    expect(passed.error).toBeNull();
    expect(passed).toMatchObject({
      status: 200,
      title: 'Behind the challenge',
      challenge: {
        interstitial: true,
        marker: 'cloudflare:just-a-moment',
        resolved: true,
        vendors: ['cloudflare'],
      },
    });
    expect(passed.challenge.waited_ms).toBeGreaterThan(0);
    expect(passed.dom_html).toContain('Content behind the challenge');

    const stuck = await withContext('challenge-stuck', (context) =>
      go(context, named('/challenge/stuck'), { challenge_wait_ms: 1000 }),
    );
    expect(stuck).toMatchObject({
      status: 403,
      title: 'Just a moment...',
      challenge: { interstitial: true, resolved: false, waited_ms: 1000 },
    });
  });

  it('reports failures as what they are and keeps what it has', async () => {
    await withContext('failures', async (context) => {
      const slow = await go(context, named('/slow?ms=4000'), { timeout_ms: 1000 });
      expect(slow.error).toMatchObject({ code: 'timeout' });
      expect(slow).toMatchObject({
        status: null,
        final_url: named('/slow?ms=4000'),
        dom_html: null,
      });

      const late = await go(context, named('/late'), {
        wait: { until: 'load', selector: '#late' },
        capture: { ...NO_CAPTURE, dom: true },
      });
      expect(late.error).toBeNull();
      expect(late.dom_html).toContain('<p id="late">here</p>');

      const never = await go(context, named('/landing'), {
        wait: { until: 'load', selector: '#never' },
        timeout_ms: 1500,
        capture: { ...NO_CAPTURE, dom: true },
      });
      // The page itself arrived: its capture is still returned with the error.
      expect(never.error).toMatchObject({ code: 'timeout' });
      expect(never).toMatchObject({ status: 200, title: 'Landing' });
      expect(never.dom_html).toContain('Landed');

      const tunnel = await go(context, 'https://tls.kage.test/');
      expect(tunnel.error).toMatchObject({
        code: 'proxy_failed',
        net_error: 'ERR_TUNNEL_CONNECTION_FAILED',
      });

      const download = await go(context, named('/download'));
      expect(download.error).toMatchObject({ code: 'aborted' });

      const missing = await go(context, named('/status/404'), {
        capture: { ...NO_CAPTURE, raw: true },
      });
      expect(missing).toMatchObject({ status: 404, error: null });

      const big = await go(context, named('/big.html'), {
        capture: { ...NO_CAPTURE, dom: true, raw: true },
        max_body_bytes: 1001,
      });
      expect(big).toMatchObject({
        dom_truncated: true,
        raw_body: { truncated: true, encoding: 'utf8' },
      });
      expect(Buffer.byteLength(big.dom_html ?? '')).toBeLessThanOrEqual(1001);
      expect(Buffer.byteLength(big.raw_body?.data ?? '')).toBeLessThanOrEqual(1001);
      expect(big.raw_body?.bytes).toBeGreaterThan(80_000);

      const capped = await go(context, named('/'), {
        wait: { until: 'network_idle' },
        capture: { ...NO_CAPTURE, xhr: { content_types: ['*/*'], max_bytes: 3, max_responses: 2 } },
      });
      expect(capped.xhr).toHaveLength(2);
      expect(capped.xhr_dropped).toBe(2);
      for (const entry of capped.xhr) {
        expect(
          Buffer.from(entry.body?.data ?? '', entry.body?.encoding === 'base64' ? 'base64' : 'utf8')
            .byteLength,
        ).toBeLessThanOrEqual(3);
      }
    });
  });

  it('says so when the proxy refuses the persona', async () => {
    const result = await withContext(
      'bad-proxy',
      (context) => go(context, named('/landing'), { timeout_ms: 8000 }),
      {
        change: { proxy: { server: proxy.server, username: proxy.username, password: 'wrong' } },
      },
    );
    expect(result.status).not.toBe(200);
    expect(result.dom_html).toBeNull();
    const dead = await withContext(
      'dead-proxy',
      (context) => go(context, named('/landing'), { timeout_ms: 8000 }),
      {
        change: { proxy: { server: 'http://127.0.0.1:9', username: 'u', password: 'p' } },
      },
    );
    expect(dead.error).toMatchObject({
      code: 'proxy_failed',
      net_error: 'ERR_PROXY_CONNECTION_FAILED',
    });
  });

  it("refuses a fingerprint that is not this engine's, another browser major, and another screen", async () => {
    const attempts: [string, OpenContextRequest, string][] = [
      ['not a document', openRequest({ ...linux, schema: 2 } as never, 'x'), 'invalid_request'],
      ['a firefox document', openRequest({ ...linux, engine: 'firefox' }, 'x'), 'invalid_request'],
      [
        'another major',
        openRequest({ ...linux, browser_version: '154.0.8037.97' }, 'x'),
        'browser_major_mismatch',
      ],
      [
        'another screen',
        openRequest(linux, 'x', { screen: { ...linux.native.screen, width: 1366 } }),
        'invalid_request',
      ],
    ];
    for (const [label, request, code] of attempts) {
      const error: unknown = await engine.openContext(request).catch((thrown: unknown) => thrown);
      expect(error, label).toBeInstanceOf(EngineError);
      expect((error as EngineError).code, label).toBe(code);
    }
    expect(engine.status()).toEqual({ browserVersion: host.chromeVersion });
    expect(engine.name).toBe('chromium');
  });

  it('moves a document of an older Chrome to this one, and none of another engine', async () => {
    const major = Number(host.chromeVersion.split('.')[0]);
    const older = {
      ...linux,
      browser_version: `${String(major - 1)}.0.8000.1`,
      user_agent: chromeUserAgentOf(linux.user_agent, major, major - 1),
    };
    const moved = await engine.upgradePersona({ fingerprint: older });
    expect(moved.upgraded).toBe(true);
    // A persona that moves is one made new on this Chrome.
    expect(moved.fingerprint).toEqual(linux);
    expect(await engine.upgradePersona({ fingerprint: linux })).toMatchObject({ upgraded: false });
    await expect(
      engine.upgradePersona({ fingerprint: { ...linux, engine: 'firefox' } }),
    ).rejects.toMatchObject({ code: 'invalid_request' });
  });

  it('closes every open context when the engine closes, and closing twice is fine', async () => {
    const local = new ChromiumEngine(
      {
        personasDir: path.join(root, 'personas'),
        allowCrossOsClaims: false,
        gl: 'none',
        selftestPages: [],
        log: silentLog,
      },
      host,
    );
    const context = await local.openContext(openRequest(linux, 'closing'));
    expect(context.browserVersion).toBe(host.chromeVersion);
    await local.close();
    await context.close();
    await local.close();
  });
});
