/**
 * The engine parity suite (spec §21, docs/02-plan.md M5): one set of
 * contract, capture and behavior tests that both engines run, each against
 * its real browser on its own X display, through a local authenticated
 * proxy, against the fixture site. A difference between the engines that is
 * meant to be there is named in {@link ParitySubject}; everything else must
 * come out the same.
 *
 * Nothing here leaves the machine: the test proxy refuses every tunnel.
 */
import dgram from 'node:dgram';
import { mkdtemp, readFile } from 'node:fs/promises';
import { networkInterfaces, tmpdir, userInfo } from 'node:os';
import path from 'node:path';

import type {
  CaptureOptions,
  ClaimedOs,
  EngineName,
  FlowEvent,
  FlowRequest,
  NavigateRequest,
  OpenContextRequest,
  PageResult,
  SideEffect,
} from '@kage/contract';
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';

import { ArtifactWriter } from '../artifacts.js';
import { HUMAN_BOUNDS, motorProfile } from '../behavior/profile.js';
import {
  silentLog,
  type Engine,
  type EngineContext,
  type EngineOptions,
  type OperationEnv,
} from '../engine.js';
import type { FingerprintDocument } from '../fingerprint.js';
import { judgeProbe, parseProbeReport, type ProbeReport } from '../selftest/probe.js';

import { startFixtureSite, type FixtureRequest, type FixtureSite } from './fixture-site.js';
import { inputReportOf, inputStatistics } from './recorder.js';
import { startTestProxy, type TestProxy } from './test-proxy.js';

/** An engine as the parity suite runs it, and what is meant to differ. */
export interface ParitySubject {
  readonly name: EngineName;
  /** The system the engine's personas claim in this suite. */
  readonly claimedOs: ClaimedOs;
  /** Major version of the browser the engine drives. */
  readonly browserMajor: number;
  createEngine(options: EngineOptions): Promise<Engine>;
  /** What a full-page screenshot fires in the page. */
  readonly fullPageSideEffects: readonly SideEffect[];
  /** The media type of a screenshot that was asked for as WebP. */
  readonly webpScreenshot: 'image/webp' | 'image/png';
  /** How the browser names a refused tunnel and a proxy that is not there; `null` when it gives no name. */
  readonly netErrors: { readonly tunnel: string | null; readonly deadProxy: string | null };
}

const SEEDS = {
  first: '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac',
  other: '0b6d1f4e8a2c5397b1d3f5a7c9e0b2d4f6a8c0e2b4d6f8a1c3e5a7c9e1b3d5f7',
};
const LANGUAGES = ['de-DE', 'de', 'en-US', 'en'];
const SCREEN = { min_width: 1600, max_width: 1920, min_height: 900, max_height: 1080 };
const NO_CAPTURE: CaptureOptions = {
  dom: false,
  raw: false,
  xhr: null,
  console: false,
  screenshot: null,
  pdf: null,
};
const DOM: CaptureOptions = { ...NO_CAPTURE, dom: true };

const headerOf = (request: FixtureRequest | undefined, name: string) =>
  request?.headers.find(([key]) => key.toLowerCase() === name)?.[1] ?? null;

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** Defines the parity suite for one engine. Call it from that engine's test file. */
export function defineParitySuite(subject: ParitySubject): void {
  vi.setConfig({ testTimeout: 240_000, hookTimeout: 240_000 });

  let site: FixtureSite;
  let proxy: TestProxy;
  let root: string;
  let engine: Engine;
  let env: OperationEnv;
  let persona: FingerprintDocument;
  let otherPersona: FingerprintDocument;

  /** A name only the test proxy can resolve: plain http, not a secure context. */
  const named = (pathname: string) => `http://fixture.kage.test${pathname}`;
  /** A loopback name: a secure context, and still through the proxy. */
  const secure = (pathname: string) => `http://fixture.localhost:${site.port}${pathname}`;

  const generate = async (seed: string) =>
    (
      await engine.generatePersona({
        seed,
        claimed_os: subject.claimedOs,
        languages: LANGUAGES,
        screen: SCREEN,
      })
    ).fingerprint as FingerprintDocument;

  const openRequest = (
    document: FingerprintDocument,
    profile: string,
    change: Partial<OpenContextRequest> = {},
  ): OpenContextRequest => ({
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
  });

  const go = (
    context: EngineContext,
    url: string,
    change: Partial<NavigateRequest> = {},
  ): Promise<PageResult> =>
    context.navigate({ url, wait: { until: 'load' }, capture: NO_CAPTURE, ...change }, env);

  async function withContext<T>(
    profile: string,
    run: (context: EngineContext) => Promise<T>,
    options: { document?: FingerprintDocument; change?: Partial<OpenContextRequest> } = {},
  ): Promise<T> {
    const context = await engine.openContext(
      openRequest(options.document ?? persona, profile, options.change),
    );
    try {
      return await run(context);
    } finally {
      await context.close();
    }
  }

  const lastRequest = async (pathname: string) =>
    (await site.requests()).filter((request) => request.path === pathname).at(-1);

  const probe = async (context: EngineContext): Promise<ProbeReport> => {
    const result = await go(context, secure('/probe/'), {
      wait: { until: 'load', selector: '#kage-probe-report[data-done]' },
      capture: DOM,
    });
    const json =
      /<script type="application\/json" id="kage-probe-report"[^>]*>([\s\S]*?)<\/script>/.exec(
        result.dom_html ?? '',
      )?.[1];
    const report = parseProbeReport(
      (json ?? '').replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&'),
    );
    if (report === null) {
      throw new Error(`the probe published no report: ${result.error?.message ?? 'no error'}`);
    }
    return report;
  };

  const flow = async (context: EngineContext, request: Omit<FlowRequest, 'capture'>) => {
    const events: FlowEvent[] = [];
    await context.flow({ capture: NO_CAPTURE, ...request }, env, (event) => events.push(event));
    const steps = events.filter((event) => event.type === 'step');
    const final = events.find((event) => event.type === 'final');
    if (final === undefined) {
      throw new Error('the flow sent no final event');
    }
    return { events, steps, final };
  };

  beforeAll(async () => {
    if (userInfo().uid === 0) {
      throw new Error(
        'these tests start browsers with their sandbox, which does not run as root: run them as another user',
      );
    }
    site = await startFixtureSite();
    proxy = await startTestProxy({ httpPort: site.port });
    root = await mkdtemp(path.join(tmpdir(), `kage-parity-${subject.name}-`));
    engine = await subject.createEngine({
      personasDir: path.join(root, 'personas'),
      allowCrossOsClaims: false,
      gl: 'llvmpipe',
      selftestPages: [{ id: 'probe', url: secure('/probe/'), kind: 'probe' }],
      log: silentLog,
    });
    env = {
      artifacts: new ArtifactWriter(path.join(root, 'artifacts')),
      defaultArtifactPrefix: 'sidecar/test',
    };
    persona = await generate(SEEDS.first);
    otherPersona = await generate(SEEDS.other);
  });

  afterAll(async () => {
    await engine.close();
    await proxy.stop();
    await site.stop();
  });

  describe(`engine parity: ${subject.name}`, () => {
    it('generates the same fingerprint for the same seed, and another for another', async () => {
      expect(await generate(SEEDS.first)).toEqual(persona);
      expect(persona).toMatchObject({
        engine: subject.name,
        claimed_os: subject.claimedOs,
        seed: SEEDS.first,
        inject: 'none',
      });
      expect(otherPersona.seed).toBe(SEEDS.other);
      expect(JSON.stringify(otherPersona.generated)).not.toBe(JSON.stringify(persona.generated));
      expect(engine.status().browserVersion).toBe(persona.browser_version);
    });

    it('captures a page through the authenticated proxy: document, XHR, screenshot, PDF', async () => {
      await site.reset();
      const before = proxy.log.length;
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
        side_effects: ['beforeprint', 'afterprint'],
        behavior: null,
        challenge: { interstitial: false, vendors: [], script_hosts: ['fixture.kage.test'] },
      });
      expect(result.dom_html).toContain('<li>gamma</li>');
      expect(result.raw_body).toMatchObject({ truncated: false, encoding: 'utf8' });
      expect(result.raw_body?.data).toContain('<script src="/app.js"></script>');
      expect(result.raw_body?.data).not.toContain('<li>gamma</li>');
      expect(
        result.headers.some(
          (header) => header.name === 'set-cookie' && header.value.startsWith('visits=1'),
        ),
      ).toBe(true);
      // Both ways a script asks, filtered by media type.
      expect(
        result.xhr.map((entry) => [entry.kind, new URL(entry.url).pathname, entry.status]).sort(),
      ).toEqual([
        ['fetch', '/api/items', 200],
        ['xhr', '/api/time', 200],
      ]);
      expect(
        JSON.parse(result.xhr.find((entry) => entry.kind === 'fetch')?.body?.data ?? '{}'),
      ).toEqual({ items: [{ name: 'alpha' }, { name: 'beta' }, { name: 'gamma' }] });
      // The files are where the result says, with the size and the hash it says.
      expect(result.screenshot?.content_type).toBe(subject.webpScreenshot);
      expect(result.screenshot?.path).toMatch(/^requests\/r1\/[a-z0-9]+-screenshot\.(webp|png)$/);
      for (const artifact of [result.screenshot, result.pdf]) {
        const file = await readFile(path.join(root, 'artifacts', artifact?.path ?? ''));
        expect(file.byteLength).toBe(artifact?.bytes);
        expect(file.byteLength).toBeGreaterThan(500);
      }
      const pdf = await readFile(path.join(root, 'artifacts', result.pdf?.path ?? ''));
      expect(pdf.subarray(0, 5).toString('latin1')).toBe('%PDF-');
      // Everything went through the proxy, with the persona's credentials.
      const asked = proxy.log.slice(before).filter((entry) => entry.target.startsWith('fixture.'));
      const relayed = asked.filter((entry) => entry.relayed);
      expect(relayed.length).toBeGreaterThanOrEqual(5);
      expect(relayed.every((entry) => entry.kind === 'http' && entry.authorized)).toBe(true);
    });

    it('follows redirects, reports every hop, and sends loopback through the proxy too', async () => {
      await withContext('redirects', async (context) => {
        const result = await go(context, named('/redirect/a'), { capture: DOM });
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
        // A typed navigation: no referer.
        expect(headerOf(await lastRequest('/landing?from=redirect'), 'referer')).toBeNull();

        const before = proxy.log.length;
        const loopback = await go(context, `http://127.0.0.1:${site.port}/landing`);
        expect(loopback.status).toBe(200);
        expect(
          proxy.log.slice(before).filter((entry) => entry.target === `127.0.0.1:${site.port}`)
            .length,
        ).toBeGreaterThanOrEqual(1);
      });
    });

    it('leaves in the page only what a capture is known to fire', async () => {
      // What one load of the events page noticed while it was the page.
      const events = async (run: string) =>
        (await site.reports())
          .filter((report) => report['page'] === 'events' && report['run'] === run)
          .map((report) => String(report['what']))
          .filter((event) => !['pagehide', 'visibilitychange', 'blur', 'focus'].includes(event));
      const wait = { until: 'load' as const, selector: 'body[data-ready]', extra_ms: 300 };

      await withContext('events', async (context) => {
        await site.reset();
        const plain = await go(context, named('/events.html?plain'), {
          wait,
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
        await sleep(300);
        // The page saw itself load and nothing else: no resize, no DOM change.
        expect(await events('plain')).toEqual(['ready']);

        const full = await go(context, named('/events.html?full'), {
          wait,
          capture: { ...NO_CAPTURE, screenshot: { full_page: true, format: 'jpeg' } },
        });
        expect(full.side_effects).toEqual(subject.fullPageSideEffects);
        expect(full.screenshot?.path).toMatch(/-screenshot\.jpg$/);
        await sleep(300);
        expect(new Set((await events('full')).slice(1))).toEqual(
          new Set(subject.fullPageSideEffects),
        );

        const printed = await go(context, named('/events.html?printed'), {
          wait,
          capture: { ...NO_CAPTURE, pdf: {} },
        });
        expect(printed.side_effects).toEqual(['beforeprint', 'afterprint']);
        await sleep(300);
        expect(await events('printed')).toEqual(['ready', 'beforeprint', 'afterprint']);
        expect(await events('plain')).toEqual(['ready']);
      });
    });

    it('keeps cookies across two opens of one persona and not across personas', async () => {
      const visits = (result: PageResult) =>
        /visits=(\d+)/.exec(
          result.headers.find((header) => header.name === 'set-cookie')?.value ?? '',
        )?.[1];
      expect(visits(await withContext('returning', (context) => go(context, named('/'))))).toBe(
        '1',
      );
      expect(visits(await withContext('returning', (context) => go(context, named('/'))))).toBe(
        '2',
      );
      expect(visits(await withContext('stranger', (context) => go(context, named('/'))))).toBe('1');
    });

    it('shows the same browser on every open of one fingerprint', async () => {
      const first = await withContext('same-1', probe);
      const second = await withContext('same-2', probe);
      expect(second.scopes).toEqual(first.scopes);
      expect(second.environment).toEqual(first.environment);
      // Another persona is another browser.
      const other = await withContext('same-3', probe, {
        document: otherPersona,
        change: { persona_id: '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8f' },
      });
      expect(other.scopes.window.user_agent).toBe(first.scopes.window.user_agent);
      expect(JSON.stringify(other.scopes.window)).not.toBe(JSON.stringify(first.scopes.window));
    });

    it('passes the coherence probe in the window, every frame and both workers', async () => {
      await site.reset();
      const report = await withContext('coherence', probe);
      const findings = judgeProbe(
        report,
        {
          claimed_os: subject.claimedOs,
          browser_major: subject.browserMajor,
          timezone: 'Europe/Berlin',
          languages: LANGUAGES,
        },
        'probe',
        10,
      );
      expect(findings.filter((finding) => finding.status === 'fail')).toEqual([]);
      // Every scope reported: none was skipped as a warning.
      expect(findings.filter((finding) => finding.check.startsWith('scope.'))).toEqual([]);
      for (const check of ['coherence.fonts', 'coherence.webgl', 'fonts.foreign', 'webgl.os']) {
        expect(findings.find((finding) => finding.check === check)?.status, check).toBe('pass');
      }
      expect(report.automation).toMatchObject({
        webdriver: false,
        console_stack_reads: 0,
        getters_native: true,
        illegal_invocation_throws: true,
        navigator_own_properties: [],
      });
      // Workers measured fonts and read WebGL themselves.
      for (const scope of [report.scopes.worker, report.scopes.blob_worker]) {
        expect(scope?.fonts?.length).toBeGreaterThan(0);
        expect(scope?.webgl?.renderer).toBe(report.scopes.window.webgl?.renderer);
      }
      // A plain navigation carries neither of the two headers a cache that
      // was switched off sends.
      const navigation = (report.requests.navigation ?? []).map(([name]) => name.toLowerCase());
      expect(navigation).not.toContain('pragma');
      expect(navigation).not.toContain('cache-control');
    });

    it('exports and imports cookies as rows of http_cookies, and the storage of the page', async () => {
      const exported = await withContext('state-out', async (context) => {
        await go(context, named('/set-cookies'));
        await go(context, named('/storage.html'), {
          wait: { until: 'load', selector: 'body[data-ready]' },
        });
        return context.exportState({ origin: named(''), include_storage: true });
      });
      const byName = new Map(exported.cookies.map((cookie) => [cookie.name, cookie]));
      expect(byName.get('session_only')).toMatchObject({
        domain: 'fixture.kage.test',
        host_only: true,
        path: '/',
        expires_at: null,
      });
      expect(byName.get('persistent')).toMatchObject({ http_only: true, same_site: 'strict' });
      expect(byName.get('persistent')?.expires_at).not.toBeNull();
      expect(byName.get('scoped')).toMatchObject({ path: '/account' });
      expect(exported.local_storage).toEqual(
        expect.arrayContaining([
          { name: 'seen', value: '1' },
          { name: 'theme', value: 'dark' },
        ]),
      );
      expect(exported.session_storage).toEqual([{ name: 'step', value: '2' }]);

      await withContext('state-in', async (context) => {
        const imported = await context.importState({
          origin: named(''),
          cookies: [
            ...exported.cookies,
            { ...(exported.cookies[0] ?? never()), domain: 'elsewhere.test', name: 'foreign' },
          ],
        });
        expect(imported.imported).toBe(exported.cookies.length);
        expect(imported.rejected).toMatchObject([{ name: 'foreign', domain: 'elsewhere.test' }]);
        const echoed = await go(context, named('/cookies'), {
          capture: { ...NO_CAPTURE, raw: true },
        });
        const sent = (JSON.parse(echoed.raw_body?.data ?? '{}') as { cookie: string }).cookie;
        expect(sent).toContain('session_only=s1');
        expect(sent).toContain('persistent=p1');
        // The path-scoped cookie is not sent to another path.
        expect(sent).not.toContain('scoped=');
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
        const report = (await site.reports()).find((entry) => entry['page'] === 'webrtc') as
          { candidates: string[] } | undefined;
        // No packet left the browser for the STUN server, and no candidate
        // came from asking one.
        expect(packets).toEqual([]);
        expect(report?.candidates.filter((kind) => kind !== 'host')).toEqual([]);
      } finally {
        listener.close();
      }
    });

    it('reports failures as what they are and keeps what it has', async () => {
      await withContext('failures', async (context) => {
        const slow = await go(context, named('/slow?ms=4000'), { timeout_ms: 1000 });
        expect(slow.error).toMatchObject({ code: 'timeout' });
        expect(slow).toMatchObject({ status: null, dom_html: null });

        const late = await go(context, named('/late'), {
          wait: { until: 'load', selector: '#late' },
          capture: DOM,
        });
        expect(late.error).toBeNull();
        expect(late.dom_html).toContain('<p id="late">here</p>');

        const never = await go(context, named('/landing'), {
          wait: { until: 'load', selector: '#never' },
          timeout_ms: 1500,
          capture: DOM,
        });
        // The page itself arrived: its capture is still returned with the error.
        expect(never.error).toMatchObject({ code: 'timeout' });
        expect(never).toMatchObject({ status: 200, title: 'Landing' });

        const tunnel = await go(context, 'https://tls.kage.test/');
        expect(tunnel.error).toMatchObject({
          code: 'proxy_failed',
          net_error: subject.netErrors.tunnel,
        });
        expect(tunnel.status).toBeNull();

        const download = await go(context, named('/download'));
        expect(download.error).toMatchObject({ code: 'aborted' });

        const missing = await go(context, named('/status/404'));
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

        const capped = await go(context, named('/'), {
          wait: { until: 'network_idle' },
          capture: {
            ...NO_CAPTURE,
            xhr: { content_types: ['*/*'], max_bytes: 3, max_responses: 2 },
          },
        });
        expect(capped.xhr).toHaveLength(2);
        expect(capped.xhr_dropped).toBe(2);

        const challenge = await go(context, named('/challenge'), {
          capture: DOM,
          challenge_wait_ms: 6000,
        });
        expect(challenge.challenge).toMatchObject({ interstitial: true, resolved: true });
        expect(challenge.dom_html).toContain('Content behind the challenge');
      });
    });

    it('says so when the proxy refuses the persona, and when it is not there', async () => {
      const refused = await withContext(
        'bad-proxy',
        (context) => go(context, named('/landing'), { timeout_ms: 8000, capture: DOM }),
        {
          change: { proxy: { server: proxy.server, username: proxy.username, password: 'wrong' } },
        },
      );
      expect(refused.error).toMatchObject({ code: 'proxy_failed' });
      expect(refused.dom_html ?? '').not.toContain('Landed');
      const dead = await withContext(
        'dead-proxy',
        (context) => go(context, named('/landing'), { timeout_ms: 8000 }),
        { change: { proxy: { server: 'http://127.0.0.1:9', username: 'u', password: 'p' } } },
      );
      expect(dead.error).toMatchObject({
        code: 'proxy_failed',
        net_error: subject.netErrors.deadProxy,
      });
    });

    it('arrives through a link and through a search result, with the referer of the click', async () => {
      await withContext('entry', async (context) => {
        await site.reset();
        const linked = await go(context, secure('/listing?page=2'), {
          capture: DOM,
          behavior: { seed: SEEDS.other, entry: { strategy: 'link', from_url: secure('/') } },
        });
        expect(linked.error).toBeNull();
        expect(linked).toMatchObject({ status: 200, final_url: secure('/listing?page=2') });
        expect(linked.behavior).toMatchObject({
          seed: SEEDS.other,
          profile_id: motorProfile(persona.seed).id,
          clicks: 1,
          entry: {
            strategy: 'link',
            outcome: 'arrived',
            from_url: secure('/'),
            referer: secure('/'),
          },
        });
        expect(linked.dom_html).toContain(`<p id="referer">${secure('/')}</p>`);
        const arrived = await lastRequest('/listing?page=2');
        expect(headerOf(arrived, 'referer')).toBe(secure('/'));
        expect(headerOf(arrived, 'sec-fetch-site')).toBe('same-origin');
        expect(headerOf(arrived, 'sec-fetch-user')).toBe('?1');
        expect(headerOf(arrived, 'sec-fetch-mode')).toBe('navigate');

        // A results page on another site; the result that leads to the target's host is clicked.
        const results = named(
          `/search?q=fixture&r=${encodeURIComponent('http://elsewhere.kage.test/landing')}&r=${encodeURIComponent(secure('/landing'))}`,
        );
        const searched = await go(context, secure('/landing?from=search'), {
          capture: DOM,
          behavior: { entry: { strategy: 'search_engine', from_url: results } },
        });
        expect(searched.behavior?.entry).toMatchObject({
          strategy: 'search_engine',
          outcome: 'arrived',
          from_url: results,
        });
        expect(searched).toMatchObject({ status: 200, title: 'Landing' });
        const viaSearch = await lastRequest('/landing');
        // Across sites the browser sends the origin only, and says so.
        expect(headerOf(viaSearch, 'referer')).toBe(named('/'));
        expect(headerOf(viaSearch, 'sec-fetch-site')).toBe('cross-site');

        const missing = await go(context, secure('/status/404'), {
          capture: DOM,
          behavior: { entry: { strategy: 'link', from_url: secure('/') } },
        });
        expect(missing.error).toMatchObject({ code: 'aborted' });
        expect(missing.behavior?.entry).toMatchObject({ outcome: 'link_not_found', referer: null });
        expect(missing).toMatchObject({ status: null, dom_html: null });

        const fallback = await go(context, secure('/landing?fallback'), {
          behavior: {
            entry: {
              strategy: 'search_engine',
              from_url: named('/search?q=nothing'),
              fallback: 'none',
            },
          },
        });
        expect(fallback.behavior?.entry).toMatchObject({ outcome: 'fell_back' });
        expect(fallback).toMatchObject({ status: 200, error: null });
        expect(headerOf(await lastRequest('/landing?fallback'), 'referer')).toBeNull();

        const blocked = await go(context, secure('/landing?blocked'), {
          behavior: { entry: { strategy: 'search_engine', from_url: named('/challenge/stuck') } },
        });
        expect(blocked.behavior?.entry).toMatchObject({ outcome: 'blocked' });
        expect(blocked.error).toMatchObject({ code: 'aborted' });

        // `none` and `direct` are one thing on the wire: a typed navigation.
        const direct = await go(context, secure('/landing?direct'), {
          behavior: { entry: { strategy: 'direct' } },
        });
        expect(direct.behavior?.entry).toMatchObject({ strategy: 'direct', outcome: 'arrived' });
        expect(headerOf(await lastRequest('/landing?direct'), 'referer')).toBeNull();
        expect(headerOf(await lastRequest('/landing?direct'), 'sec-fetch-site')).toBe('none');
      });
    });

    it('scrolls, dwells and wanders like a person when a navigation asks for it', async () => {
      const result = await withContext('behavior', async (context) => {
        const page = await go(context, named('/input/'), {
          wait: { until: 'load', selector: 'body[data-ready]' },
          dwell_ms: 5200,
          timeout_ms: 120_000,
          behavior: { seed: SEEDS.first, scroll_depth: 0.35, wander: true },
        });
        // The recorder publishes every quarter of a second; read it afterwards.
        const read = await flow(context, {
          actions: [
            { action: 'wait_for', ms: 400 },
            { action: 'extract', capture: DOM },
          ],
        });
        return { page, report: inputReportOf(read.steps[1]?.capture?.dom_html ?? null) };
      });
      const { behavior } = result.page;
      expect(behavior).toMatchObject({ seed: SEEDS.first, entry: null, clicks: 0, keys: 0 });
      expect(behavior?.notches).toBeGreaterThanOrEqual(3);
      expect(behavior?.dwell_ms).toBeGreaterThanOrEqual(5000);
      const measured = inputStatistics(result.report);
      expect(measured.untrusted).toBe(0);
      // A wheel turns by the browser's own notch, and the page glides.
      // Chrome hands a page two notches of one frame as one event of twice
      // the size; the distance per notch is what both browsers agree on.
      const notches = behavior?.notches ?? 0;
      expect(measured.wheel.count).toBeLessThanOrEqual(notches);
      expect(measured.wheel.count).toBeGreaterThanOrEqual(notches / 2);
      expect(measured.wheel.modes).toEqual([0]);
      expect(measured.wheel.total / notches).toBeGreaterThanOrEqual(90);
      expect(measured.wheel.total / notches).toBeLessThanOrEqual(130);
      expect(measured.scrollEvents).toBeGreaterThan(measured.wheel.count);
      const reached = result.report.window.scroll_top;
      const scrollable = result.report.window.scroll_height - result.report.window.inner[1];
      expect(reached / scrollable).toBeGreaterThan(0.3);
      expect(reached / scrollable).toBeLessThan(0.6);
    });

    it('clicks, types and selects through the mouse and keyboard of its display', async () => {
      const text = 'Zürich y@web';
      const run = await withContext('flow-human', (context) =>
        flow(context, {
          behavior: { seed: SEEDS.first, typos: false },
          timeout_ms: 200_000,
          actions: [
            { action: 'goto', url: named('/input/') },
            { action: 'wait_for', selector: 'body[data-ready]' },
            { action: 'hover', selector: '#far' },
            { action: 'click', selector: '#target' },
            { action: 'type', selector: '#field', text },
            { action: 'select', selector: '#choice', value: 'three' },
            { action: 'click', selector: '#bottom' },
            { action: 'wait_for', ms: 400 },
            { action: 'extract', capture: DOM },
          ],
        }),
      );
      expect(run.steps.map((step) => [step.action, step.ok, step.error])).toEqual(
        [
          'goto',
          'wait_for',
          'hover',
          'click',
          'type',
          'select',
          'click',
          'wait_for',
          'extract',
        ].map((action) => [action, true, null]),
      );
      expect(run.final).toMatchObject({ ok: true });
      const report = inputReportOf(run.steps.at(-1)?.capture?.dom_html ?? null);
      const measured = inputStatistics(report);
      // Everything a page can check about where input comes from.
      expect(measured.untrusted).toBe(0);
      expect(measured.pointerTypes).toEqual(['mouse']);
      expect(report.field).toBe(text);
      expect(report.choice).toBe('three');
      const clicked = report.events
        .filter((event) => event.type === 'click')
        .map((event) => event.target);
      expect(clicked).toEqual(expect.arrayContaining(['target', 'field', 'choice', 'bottom']));
      // The keyboard is a German one: Z and Y change places, and capitals
      // and @ need their modifier keys.
      const keys = report.events
        .filter((event) => event.type === 'keydown')
        .map((event) => `${event.key ?? ''}/${event.code ?? ''}`);
      expect(keys).toEqual(
        expect.arrayContaining(['Shift/ShiftLeft', 'Z/KeyY', 'y/KeyZ', '@/KeyQ']),
      );
      expect(keys.indexOf('Shift/ShiftLeft')).toBeLessThan(keys.indexOf('Z/KeyY'));
      expect(keys.some((key) => key.startsWith('AltGraph/'))).toBe(true);
      // Movements are curves at a mouse's pace, never lines at a script's.
      expect(measured.moves.length).toBeGreaterThanOrEqual(3);
      for (const move of measured.moves) {
        expect(move.straightness).toBeGreaterThan(1);
        expect(move.duration).toBeGreaterThan(120);
        expect(move.duration).toBeLessThan(3200);
        expect(move.intervalSpread).toBeGreaterThan(1);
      }
      // Typing and clicking keep a person's times: the spec's 60 to 220 ms
      // between keys (with the slack of a busy machine), a rest before each
      // click, a button that stays down for a moment.
      expect(measured.keyIntervals.length).toBeGreaterThanOrEqual(text.length - 2);
      const [low, high] = HUMAN_BOUNDS.keyIntervalMs;
      for (const interval of measured.keyIntervals) {
        expect(interval).toBeGreaterThan(low - 25);
        expect(interval).toBeLessThan(high + 90);
      }
      for (const hold of measured.buttonHolds) {
        expect(hold).toBeGreaterThan(30);
        expect(hold).toBeLessThan(260);
      }
      for (const hover of measured.hovers) {
        expect(hover).toBeGreaterThan(40);
      }
      expect(run.final.behavior).toMatchObject({
        seed: SEEDS.first,
        profile_id: motorProfile(persona.seed).id,
        typos: 0,
      });
      expect(run.final.behavior?.clicks).toBeGreaterThanOrEqual(4);
    });

    it("gives two personas two hands, each a person's", async () => {
      // The two personas of this suite were picked so that their typing
      // differs more than one person's does from one run to the next.
      const profiles = [motorProfile(persona.seed), motorProfile(otherPersona.seed)];
      expect(profiles[0]?.id).not.toBe(profiles[1]?.id);
      const text = 'the quick brown fox jumps over the lazy dog and runs on';
      const medians: number[] = [];
      for (const [index, document] of [persona, otherPersona].entries()) {
        const run = await withContext(
          `hands-${index}`,
          (context) =>
            flow(context, {
              behavior: { seed: SEEDS.first },
              timeout_ms: 200_000,
              actions: [
                { action: 'goto', url: named('/input/') },
                { action: 'type', selector: '#field', text },
                { action: 'wait_for', ms: 400 },
                { action: 'extract', capture: DOM },
              ],
            }),
          { document, change: { persona_id: `0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d${80 + index}` } },
        );
        const measured = inputStatistics(
          inputReportOf(run.steps.at(-1)?.capture?.dom_html ?? null),
        );
        expect(run.final.behavior?.profile_id).toBe(profiles[index]?.id);
        expect(measured.medianKeyInterval).toBeGreaterThan(HUMAN_BOUNDS.keyIntervalMs[0]);
        expect(measured.medianKeyInterval).toBeLessThan(HUMAN_BOUNDS.keyIntervalMs[1]);
        // What the page measures is what the profile says, within a busy machine's slack.
        expect(
          Math.abs(measured.medianKeyInterval - (profiles[index]?.typing.interval ?? 0)),
        ).toBeLessThan(25);
        medians.push(measured.medianKeyInterval);
      }
      const planned = Math.abs(
        (profiles[0]?.typing.interval ?? 0) - (profiles[1]?.typing.interval ?? 0),
      );
      expect(planned).toBeGreaterThan(30);
      expect(Math.abs((medians[0] ?? 0) - (medians[1] ?? 0))).toBeGreaterThan(planned - 25);
    });

    it('runs the other steps of a flow, by the driver when told, and stops at the first failure', async () => {
      const run = await withContext('flow-rest', (context) =>
        flow(context, {
          timeout_ms: 120_000,
          actions: [
            { action: 'goto', url: named('/') },
            { action: 'wait_for', network_idle: true },
            { action: 'click', selector: '#next' },
            { action: 'wait_for', url: '**/listing?page=2' },
            { action: 'assert', text_contains: 'Listing' },
            { action: 'back' },
            { action: 'wait_for', selector: '#items' },
            { action: 'press', key: 'End' },
            { action: 'scroll', depth: 0.2, human: false },
            { action: 'hover', selector: '#title', human: false },
            { action: 'click', selector: '#next', human: false },
            { action: 'wait_for', selector: '#referer' },
            { action: 'screenshot', options: { full_page: false, format: 'png' } },
            { action: 'pdf', options: {} },
            {
              action: 'extract',
              capture: { ...DOM, xhr: { content_types: ['application/json'], max_bytes: 10_000 } },
            },
            { action: 'assert', selector: '#no-such-element' },
            { action: 'goto', url: named('/landing') },
          ],
        }),
      );
      const failed = run.steps.find((step) => !step.ok);
      expect(run.steps.filter((step) => step.ok).map((step) => step.action)).toEqual([
        'goto',
        'wait_for',
        'click',
        'wait_for',
        'assert',
        'back',
        'wait_for',
        'press',
        'scroll',
        'hover',
        'click',
        'wait_for',
        'screenshot',
        'pdf',
        'extract',
      ]);
      // The step that fails ends the flow: the one after it never runs.
      expect(failed).toMatchObject({
        index: 15,
        action: 'assert',
        error: 'no element matches #no-such-element',
      });
      expect(run.steps).toHaveLength(16);
      expect(run.final).toMatchObject({
        ok: false,
        result: { final_url: named('/listing?page=2') },
      });
      const captured = (action: string) =>
        run.steps.find((step) => step.action === action)?.capture;
      expect(captured('screenshot')?.screenshot?.content_type).toBe('image/png');
      expect(captured('pdf')?.pdf?.content_type).toBe('application/pdf');
      expect(captured('extract')?.dom_html).toContain('<h1>Listing</h1>');
      expect(captured('extract')).toMatchObject({
        status: 200,
        final_url: named('/listing?page=2'),
      });
      // The driver's click is a click too: the link was followed with its referer.
      expect(captured('extract')?.dom_html).toContain(`<p id="referer">${named('/')}</p>`);

      const timed = await withContext('flow-timeout', (context) =>
        flow(context, {
          actions: [
            { action: 'goto', url: named('/landing') },
            { action: 'wait_for', selector: '#never', timeout_ms: 600 },
          ],
        }),
      );
      expect(timed.steps[1]).toMatchObject({
        ok: false,
        error: 'no element matched #never within 600 ms',
      });
      expect(timed.final.ok).toBe(false);
    });

    it('tests itself against its own probe, and fails nothing for a page it cannot reach', async () => {
      const result = await withContext('selftest', (context) =>
        context.selftest(
          {
            expected: {
              claimed_os: subject.claimedOs,
              browser_major: subject.browserMajor,
              timezone: 'Europe/Berlin',
              languages: LANGUAGES,
            },
            pages: [
              { id: 'probe', url: secure('/probe/'), kind: 'probe' },
              { id: 'gone', url: 'https://unreachable.kage.test/', kind: 'generic' },
            ],
            timeout_ms: 20_000,
          },
          env,
        ),
      );
      expect(result.findings.filter((finding) => finding.status === 'fail')).toEqual([]);
      expect(result.findings.filter((finding) => finding.status === 'unknown')).toMatchObject([
        { page: 'gone', check: 'page', hard: false },
      ]);
      expect(result).toMatchObject({ passed: true, complete: false });
      expect(result.score).toBeGreaterThanOrEqual(90);
    });

    it('refuses what is not its own, and closes every context when it closes', async () => {
      const foreign = { ...persona, engine: subject.name === 'firefox' ? 'chromium' : 'firefox' };
      await expect(
        engine.openContext(openRequest(foreign as FingerprintDocument, 'x1')),
      ).rejects.toMatchObject({
        code: 'invalid_request',
      });
      await expect(
        engine.openContext(
          openRequest(persona, 'x2', { screen: { ...persona.native.screen, width: 1024 } }),
        ),
      ).rejects.toMatchObject({ code: 'invalid_request' });
      await expect(
        engine.generatePersona({
          seed: SEEDS.first,
          claimed_os: subject.claimedOs,
          languages: LANGUAGES,
          browser_major: subject.browserMajor + 1,
        }),
      ).rejects.toMatchObject({ code: 'browser_major_mismatch' });

      const context = await engine.openContext(openRequest(persona, 'closing'));
      await engine.close();
      await context.close();
      // A closed context answers like a page that went away.
      expect((await go(context, named('/landing'))).error).toMatchObject({ code: 'aborted' });
    });
  });
}

function never(): never {
  throw new Error('the list was empty');
}
