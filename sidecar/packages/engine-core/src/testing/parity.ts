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
import { appendFileSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { networkInterfaces, tmpdir, userInfo } from 'node:os';
import path from 'node:path';

import type {
  CaptureOptions,
  ClaimedOs,
  EngineName,
  Entry,
  FlowEvent,
  FlowRequest,
  NavigateRequest,
  OpenContextRequest,
  PageLink,
  PageResult,
  SideEffect,
} from '@kage/contract';
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest';

import { ArtifactWriter } from '../artifacts.js';
import { HUMAN_BOUNDS, motorProfile } from '../behavior/profile.js';
import { allowedCpus } from '../cpus.js';
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
import {
  activationReportOf,
  inputReportOf,
  inputStatistics,
  pressesOf,
  touched,
  type LeaveReport,
} from './recorder.js';
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
  /**
   * What the browser tells an untouched page when asked whether sound may
   * start by itself (`navigator.getAutoplayPolicy()`): the answer of that
   * browser as nobody configured it. `null` where the browser has no such call.
   */
  readonly autoplayPolicy: { readonly mediaelement: string; readonly audiocontext: string } | null;
  /** How the browser names a refused tunnel and a proxy that is not there; `null` when it gives no name. */
  readonly netErrors: { readonly tunnel: string | null; readonly deadProxy: string | null };
  /**
   * What the probe's window must read where the engine's document fixes
   * it, for an engine whose document holds the browser's values (Firefox:
   * the configuration it applies). Absent for an engine whose page reads
   * the browser's own (Chromium).
   */
  readonly documentReads?: (document: FingerprintDocument) => DocumentReads;
  /**
   * The same persona's document as the engine's previous browser major made
   * it, for an engine that moves documents to a new major and keeps the
   * profile before its first start under it (Firefox). Absent: the suite
   * does not move a persona.
   */
  readonly olderDocument?: (document: FingerprintDocument) => FingerprintDocument;
  /**
   * Whether a frame of one site, inside a page of another, sees nothing of
   * the storage its site set as the first party, as in the browser nobody
   * configured (true for both engines since the Chrome switch audit).
   * `false` would record a measured tell of an engine, with its cause, so
   * that the test notices when it changes.
   */
  readonly thirdPartyStoragePartitioned: boolean;
}

/** Values of the probe's window that a fingerprint document fixes; a field left out is not compared. */
export interface DocumentReads {
  readonly user_agent?: string;
  readonly platform?: string;
  readonly oscpu?: string;
  readonly app_version?: string;
  readonly hardware_concurrency?: number;
  readonly screen?: {
    readonly width: number;
    readonly height: number;
    readonly avail_width: number;
    readonly avail_height: number;
    readonly color_depth: number;
  };
  readonly webgl?: { readonly vendor: string; readonly renderer: string };
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

  /** The paths the site was asked for since its last reset that begin as given, in order. */
  const askedFor = async (start: string) =>
    (await site.requests())
      .map((request) => request.path)
      .filter((asked) => asked.startsWith(start));

  /** What one load of the leave recorder reported when its page went away. */
  const leaveReport = async (run: string): Promise<LeaveReport> => {
    for (let waited = 0; waited < 5000; waited += 100) {
      const found = (await site.reports()).find(
        (report) => report['page'] === 'leave' && report['run'] === run,
      );
      if (found !== undefined) {
        return found as unknown as LeaveReport;
      }
      await sleep(100);
    }
    throw new Error(`the page of the run ${run} reported nothing: it was not left`);
  };

  /**
   * Writes down what a test measured, where `KAGE_PARITY_NOTES` names a
   * file: numbers for the record of a milestone, one JSON line each. The
   * tests assert what must hold; this keeps what was seen.
   */
  const measured = (what: string, values: Record<string, unknown>) => {
    const file = process.env['KAGE_PARITY_NOTES'];
    if (file !== undefined && file !== '') {
      appendFileSync(file, `${JSON.stringify({ engine: subject.name, what, ...values })}\n`);
    }
  };

  /** How many events of each type a recorder saw, and when the first and last came. */
  const kinds = (events: readonly { type: string; t: number }[]) => ({
    count: Object.fromEntries(
      [...new Set(events.map((event) => event.type))].map((type) => [
        type,
        events.filter((event) => event.type === type).length,
      ]),
    ),
    first_t: events[0]?.t ?? null,
    last_t: events.at(-1)?.t ?? null,
  });

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

    it('reads a page nobody touched without granting it user activation', async () => {
      await withContext('untouched', async (context) => {
        // Everything a navigation reads of a page: a wait for a selector,
        // the title and the text right after `load`, the document at the
        // end. Meanwhile the page asks its browser, at `load`, 300 ms
        // later and 4 s later, whether a user has touched it.
        const result = await go(context, named('/activation/'), {
          wait: { until: 'load', selector: '#kage-activation-report[data-done]' },
          capture: DOM,
          timeout_ms: 30_000,
        });
        expect(result.error).toBeNull();
        // The captures were taken all the same.
        expect(result).toMatchObject({
          status: 200,
          title: 'Fixture activation recorder',
          behavior: null,
        });
        expect(result.dom_html).toContain('<h1 id="title">Activation recorder</h1>');
        const loaded = activationReportOf(result.dom_html);
        expect(loaded.samples.map((sample) => sample.at)).toEqual([
          'start',
          'load',
          'load+300ms',
          'load+4000ms',
        ]);
        for (const sample of loaded.samples) {
          expect(sample, sample.at).toMatchObject({
            is_active: false,
            has_been_active: false,
            // Sound waits for the user: a context made at any of the moments.
            audio_new: 'suspended',
            activating_input_events: 0,
          });
          expect(touched(sample), sample.at).toBe(false);
        }
        expect(loaded.untrusted_input_events).toBe(0);
        // Asked outright, the browser says what one nobody configured says.
        expect(loaded.autoplay_policy).toEqual(subject.autoplayPolicy);

        // The reads of a flow, then a mouse that only moves, then a click.
        const run = await flow(context, {
          behavior: { seed: SEEDS.first },
          timeout_ms: 90_000,
          actions: [
            { action: 'wait_for', selector: '#press' },
            { action: 'assert', text_contains: 'Nobody has to touch it' },
            { action: 'assert', selector: '#kage-activation-report[data-done]' },
            { action: 'wait_for', ms: 600 },
            { action: 'extract', capture: DOM },
            { action: 'hover', selector: '#far' },
            { action: 'wait_for', ms: 600 },
            { action: 'extract', capture: DOM },
            { action: 'click', selector: '#press' },
            { action: 'wait_for', ms: 600 },
            { action: 'extract', capture: DOM },
          ],
        });
        expect(run.steps.filter((step) => !step.ok)).toEqual([]);
        const [read, moved, clicked] = run.steps
          .filter((step) => step.action === 'extract')
          .map((step) => activationReportOf(step.capture?.dom_html ?? null).now);
        // Still nobody's: reading it again and again changed nothing.
        expect(read).toMatchObject({ is_active: false, has_been_active: false });
        expect(touched(read ?? never())).toBe(false);
        // Moving the mouse is input, and no reason for a browser to
        // believe that the user wants anything.
        expect(moved?.trusted_input_events).toBeGreaterThan((read?.trusted_input_events ?? 0) + 5);
        expect(moved).toMatchObject({
          is_active: false,
          has_been_active: false,
          activating_input_events: 0,
        });
        expect(touched(moved ?? never())).toBe(false);
        // A click is: activation comes from input, as it does for a person.
        expect(clicked).toMatchObject({ is_active: true, has_been_active: true });
        expect(clicked?.activating_input_events).toBeGreaterThanOrEqual(2);
        expect(run.final.behavior?.clicks).toBe(1);
      });
    });

    it('sends nothing at all on a still visit, and reports the visit all the same (1.5.0)', async () => {
      await withContext('still', async (context) => {
        await site.reset();
        const run = 'still-1';
        const leave = secure(`/leave/?run=${run}`);
        // The engine holds the rule of the contract too: a still visit that
        // asks for a hand is refused, and nothing is loaded for it.
        await expect(
          go(context, leave, {
            behavior: { seed: SEEDS.first, mode: 'still', scroll_depth: 0.5, wander: true },
          }),
        ).rejects.toMatchObject({ code: 'invalid_request' });
        expect(await askedFor('/leave/')).toEqual([]);

        const began = Date.now();
        const visit = await go(context, leave, {
          wait: { until: 'load', selector: 'body[data-ready]' },
          dwell_ms: 1500,
          behavior: { seed: SEEDS.first, mode: 'still', entry: { strategy: 'direct' } },
        });
        const took = Date.now() - began;
        expect(visit.error).toBeNull();
        expect(visit).toMatchObject({ status: 200, final_url: leave, title: 'Leave' });
        expect(visit.behavior).toEqual({
          seed: SEEDS.first,
          profile_id: motorProfile(persona.seed).id,
          entry: {
            strategy: 'direct',
            outcome: 'arrived',
            from_url: null,
            referer: null,
            backs: null,
          },
          dwell_ms: 1500,
          moves: 0,
          positions: 0,
          overshoots: 0,
          clicks: 0,
          notches: 0,
          keys: 0,
          typos: 0,
          wanders: 0,
          patience_ms: null,
          scroll_reached: null,
          scroll_ended_by: null,
        });
        expect(took).toBeGreaterThanOrEqual(1500);
        // Arrived as a typed address arrives.
        const arrived = await lastRequest(`/leave/?run=${run}`);
        expect(headerOf(arrived, 'referer')).toBeNull();
        expect(headerOf(arrived, 'sec-fetch-site')).toBe('none');

        // Left by a typed address too; then the page says what it saw of its visitor.
        const next = await go(context, secure('/landing?from=still'));
        expect(next.error).toBeNull();
        const left = await leaveReport(run);
        const saw = left.events.map((event) => event.type);
        measured('a still visit', {
          took_ms: took,
          saw,
          moves: left.moves,
          first_move: left.firstMove,
          wheels: left.wheels,
          activation: left.activation,
        });
        // No wheel, no button, no key, and nothing a browser takes for its user.
        expect(left).toMatchObject({ wheels: 0, scrollY: 0, activation: false });
        expect(pressesOf(left)).toEqual([]);
        // And no hand: Chrome itself tells a page that has just loaded
        // where a resting pointer is, with one `mousemove` about 100 ms in,
        // as it does on a person's machine (docs/runbooks/engine-chromium.md);
        // Firefox tells nothing. Any other movement, and any during the
        // dwell, would have been sent.
        expect(left.moves).toBeLessThanOrEqual(1);
        if (left.moves === 1) {
          expect(left.firstMove).toBe(left.lastMove);
          expect(left.firstMove ?? Number.NaN).toBeLessThan(1000);
        } else {
          expect(left.firstMove).toBeNull();
        }
        for (const input of ['untrusted-mousemove', 'keydown', 'keyup']) {
          expect(saw, input).not.toContain(input);
        }
        expect(saw).toContain('pagehide');
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
      const seen = async (context: EngineContext) => ({
        ...(await probe(context)),
        applied: context.applied,
      });
      const first = await withContext('same-1', seen);
      measured('the window of a persona', {
        applied: first.applied,
        hardware_concurrency: first.scopes.window.hardware_concurrency,
        screen: first.scopes.window.screen,
        webgl: first.scopes.window.webgl,
      });
      const second = await withContext('same-2', seen);
      expect(second.scopes).toEqual(first.scopes);
      expect(second.environment).toEqual(first.environment);
      // Another persona is another browser: each window shows what was
      // applied for its persona, and that is the persona's own document,
      // but that a browser that counts the CPUs it runs on (Chrome) has no
      // more of them than the host. Two personas that differ only in their
      // CPUs, and ask for more than a small host has, look alike there.
      const other = await withContext('same-3', seen, {
        document: otherPersona,
        change: { persona_id: '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8f' },
      });
      expect(otherPersona.native).not.toEqual(persona.native);
      const host = allowedCpus().length;
      for (const [one, document] of [
        [first, persona],
        [other, otherPersona],
      ] as const) {
        expect(one.applied.screen).toEqual(document.native.screen);
        const asked = document.native.hardware_concurrency;
        expect(one.applied.hardware_concurrency).toBeGreaterThanOrEqual(Math.min(asked, host));
        expect(one.applied.hardware_concurrency).toBeLessThanOrEqual(asked);
        expect(one.scopes.window).toMatchObject({
          user_agent: first.scopes.window.user_agent,
          hardware_concurrency: one.applied.hardware_concurrency,
          screen: {
            width: one.applied.screen.width,
            height: one.applied.screen.height,
            color_depth: one.applied.screen.color_depth,
          },
        });
      }
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
      for (const check of [
        'coherence.fonts',
        'coherence.webgl',
        'fonts.foreign',
        'webgl.os',
        // Loading and reading the probe granted it nothing (spec §10.2).
        'automation.user_activation',
        'automation.audio_autoplay',
      ]) {
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
      // Nothing of the driver is there for a page to find (the Camoufox 156
      // milestone measured both on Camoufox 152).
      for (const check of [
        'automation.driver_worker',
        'automation.extra_globals',
        'coherence.intl_language',
      ]) {
        expect(findings.find((finding) => finding.check === check)?.status, check).toBe('pass');
      }
      // What the window reads is what the document says, where it says it.
      if (subject.documentReads !== undefined) {
        const want = subject.documentReads(persona);
        const seen = report.scopes.window;
        expect({
          ...(want.user_agent === undefined ? {} : { user_agent: seen.user_agent }),
          ...(want.platform === undefined ? {} : { platform: seen.platform }),
          ...(want.oscpu === undefined ? {} : { oscpu: seen.oscpu }),
          ...(want.app_version === undefined ? {} : { app_version: seen.app_version }),
          ...(want.hardware_concurrency === undefined
            ? {}
            : { hardware_concurrency: seen.hardware_concurrency }),
          ...(want.screen === undefined
            ? {}
            : {
                screen: {
                  width: seen.screen?.width,
                  height: seen.screen?.height,
                  avail_width: seen.screen?.avail_width,
                  avail_height: seen.screen?.avail_height,
                  color_depth: seen.screen?.color_depth,
                },
              }),
          ...(want.webgl === undefined
            ? {}
            : { webgl: { vendor: seen.webgl?.vendor, renderer: seen.webgl?.renderer } }),
        }).toEqual(want);
      }
    });

    it.skipIf(subject.olderDocument === undefined)(
      'moves a persona of the previous browser to this one, which passes the probe, and keeps its profile first',
      async () => {
        const older = subject.olderDocument?.(persona);
        if (older === undefined) {
          return;
        }
        const moved = await engine.upgradePersona({ fingerprint: older });
        expect(moved.upgraded).toBe(true);
        // A persona that moves is one made new on this browser.
        expect(moved.fingerprint).toEqual(persona);
        expect(moved.summary.browser_major).toBe(subject.browserMajor);
        expect(await engine.upgradePersona({ fingerprint: persona })).toMatchObject({
          upgraded: false,
          fingerprint: persona,
        });
        // Its profile last ran under the previous major.
        const userData = path.join(root, 'personas', 'personas', 'moved', 'user-data');
        await mkdir(userData, { recursive: true });
        await writeFile(
          path.join(userData, 'compatibility.ini'),
          `[Compatibility]\nLastVersion=${String(subject.browserMajor - 1)}.0_20260101000000/20260101000000\n`,
        );
        await site.reset();
        const report = await withContext('moved', probe, {
          document: moved.fingerprint as FingerprintDocument,
        });
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
        // The profile as it was is kept on the personas volume.
        const note = JSON.parse(
          await readFile(
            path.join(
              root,
              'personas',
              '.upgrade',
              '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e',
              'upgrade.json',
            ),
            'utf8',
          ),
        ) as Record<string, unknown>;
        expect(note).toMatchObject({
          profile_dir: 'personas/moved',
          from_major: subject.browserMajor - 1,
        });
      },
    );

    it('keeps what a site stored as the first party out of its frames in other sites', async () => {
      await site.reset();
      await withContext('partition', async (context) => {
        // The frame's site stores a cookie and a value as the first party…
        await go(context, secure('/partition/set'), {
          wait: { until: 'load', selector: 'body[data-done]' },
        });
        // …and is then a frame in a page of another site.
        await go(
          context,
          named(`/partition/top?frame=${encodeURIComponent(secure('/partition/frame'))}`),
          {
            wait: { until: 'load', selector: 'body[data-done]' },
          },
        );
      });
      const report = (await site.reports()).find((entry) => entry['page'] === 'partition');
      measured('storage of a frame in another site', { report });
      // The first-party cookie is Lax by default and stays out of the frame
      // in both browsers. Camoufox 152 showed the frame both the cookie and
      // the stored value (its cookie behaviour was Playwright's 4, not
      // Firefox's 5).
      expect(report).toMatchObject({ cookie: '' });
      expect(report?.['stored']).toBe(subject.thirdPartyStoragePartitioned ? null : 'first-party');
    });

    it('blocks a window a page opens with nobody’s click before it', async () => {
      await site.reset();
      await withContext('popup', (context) =>
        go(context, named('/popup.html'), { wait: { until: 'load', selector: 'body[data-done]' } }),
      );
      const report = (await site.reports()).find((entry) => entry['page'] === 'popup');
      // Camoufox 152 opened it: its popup blocker was off.
      expect(report).toMatchObject({ opened: false });
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
        // The times of the result count from the click, when the target was
        // asked for: its first byte came some milliseconds after it.
        expect(linked.timing.ttfb_ms ?? 0).toBeGreaterThan(1);
        expect(linked.timing.ttfb_ms ?? Number.NaN).toBeLessThanOrEqual(
          linked.timing.load_ms ?? Number.NaN,
        );
        measured('a click on a page the entry loaded', { ttfb_ms: linked.timing.ttfb_ms });
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

    it('clicks on a page it loaded to start from as on the page that is open: over two lines, not under a cover or for another tab, and no page of a page that answers itself or of a file', async () => {
      await withContext('entry-odd', async (context) => {
        const odd = (run: string) => secure(`/leave/odd?run=${run}`);
        const enter = async (
          run: string,
          landing: string,
          fallback: 'stop' | 'none' = 'stop',
          seed: string = SEEDS.first,
        ) => {
          await site.reset();
          const began = Date.now();
          const result = await go(context, secure(landing), {
            behavior: { seed, entry: { strategy: 'link', from_url: odd(run), fallback } },
          });
          return { result, took: Date.now() - began };
        };
        /** Leaves the page, so that it says what it saw. */
        const left = async (run: string) => {
          await go(context, secure('/landing?from=elsewhere'));
          return leaveReport(run);
        };

        // A link that runs over the end of a line is clicked on one of its
        // lines, not in the rectangle around them, which holds other text;
        // wherever on it the hand of either seed aims.
        for (const seed of [SEEDS.first, SEEDS.other]) {
          const wrapped = await enter('wrapped', '/landing?from=wrapped', 'stop', seed);
          expect(wrapped.result.error, seed).toBeNull();
          expect(wrapped.result.behavior?.entry, seed).toMatchObject({
            outcome: 'arrived',
            from_url: odd('wrapped'),
            referer: odd('wrapped'),
          });
          expect(pressesOf(await leaveReport('wrapped')), seed).toEqual([
            ['mousedown', true, 'wrapped'],
            ['mouseup', true, 'wrapped'],
            ['click', true, 'wrapped'],
          ]);
        }

        // Something lies over the link: the hand goes there and does not click.
        const covered = await enter('covered', '/landing?from=covered');
        expect(covered.result.error).toMatchObject({ code: 'aborted' });
        expect(covered.result.behavior).toMatchObject({
          clicks: 0,
          entry: { outcome: 'link_not_found', referer: null },
        });
        expect(await askedFor('/landing?from=covered')).toEqual([]);
        expect(pressesOf(await left('covered'))).toEqual([]);

        // A link that opens another tab is none for this one: no tab is opened.
        const otherTab = await enter('tab', '/landing?from=tab');
        expect(otherTab.result.error).toMatchObject({ code: 'aborted' });
        expect(otherTab.result.behavior).toMatchObject({
          clicks: 0,
          moves: 0,
          entry: { outcome: 'link_not_found' },
        });
        expect(await askedFor('/landing?from=tab')).toEqual([]);
        expect(pressesOf(await left('tab'))).toEqual([]);

        // A page that answers the click itself: no page came of it, and the
        // wait for one ends soon, not after the quarter minute a document is
        // given (the request itself is shorter than that). With the fallback
        // the address is then typed.
        const soft = await enter('soft', '/landing?from=soft');
        expect(soft.result.error).toMatchObject({ code: 'aborted' });
        expect(soft.result.behavior).toMatchObject({
          clicks: 1,
          entry: { outcome: 'link_not_found', referer: null },
        });
        expect(soft.took).toBeLessThan(15_000);
        expect(await askedFor('/landing?from=soft')).toEqual([]);
        expect(pressesOf(await left('soft')).at(-1)).toEqual(['click', true, 'soft']);
        const softTyped = await enter('soft-typed', '/landing?from=soft', 'none');
        expect(softTyped.result).toMatchObject({ status: 200, error: null, title: 'Landing' });
        expect(softTyped.result.behavior?.entry).toMatchObject({ outcome: 'fell_back' });
        expect(await askedFor('/landing?from=soft')).toEqual(['/landing?from=soft']);
        measured('a link the page answers itself, on a page the entry loaded', {
          took_ms: soft.took,
        });

        // A link the browser answers by saving a file: the result is of no
        // page, not of the page that was open under the address of the file,
        // and nothing is typed after it, whatever the fallback says.
        const file = await enter('file', '/download?from=odd', 'none');
        expect(file.result.error).toMatchObject({ code: 'aborted' });
        expect(file.result).toMatchObject({
          status: null,
          title: null,
          final_url: secure('/download?from=odd'),
        });
        expect(file.result.behavior).toMatchObject({ clicks: 1, entry: null });
        expect(await askedFor('/download')).toEqual(['/download?from=odd']);
        expect(headerOf(await lastRequest('/download?from=odd'), 'referer')).toBe(odd('file'));
        expect(pressesOf(await left('file')).at(-1)).toEqual(['click', true, 'file']);
      });
    });

    it('clicks a link of the page that is open, as its reader would, and loads nothing to start from', async () => {
      await withContext('open-click', async (context) => {
        // An ordinary visit: the page loads, and its reader stays a moment.
        const visit = (url: string) =>
          go(context, url, {
            wait: { until: 'load', selector: 'body[data-ready]' },
            dwell_ms: 1200,
            behavior: { seed: SEEDS.first, wander: true },
          });

        // The control first, in a tab nobody has clicked in: the page is
        // left for a typed address, and has seen nothing of a hand.
        await site.reset();
        expect((await visit(secure('/leave/?run=typed'))).error).toBeNull();
        const typed = await go(context, secure('/landing?from=typed'), {
          behavior: { seed: SEEDS.other },
        });
        expect(typed.behavior).toMatchObject({ clicks: 0, entry: null });
        const typedRequest = await lastRequest('/landing?from=typed');
        expect(headerOf(typedRequest, 'referer')).toBeNull();
        expect(headerOf(typedRequest, 'sec-fetch-site')).toBe('none');
        const unclicked = await leaveReport('typed');
        expect(pressesOf(unclicked)).toEqual([]);
        expect(unclicked.activation).toBe(false);
        measured('a page left for a typed address', {
          saw: unclicked.events.map((event) => event.type),
          moves: unclicked.moves,
          activation: unclicked.activation,
        });

        for (const [run, scrolls] of [
          ['next', false],
          ['below', true],
        ] as const) {
          await site.reset();
          const leave = secure(`/leave/?run=${run}`);
          const landing = `/landing?from=${run === 'next' ? 'leave' : 'below'}`;
          const target = secure(landing);
          expect((await visit(leave)).error, run).toBeNull();
          const began = Date.now();
          const next = await go(context, target, {
            capture: DOM,
            behavior: {
              seed: SEEDS.other,
              entry: { strategy: 'link', from_url: leave, from_open_page: true },
            },
          });
          expect(next.error, run).toBeNull();
          expect(next, run).toMatchObject({ status: 200, final_url: target, title: 'Landing' });
          expect(next.behavior, run).toMatchObject({
            seed: SEEDS.other,
            clicks: 1,
            keys: 0,
            entry: {
              strategy: 'link',
              outcome: 'arrived',
              from_url: leave,
              referer: leave,
              backs: 0,
            },
          });
          // What the site received is what a reader's click sends.
          const request = await lastRequest(landing);
          expect(headerOf(request, 'referer'), run).toBe(leave);
          expect(headerOf(request, 'sec-fetch-site'), run).toBe('same-origin');
          expect(headerOf(request, 'sec-fetch-user'), run).toBe('?1');
          expect(headerOf(request, 'sec-fetch-mode'), run).toBe('navigate');
          expect(headerOf(request, 'sec-fetch-dest'), run).toBe('document');
          // Nothing was loaded to start from: the page was asked for once,
          // by the visit before.
          expect(await askedFor('/leave/'), run).toEqual([`/leave/?run=${run}`]);
          // What the page that was left saw of its visitor: the pointer on
          // its way, a button going down and up on the link, a click no
          // script made, and then the end. It was touched, and knows it.
          const left = await leaveReport(run);
          const order = left.events.map((event) => event.type);
          expect(pressesOf(left), run).toEqual([
            ['mousedown', true, run],
            ['mouseup', true, run],
            ['click', true, run],
          ]);
          expect(order.indexOf('click'), run).toBeLessThan(order.indexOf('pagehide'));
          expect(order, run).not.toContain('untrusted-mousemove');
          expect(order, run).not.toContain('keydown');
          expect(left.activation, run).toBe(true);
          expect(left.moves, run).toBeGreaterThan(10);
          if (scrolls) {
            // The link was below the window: the wheel brought it up.
            expect(left.wheels, run).toBeGreaterThan(0);
            expect(left.scrollY, run).toBeGreaterThan(200);
            expect(next.behavior?.notches, run).toBeGreaterThan(0);
          } else {
            expect(left.wheels, run).toBe(0);
            expect(next.behavior?.notches, run).toBe(0);
          }
          // The hand takes its time to get there, and the times of the
          // result count from the click, not from the start of the request.
          const travelled = (request?.at ?? Number.NaN) - began;
          expect(travelled, run).toBeGreaterThan(300);
          expect(next.timing.ttfb_ms ?? Number.NaN, run).toBeLessThan(travelled);
          expect(next.timing.total_ms, run).toBeGreaterThanOrEqual(travelled);
          measured('a click on the open page', {
            link: run,
            request_after_ms: travelled,
            ttfb_ms: next.timing.ttfb_ms,
            saw: order,
            moves: left.moves,
            wheels: left.wheels,
            activation: left.activation,
            click_to_pagehide_ms:
              (left.events.find((event) => event.type === 'pagehide')?.t ?? Number.NaN) -
              (left.events.find((event) => event.type === 'click')?.t ?? Number.NaN),
          });
        }
      });
    });

    it('goes back in the tab to the page that holds the link and clicks there, at no cost to the site', async () => {
      const articles = secure('/site/articles');
      const from = (page: string, backs?: number) => ({
        strategy: 'link' as const,
        from_url: page,
        from_open_page: true,
        ...(backs === undefined ? {} : { max_backs: backs }),
      });
      await withContext('back-click', async (context) => {
        await site.reset();
        expect(
          (await go(context, articles, { dwell_ms: 600, behavior: { seed: SEEDS.first } })).error,
        ).toBeNull();
        const first = await go(context, secure('/site/a'), {
          behavior: { seed: SEEDS.first, entry: from(articles) },
        });
        expect(first.behavior?.entry).toMatchObject({
          outcome: 'arrived',
          referer: articles,
          backs: 0,
        });

        // The next article is linked from the page before: one entry back.
        const began = Date.now();
        const second = await go(context, secure('/site/b'), {
          capture: DOM,
          behavior: { seed: SEEDS.other, entry: from(articles, 3) },
        });
        expect(second.error).toBeNull();
        expect(second).toMatchObject({
          status: 200,
          final_url: secure('/site/b'),
          title: 'The second article',
        });
        // Alt and the left arrow once, then one click.
        expect(second.behavior).toMatchObject({
          clicks: 1,
          keys: 2,
          entry: {
            strategy: 'link',
            outcome: 'arrived',
            from_url: articles,
            referer: articles,
            backs: 1,
          },
        });
        const request = await lastRequest('/site/b');
        expect(headerOf(request, 'referer')).toBe(articles);
        expect(headerOf(request, 'sec-fetch-site')).toBe('same-origin');
        expect(headerOf(request, 'sec-fetch-user')).toBe('?1');
        // The page that came back came out of the browser: the site was
        // asked for it once, when it was first read.
        expect(await askedFor('/site/')).toEqual(['/site/articles', '/site/a', '/site/b']);
        measured('back, then a click', {
          backs: 1,
          request_after_ms: (request?.at ?? Number.NaN) - began,
          ttfb_ms: second.timing.ttfb_ms,
          asked: await askedFor('/site/'),
        });

        // Two entries back: on to the home page by the navigation, and
        // from there the third article is linked two pages ago.
        const home = await go(context, secure('/site/'), {
          behavior: { seed: SEEDS.first, entry: from(secure('/site/b')) },
        });
        expect(home.behavior?.entry).toMatchObject({ outcome: 'arrived', backs: 0 });
        const startedTwo = Date.now();
        const third = await go(context, secure('/site/c'), {
          behavior: { seed: SEEDS.other, entry: from(articles, 3) },
        });
        expect(third).toMatchObject({ status: 200, title: 'The third article', error: null });
        expect(third.behavior).toMatchObject({
          clicks: 1,
          keys: 4,
          entry: { outcome: 'arrived', referer: articles, backs: 2 },
        });
        expect(headerOf(await lastRequest('/site/c'), 'referer')).toBe(articles);
        expect(await askedFor('/site/')).toEqual([
          '/site/articles',
          '/site/a',
          '/site/b',
          '/site/',
          '/site/c',
        ]);
        measured('back, then a click', {
          backs: 2,
          request_after_ms: ((await lastRequest('/site/c'))?.at ?? Number.NaN) - startedTwo,
          ttfb_ms: third.timing.ttfb_ms,
        });

        // No further back than it may: the listing is two entries back
        // again, and one is allowed. The browser says so beforehand, so
        // nothing moves, no key is pressed, and nothing is asked for.
        const about = await go(context, secure('/site/about'), {
          behavior: { seed: SEEDS.first, entry: from(secure('/site/c')) },
        });
        expect(about.behavior?.entry).toMatchObject({ outcome: 'arrived', backs: 0 });
        const before = await askedFor('/site/');
        const tooFar = await go(context, secure('/site/dup-1'), {
          behavior: { seed: SEEDS.other, entry: from(articles, 1) },
        });
        expect(tooFar.error).toMatchObject({ code: 'aborted' });
        expect(tooFar.behavior).toMatchObject({
          clicks: 0,
          keys: 0,
          moves: 0,
          entry: { outcome: 'link_not_found', referer: null, backs: null },
        });
        expect(await askedFor('/site/')).toEqual(before);
        // The tab is where it was: a link of the page that was open is still a link of the open page.
        const still = await go(context, secure('/site/private'), {
          behavior: { seed: SEEDS.first, entry: from(secure('/site/about')) },
        });
        expect(still.behavior?.entry).toMatchObject({
          outcome: 'arrived',
          referer: secure('/site/about'),
          backs: 0,
        });
      });

      // A page of another origin: the browser does not name its entry to
      // the page that is open, so the persona goes and looks, one entry at
      // a time, and only because this tab has shown that page.
      await withContext('back-click-origins', async (context) => {
        await site.reset();
        const elsewhere = `http://other.localhost:${site.port}/site/a`;
        expect((await go(context, articles)).error).toBeNull();
        expect((await go(context, elsewhere)).error).toBeNull();
        const back = await go(context, secure('/site/b'), {
          behavior: { seed: SEEDS.other, entry: from(articles, 2) },
        });
        expect(back).toMatchObject({ status: 200, title: 'The second article', error: null });
        expect(back.behavior).toMatchObject({
          clicks: 1,
          keys: 2,
          entry: { outcome: 'arrived', referer: articles, backs: 1 },
        });
        expect(await askedFor('/site/')).toEqual(['/site/articles', '/site/a', '/site/b']);
      });
    });

    it('types the address when the tab does not hold the page, or the page no link, and only when it may', async () => {
      const a = secure('/site/a');
      const b = secure('/site/b');
      const notes = secure('/site/a/notes');
      const from = (page: string, more: Partial<Entry> = {}) => ({
        strategy: 'link' as const,
        from_url: page,
        from_open_page: true,
        ...more,
      });
      await withContext('open-missing', async (context) => {
        await site.reset();
        expect((await go(context, a)).error).toBeNull();

        // The tab never showed the listing. Nothing is loaded, nothing
        // moves, and the result is of no page.
        const stopped = await go(context, b, {
          capture: { ...DOM, links: true },
          behavior: { seed: SEEDS.first, entry: from(secure('/site/articles'), { max_backs: 3 }) },
        });
        expect(stopped.error).toMatchObject({ code: 'aborted' });
        expect(stopped.behavior).toMatchObject({
          clicks: 0,
          keys: 0,
          moves: 0,
          entry: {
            strategy: 'link',
            outcome: 'link_not_found',
            from_url: secure('/site/articles'),
            referer: null,
            backs: null,
          },
        });
        expect(stopped).toMatchObject({
          status: null,
          dom_html: null,
          links: null,
          links_truncated: false,
        });
        expect(await askedFor('/site/')).toEqual(['/site/a']);

        // Told to, it types the address: on the wire a typed navigation like any other.
        const typed = await go(context, b, {
          behavior: {
            seed: SEEDS.first,
            entry: from(secure('/site/articles'), { max_backs: 3, fallback: 'none' }),
          },
        });
        expect(typed).toMatchObject({ status: 200, error: null, final_url: b });
        expect(typed.behavior).toMatchObject({
          clicks: 0,
          keys: 0,
          entry: { outcome: 'fell_back', referer: null, backs: null },
        });
        const request = await lastRequest('/site/b');
        expect(headerOf(request, 'referer')).toBeNull();
        expect(headerOf(request, 'sec-fetch-site')).toBe('none');
        expect(headerOf(request, 'sec-fetch-user')).toBe('?1');

        // The page that links to the notes is one entry back, and nobody
        // said the persona may go back: it does not.
        const before = await askedFor('/site/');
        const stayed = await go(context, notes, {
          behavior: { seed: SEEDS.first, entry: from(a) },
        });
        expect(stayed.error).toMatchObject({ code: 'aborted' });
        expect(stayed.behavior).toMatchObject({
          clicks: 0,
          keys: 0,
          entry: { outcome: 'link_not_found', backs: null },
        });
        expect(await askedFor('/site/')).toEqual(before);
        // Allowed one entry, it goes: the page was there all along.
        const went = await go(context, notes, {
          behavior: { seed: SEEDS.first, entry: from(a, { max_backs: 1 }) },
        });
        expect(went.behavior).toMatchObject({
          clicks: 1,
          keys: 2,
          entry: { outcome: 'arrived', referer: a, backs: 1 },
        });

        // The open page is the page named, and holds no link to the target.
        const tall = secure('/site/tall');
        const linkless = await go(context, tall, {
          behavior: { seed: SEEDS.first, entry: from(notes) },
        });
        expect(linkless.error).toMatchObject({ code: 'aborted' });
        expect(linkless.behavior).toMatchObject({
          clicks: 0,
          keys: 0,
          entry: { outcome: 'link_not_found', referer: null, backs: null },
        });
        expect(await askedFor('/site/tall')).toEqual([]);
        const typedOn = await go(context, tall, {
          behavior: { seed: SEEDS.first, entry: from(notes, { fallback: 'none' }) },
        });
        expect(typedOn).toMatchObject({ status: 200, error: null });
        expect(typedOn.behavior?.entry).toMatchObject({ outcome: 'fell_back', backs: null });
        expect(headerOf(await lastRequest('/site/tall'), 'referer')).toBeNull();
        expect(headerOf(await lastRequest('/site/tall'), 'sec-fetch-site')).toBe('none');

        // A page two entries back that holds no link to the target either:
        // the persona goes back to it, finds none, and types.
        const third = secure('/site/c');
        const afterBack = await go(context, third, {
          behavior: { seed: SEEDS.first, entry: from(a, { max_backs: 3, fallback: 'none' }) },
        });
        expect(afterBack).toMatchObject({ status: 200, error: null, title: 'The third article' });
        expect(afterBack.behavior).toMatchObject({
          clicks: 0,
          keys: 4,
          entry: { outcome: 'fell_back', referer: null, backs: null },
        });
        expect(headerOf(await lastRequest('/site/c'), 'referer')).toBeNull();
        expect(headerOf(await lastRequest('/site/c'), 'sec-fetch-site')).toBe('none');
        // Going back cost the site nothing: every page was asked for once.
        expect(await askedFor('/site/')).toEqual([
          '/site/a',
          '/site/b',
          '/site/a/notes',
          '/site/tall',
          '/site/c',
        ]);
      });
    });

    it('does not click what its reader could not, and calls no page what is none: links under something else, for another tab, answered by the page itself, to a file', async () => {
      await withContext('open-odd', async (context) => {
        const odd = (run: string) => secure(`/leave/odd?run=${run}`);
        const attempt = async (
          run: string,
          landing: string,
          fallback: 'stop' | 'none' = 'stop',
        ) => {
          await site.reset();
          // A bare load: until the entry, the page sees no hand at all.
          expect((await go(context, odd(run))).error, run).toBeNull();
          const began = Date.now();
          const result = await go(context, secure(landing), {
            behavior: {
              seed: SEEDS.first,
              entry: { strategy: 'link', from_url: odd(run), from_open_page: true, fallback },
            },
          });
          return { result, took: Date.now() - began };
        };
        /** Leaves the page, so that it says what it saw. */
        const left = async (run: string) => {
          await go(context, secure('/landing?from=elsewhere'));
          return leaveReport(run);
        };

        // A link that runs over the end of a line is clicked on one of its
        // lines, not in the rectangle around them, which holds other text.
        const wrapped = await attempt('wrapped', '/landing?from=wrapped');
        expect(wrapped.result.error).toBeNull();
        expect(wrapped.result.behavior?.entry).toMatchObject({ outcome: 'arrived', backs: 0 });
        expect(pressesOf(await leaveReport('wrapped'))).toEqual([
          ['mousedown', true, 'wrapped'],
          ['mouseup', true, 'wrapped'],
          ['click', true, 'wrapped'],
        ]);

        // Something lies over the link. The hand goes there, sees that it
        // would click something else, and does not click.
        const covered = await attempt('covered', '/landing?from=covered');
        expect(covered.result.error).toMatchObject({ code: 'aborted' });
        expect(covered.result.behavior).toMatchObject({
          clicks: 0,
          entry: { outcome: 'link_not_found', referer: null, backs: null },
        });
        expect(covered.result.behavior?.moves).toBeGreaterThan(0);
        expect(await askedFor('/landing?from=covered')).toEqual([]);
        const underCover = await left('covered');
        expect(pressesOf(underCover)).toEqual([]);
        expect(underCover.moves).toBeGreaterThan(5);

        // A link that opens another tab is none for this one: the hand
        // stays where it is, and no tab is opened.
        const otherTab = await attempt('tab', '/landing?from=tab');
        expect(otherTab.result.error).toMatchObject({ code: 'aborted' });
        expect(otherTab.result.behavior).toMatchObject({
          clicks: 0,
          moves: 0,
          entry: { outcome: 'link_not_found' },
        });
        expect(await askedFor('/landing?from=tab')).toEqual([]);
        expect(pressesOf(await left('tab'))).toEqual([]);

        // A page that answers the click itself, with another address and
        // no document. The click was a click; no page came of it, and the
        // wait for one ends soon, not after the quarter minute a document
        // is given.
        const soft = await attempt('soft', '/landing?from=soft');
        expect(soft.result.error).toMatchObject({ code: 'aborted' });
        expect(soft.result.behavior).toMatchObject({
          clicks: 1,
          entry: { outcome: 'link_not_found', referer: null, backs: null },
        });
        expect(soft.took).toBeLessThan(10_000);
        expect(await askedFor('/landing?from=soft')).toEqual([]);
        expect(pressesOf(await left('soft')).at(-1)).toEqual(['click', true, 'soft']);
        measured('a link the page answers itself', { took_ms: soft.took });

        // With the fallback the address is typed: the site is asked for the
        // document once.
        const softTyped = await attempt('soft-typed', '/landing?from=soft', 'none');
        expect(softTyped.result).toMatchObject({ status: 200, error: null, title: 'Landing' });
        expect(softTyped.result.behavior?.entry).toMatchObject({
          outcome: 'fell_back',
          backs: null,
        });
        expect(await askedFor('/landing?from=soft')).toEqual(['/landing?from=soft']);
        expect(headerOf(await lastRequest('/landing?from=soft'), 'sec-fetch-site')).toBe('none');

        // A link the browser answers by saving a file. The click was a
        // click and was answered, and the page that was open is still the
        // page: the result is of no page, not of that one under the
        // address of the file (which is what both drivers report). Nothing
        // is typed after it either, whatever the fallback says.
        const file = await attempt('file', '/download?from=odd', 'none');
        expect(file.result.error).toMatchObject({ code: 'aborted' });
        expect(file.result).toMatchObject({
          status: null,
          title: null,
          final_url: secure('/download?from=odd'),
        });
        expect(file.result.behavior).toMatchObject({ clicks: 1, entry: null });
        expect(await askedFor('/download')).toEqual(['/download?from=odd']);
        expect(headerOf(await lastRequest('/download?from=odd'), 'referer')).toBe(odd('file'));
        expect(pressesOf(await left('file')).at(-1)).toEqual(['click', true, 'file']);
      });
    });

    it('lists the links of a document, with whether a reader can see each and whether it is furniture', async () => {
      const LINKS: CaptureOptions = { ...NO_CAPTURE, links: true };
      const link = (url: string, text: string, more: Partial<PageLink> = {}): PageLink => ({
        url,
        text,
        rel: [],
        visible: true,
        furniture: false,
        ...more,
      });
      await withContext('links', async (context) => {
        const home = await go(context, named('/site/'), { capture: LINKS });
        expect(home.error).toBeNull();
        expect(home.links_truncated).toBe(false);
        // In the order of the document, a link that occurs twice listed
        // twice; no mail address and no script. The navigation is
        // furniture; the five traps are in the document and before no
        // reader's eyes: by an inline style, by the hidden attribute of
        // what holds it, by a class of the style sheet, by a place off the
        // canvas, by a holder that is transparent.
        expect(home.links).toEqual([
          link(named('/site/'), 'Home', { furniture: true }),
          link(named('/site/articles'), 'Articles', { furniture: true }),
          link(named('/site/about'), 'About', { furniture: true }),
          link(named('/site/articles'), 'All articles'),
          link(named('/site/a?utm_source=home&utm_medium=fixture'), 'This week: the first article'),
          link(named('/site/about#team'), 'Who writes here'),
          link(named('/site/tall'), 'The long read'),
          link('http://offsite.example.test/hello', 'A friend of the house'),
          link(named('/site/files/report.pdf'), 'The yearly report'),
          link(named('/site/trap'), 'Do not follow', { visible: false }),
          link(named('/site/trap?from=hidden'), 'Nor this', { visible: false }),
          link(named('/site/trap?from=class'), 'Nor this one', { visible: false }),
          link(named('/site/trap?from=offstage'), 'Nor this one', { visible: false }),
          link(named('/site/trap?from=clear'), 'Nor this one', { visible: false }),
        ]);

        // Every way a link is in a document, and what it carries.
        const detail = await go(context, named('/links'), { capture: LINKS });
        const told = (detail.links ?? []).map((each) => [
          each.url.replace(named('/links'), ''),
          each.visible,
          each.furniture,
        ]);
        expect(told).toEqual([
          ['/header', true, true],
          // The role decides as the element does, however it is written.
          ['/role', true, true],
          ['/plain', true, false],
          ['/rel', true, false],
          // An anchor around a block has no box of its own, and one a reader sees.
          ['/block', true, false],
          ['/image', true, false],
          // No text and no box: nothing to see, nothing to click.
          ['/empty', false, false],
          ['/long', true, false],
          ['/pair', true, false],
          ['/blank', true, false],
          // Hidden with what holds it; shown again inside what is hidden.
          ['/veiled', false, false],
          ['/unveiled', true, false],
          // Transparent with what holds it, and by itself.
          ['/faded', false, false],
          ['/self-faded', false, false],
          ['/collapsed', false, false],
          // Above the canvas no reader scrolls; to the right the page does.
          ['/above', false, false],
          ['/far-right', true, false],
          // A link to the page itself, and one to a place on it.
          ['', true, false],
          ['#top', true, false],
          ['/drawn', true, false],
          ['/aside', true, true],
          ['/footer', true, true],
        ]);
        const detailOf = (name: string) =>
          (detail.links ?? []).find((each) => each.url === named(`/links/${name}`));
        // White space collapsed; the tokens of rel in lower case.
        expect(detailOf('plain')).toMatchObject({ text: 'A plain link', rel: [] });
        expect(detailOf('rel')).toMatchObject({
          text: 'one with a rel',
          rel: ['nofollow', 'noopener', 'ugc'],
        });
        expect(detailOf('image')?.text).toBe('');
        // At most 200 characters, and never half of one.
        expect(detailOf('long')?.text).toBe('word '.repeat(40).trim());
        expect(detailOf('pair')?.text).toBe('x'.repeat(199));
        expect(detailOf('drawn')?.text).toBe('Drawn');

        // No more than the contract holds, and it says when there were more.
        const full = await go(context, named('/links?many=4978'), { capture: LINKS });
        expect(full.links).toHaveLength(5000);
        expect(full.links_truncated).toBe(false);
        expect(full.links?.at(-1)?.url).toBe(named('/links/n/4977'));
        const over = await go(context, named('/links?many=4979'), { capture: LINKS });
        expect(over.links).toHaveLength(5000);
        expect(over.links_truncated).toBe(true);
        expect(over.links?.at(-1)?.url).toBe(named('/links/n/4977'));

        // Not asked for, not there; and of a page that did not arrive there are none.
        const unasked = await go(context, named('/site/'), { capture: DOM });
        expect(unasked).toMatchObject({ links: null, links_truncated: false });
        const failed = await go(context, named('/slow?ms=4000'), {
          capture: LINKS,
          timeout_ms: 1000,
        });
        expect(failed.error).toMatchObject({ code: 'timeout' });
        expect(failed).toMatchObject({ links: null, links_truncated: false });

        // A capture of a flow lists them the same way.
        const run = await flow(context, {
          actions: [
            { action: 'goto', url: named('/site/a') },
            { action: 'extract', capture: LINKS },
            { action: 'extract', capture: DOM },
          ],
        });
        expect(run.steps.filter((step) => !step.ok)).toEqual([]);
        expect(run.steps[1]?.capture?.links?.map((each) => each.url)).toEqual(
          ['/site/', '/site/articles', '/site/about', '/site/a/notes'].map(named),
        );
        expect(run.steps[2]?.capture).toMatchObject({ links: null, links_truncated: false });
        expect(run.final.result).toMatchObject({ links: null, links_truncated: false });
      });
    });

    it('leaves a long page when its patience is used up, and no two of them after the same time', async () => {
      await withContext('patience', async (context) => {
        await site.reset();
        const tall = named('/site/tall');
        const visit = async (
          url: string,
          behavior: NonNullable<NavigateRequest['behavior']>,
          change: Partial<NavigateRequest> = {},
        ) => {
          const began = Date.now();
          const result = await go(context, url, {
            dwell_ms: 2000,
            timeout_ms: 6000,
            ...change,
            behavior: { seed: SEEDS.first, wander: false, ...behavior },
          });
          return { result, behavior: result.behavior, took: Date.now() - began };
        };
        /** How far the tall page says it was read, for each time it was left. */
        const told = async () =>
          (await site.reports())
            .filter((report) => report['page'] === 'tall')
            .map(
              (report) =>
                Number(report['scrollY']) /
                (Number(report['scrollHeight']) - Number(report['innerHeight'])),
            );

        // A page of a dozen windows, and five seconds of patience for it.
        // The budget of the request (six seconds) is for reaching the page:
        // the visit takes the patience and the dwell, which is more.
        const short = await visit(tall, { scroll_depth: 0.9, patience_ms: 5000 });
        measured('patience, the first visit', { took_ms: short.took, behavior: short.behavior });
        expect(short.result.error).toBeNull();
        expect(short.behavior).toMatchObject({
          patience_ms: 5000,
          scroll_ended_by: 'patience',
          dwell_ms: 2000,
        });
        expect(short.behavior?.notches).toBeGreaterThan(0);
        expect(short.behavior?.scroll_reached).toBeGreaterThan(0.005);
        expect(short.behavior?.scroll_reached).toBeLessThan(0.6);
        expect(short.took).toBeGreaterThanOrEqual(5000 + 2000);
        expect(short.took).toBeLessThan(5000 + 2000 + 4500);

        // Another patience, another moment: the page is left four seconds later.
        const long = await visit(tall, { scroll_depth: 0.9, patience_ms: 9000 });
        expect(long.behavior).toMatchObject({
          patience_ms: 9000,
          scroll_ended_by: 'patience',
          dwell_ms: 2000,
        });
        expect(long.took).toBeGreaterThanOrEqual(9000 + 2000);
        expect(long.took).toBeLessThan(9000 + 2000 + 4500);
        expect(long.took - short.took).toBeGreaterThan(2500);
        expect(long.behavior?.notches).toBeGreaterThan(0);
        expect(long.behavior?.scroll_reached).toBeGreaterThan(0.005);
        expect(long.behavior?.scroll_reached).toBeLessThan(0.75);

        // Without a patience the budget of the request ends the scrolling,
        // as before 1.3.0, and the result says that it did.
        const budgeted = await visit(
          tall,
          { scroll_depth: 0.9 },
          { timeout_ms: 9000, dwell_ms: 1000 },
        );
        expect(budgeted.result.error).toBeNull();
        expect(budgeted.behavior).toMatchObject({ patience_ms: null, scroll_ended_by: 'budget' });
        expect(budgeted.behavior?.scroll_reached).toBeGreaterThan(0);
        expect(budgeted.behavior?.scroll_reached).toBeLessThan(0.6);
        expect(budgeted.took).toBeLessThan(9000);

        // A page that is read to its depth long before the patience ends
        // is left then: the patience is a bound, not a time to sit out.
        const read = await visit(
          named('/leave/?run=patient'),
          { scroll_depth: 0.9, patience_ms: 60_000 },
          { dwell_ms: 300, timeout_ms: 30_000 },
        );
        expect(read.behavior).toMatchObject({ patience_ms: 60_000, scroll_ended_by: 'depth' });
        expect(read.behavior?.scroll_reached).toBeGreaterThan(0.8);
        expect(read.took).toBeLessThan(40_000);
        // A page that ends inside its window has nothing to scroll.
        const whole = await visit(
          named('/landing'),
          { scroll_depth: 0.9, patience_ms: 20_000 },
          { dwell_ms: 300 },
        );
        expect(whole.behavior).toMatchObject({
          patience_ms: 20_000,
          scroll_ended_by: 'page',
          scroll_reached: 1,
          notches: 0,
        });
        expect(whole.took).toBeLessThan(8000);
        // And where no scrolling was asked for, none is reported.
        const unscrolled = await visit(named('/landing'), { patience_ms: 4000 }, { dwell_ms: 300 });
        expect(unscrolled.behavior).toMatchObject({
          patience_ms: 4000,
          scroll_reached: null,
          scroll_ended_by: null,
        });

        // The long page itself says how far it was read when it was left,
        // each of the three times: what the result reports is what it saw.
        const reached = [short, long, budgeted].map((each) => each.behavior?.scroll_reached ?? -1);
        const seen = await told();
        expect(seen).toHaveLength(3);
        for (const [index, share] of seen.entries()) {
          expect(Math.abs(share - (reached[index] ?? -1)), `visit ${index}`).toBeLessThan(0.005);
        }
        measured('patience on a long page', {
          took_ms: [short.took, long.took, budgeted.took],
          reached,
          page_said: seen,
          ended_by: [short, long, budgeted].map((each) => each.behavior?.scroll_ended_by),
          notches: [short, long, budgeted].map((each) => each.behavior?.notches),
        });
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
      const statistics = inputStatistics(result.report);
      measured('a wheel on a page', {
        behavior,
        window: result.report.window,
        events: kinds(result.report.events),
        wheel: statistics.wheel,
        scroll_events: statistics.scrollEvents,
      });
      expect(statistics.untrusted).toBe(0);
      // A wheel turns by the browser's own notch, and the page glides.
      // Chrome hands a page two notches of one frame as one event of twice
      // the size; the distance per notch is what both browsers agree on.
      const notches = behavior?.notches ?? 0;
      expect(statistics.wheel.count).toBeLessThanOrEqual(notches);
      expect(statistics.wheel.count).toBeGreaterThanOrEqual(notches / 2);
      expect(statistics.wheel.modes).toEqual([0]);
      expect(statistics.wheel.total / notches).toBeGreaterThanOrEqual(90);
      expect(statistics.wheel.total / notches).toBeLessThanOrEqual(130);
      expect(statistics.scrollEvents).toBeGreaterThan(statistics.wheel.count);
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
      const statistics = inputStatistics(report);
      // Everything a page can check about where input comes from.
      expect(statistics.untrusted).toBe(0);
      expect(statistics.pointerTypes).toEqual(['mouse']);
      expect(report.field).toBe(text);
      expect(report.choice).toBe('three');
      measured('a select by the hand and the keyboard', {
        events: report.events
          .filter((event) => event.target === 'choice' && event.type !== 'mousemove')
          .map((event) => [event.type, Math.round(event.t)]),
      });
      const targets = (type: string) =>
        report.events.filter((event) => event.type === type).map((event) => event.target);
      // The hand pressed on each of the four. A `<select>` opens its list
      // on the press, and the list may take the release, as it would a
      // person's: on GitHub's runners Firefox's page saw no click on it in
      // three runs of five. What the page saw on it is written down above.
      expect(targets('mousedown')).toEqual(
        expect.arrayContaining(['target', 'field', 'choice', 'bottom']),
      );
      expect(targets('click')).toEqual(expect.arrayContaining(['target', 'field', 'bottom']));
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
      expect(statistics.moves.length).toBeGreaterThanOrEqual(3);
      for (const move of statistics.moves) {
        expect(move.straightness).toBeGreaterThan(1);
        expect(move.duration).toBeGreaterThan(120);
        expect(move.duration).toBeLessThan(3200);
        expect(move.intervalSpread).toBeGreaterThan(1);
      }
      // Typing and clicking keep a person's times: the spec's 60 to 220 ms
      // between keys (with the slack of a busy machine), a rest before each
      // click, a button that stays down for a moment.
      expect(statistics.keyIntervals.length).toBeGreaterThanOrEqual(text.length - 2);
      const [low, high] = HUMAN_BOUNDS.keyIntervalMs;
      for (const interval of statistics.keyIntervals) {
        expect(interval).toBeGreaterThan(low - 25);
        expect(interval).toBeLessThan(high + 90);
      }
      for (const hold of statistics.buttonHolds) {
        expect(hold).toBeGreaterThan(30);
        expect(hold).toBeLessThan(260);
      }
      for (const hover of statistics.hovers) {
        expect(hover).toBeGreaterThan(40);
      }
      expect(run.final.behavior).toMatchObject({
        seed: SEEDS.first,
        profile_id: motorProfile(persona.seed).id,
        typos: 0,
        // A flow has no patience and no scrolling of a navigation to report.
        patience_ms: null,
        scroll_reached: null,
        scroll_ended_by: null,
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

    it('paces a flow when asked: stays on a page before it acts, and never as long between two clicks', async () => {
      const floor = 2200;
      const run = await withContext('flow-paced', (context) =>
        flow(context, {
          behavior: { seed: SEEDS.other, pace: { dwell_ms: 3000, min_dwell_ms: floor } },
          timeout_ms: 200_000,
          actions: [
            { action: 'goto', url: named('/input/') },
            { action: 'click', selector: '#target' },
            { action: 'click', selector: '#second' },
            { action: 'click', selector: '#target' },
            { action: 'click', selector: '#second' },
            { action: 'wait_for', ms: 400 },
            { action: 'extract', capture: DOM },
          ],
        }),
      );
      expect(run.steps.map((step) => [step.action, step.ok, step.error])).toEqual(
        ['goto', 'click', 'click', 'click', 'click', 'wait_for', 'extract'].map((action) => [
          action,
          true,
          null,
        ]),
      );
      const report = inputReportOf(run.steps.at(-1)?.capture?.dom_html ?? null);
      expect(inputStatistics(report).untrusted).toBe(0);
      // The recorder counts from the moment its script ran. An unpaced flow
      // clicks one to two seconds after that (measured: 0.95 to 1.19 s in
      // M9, 1.65 s since the hand waits for the page to draw).
      const clicks = report.events
        .filter((event) => event.type === 'click')
        .map((event) => event.t);
      expect(clicks).toHaveLength(4);
      const [first] = clicks;
      expect(first).toBeGreaterThan(floor);
      // The hand was seen on its way there: no click out of nowhere.
      const moves = report.events.filter((event) => event.type === 'mousemove');
      expect(moves.filter((event) => event.t < (first ?? 0)).length).toBeGreaterThan(5);
      // And it did not set out the moment the page was there: while the
      // persona dwelled, the pointer rested. (Chrome itself tells a page
      // that has just loaded where a resting pointer is, with one
      // `mousemove` about 100 ms in; that one is the browser's, as it is on
      // a person's machine. An unpaced flow has a whole path there.)
      expect(moves.filter((event) => event.t < floor).length).toBeLessThanOrEqual(2);
      // What a page measures most easily is how long nothing happens after
      // a click. Without a pace that was the watch for a navigation, the
      // same 1207 to 1218 ms every time (measured); now a thinking time of
      // the persona's own follows the watch.
      const [shortest] = HUMAN_BOUNDS.thinkingMs;
      const quiet = clicks.slice(0, -1).map((at) => {
        const next = report.events.find((event) => event.type === 'mousemove' && event.t > at);
        return (next?.t ?? Number.NaN) - at;
      });
      expect(quiet).toHaveLength(3);
      for (const time of quiet) {
        expect(time).toBeGreaterThan(1200 + shortest);
      }
      expect(
        Math.max(...quiet) - Math.min(...quiet),
        `quiet for ${quiet.join(', ')}`,
      ).toBeGreaterThan(30);
      // And so from one click to the next: never a constant.
      const gaps = clicks.slice(1).map((at, index) => at - (clicks[index] ?? 0));
      expect(Math.max(...gaps) - Math.min(...gaps), `the gaps ${gaps.join(', ')}`).toBeGreaterThan(
        30,
      );
      // Each step says what of its time was pacing: the dwell after the
      // page loaded, the thinking time before every click but the first,
      // nothing for the steps that only read.
      const paced = run.steps.map((step) => step.paced_ms ?? Number.NaN);
      expect(paced[0]).toBeGreaterThanOrEqual(floor);
      expect(paced[1]).toBe(0);
      for (const thinking of paced.slice(2, 5)) {
        expect(thinking).toBeGreaterThanOrEqual(HUMAN_BOUNDS.thinkingMs[0]);
        expect(thinking).toBeLessThanOrEqual(HUMAN_BOUNDS.thinkingMs[1] + 250);
      }
      expect(paced.slice(5)).toEqual([0, 0]);
      for (const step of run.steps) {
        expect(step.duration_ms).toBeGreaterThanOrEqual(step.paced_ms ?? Number.NaN);
      }
      expect(run.final.behavior).toMatchObject({ seed: SEEDS.other, clicks: 4 });
      expect(run.final.behavior?.dwell_ms).toBeGreaterThanOrEqual(floor);
      expect(Math.abs((run.final.behavior?.dwell_ms ?? 0) - (paced[0] ?? 0))).toBeLessThan(250);

      // The same flow without a pace is what it was: no dwell, nothing reported.
      const bare = await withContext('flow-unpaced', (context) =>
        flow(context, {
          behavior: { seed: SEEDS.first },
          actions: [
            { action: 'goto', url: named('/input/') },
            { action: 'click', selector: '#target' },
            { action: 'wait_for', ms: 400 },
            { action: 'extract', capture: DOM },
          ],
        }),
      );
      expect(bare.steps.map((step) => step.paced_ms)).toEqual([null, null, null, null]);
      expect(bare.final.behavior).toMatchObject({ dwell_ms: 0, clicks: 1 });
      const unpaced = inputReportOf(bare.steps.at(-1)?.capture?.dom_html ?? null);
      const hasty = unpaced.events;
      // The hand acts on a page once it has drawn (spec §10.4), and the page
      // says when it drew its first two frames. At full speed that was about
      // 0.4 s after the recorder started; at 0.6 of a CPU 0.9 to 1.0 s, and
      // the unpaced click came at 2.4 to 2.7 s, after the floor, with the
      // persona's path (0.84 s) and rest unchanged (measured in the Chrome
      // switch audit).
      const drawnAt = unpaced.drawn?.[1] ?? Number.NaN;
      measured('a click without a pace', {
        floor_ms: floor,
        drawn: unpaced.drawn ?? null,
        events: kinds(hasty),
        moves: hasty
          .filter((event) => event.type === 'mousemove')
          .map((event) => [event.t, event.x, event.y]),
      });
      // On its way at once, and there long before the paced one has set out
      // once the page was there to see.
      expect(drawnAt).toBeGreaterThanOrEqual(0);
      const clickedAt = hasty.find((event) => event.type === 'click')?.t ?? Number.NaN;
      expect(clickedAt - drawnAt).toBeLessThan(floor);
      expect(
        hasty.filter((event) => event.type === 'mousemove' && event.t < floor).length,
      ).toBeGreaterThan(5);
    });

    it('goes back to a page the browser kept, by the driver and by the keyboard, and captures that page', async () => {
      const listing = named('/listing?page=2');
      for (const human of [false, true]) {
        const run = await withContext(`back-${human ? 'keyboard' : 'driver'}`, (context) =>
          flow(context, {
            timeout_ms: 120_000,
            actions: [
              { action: 'goto', url: listing },
              // Left for a page that came by two redirects, with a header the listing has not.
              { action: 'goto', url: named('/redirect/a') },
              { action: 'extract', capture: DOM },
              { action: 'back', human, timeout_ms: 10_000 },
              { action: 'wait_for', url: '**/listing?page=2', timeout_ms: 5000 },
              { action: 'extract', capture: DOM },
            ],
          }),
        );
        const how = human ? 'by the keyboard' : 'by the driver';
        expect(
          run.steps.filter((step) => !step.ok),
          how,
        ).toEqual([]);
        const [left, back] = run.steps
          .filter((step) => step.action === 'extract')
          .map((step) => step.capture);
        expect(left, how).toMatchObject({
          final_url: named('/landing?from=redirect'),
          status: 200,
          title: 'Landing',
        });
        expect(left?.redirects, how).toHaveLength(2);
        // Well inside its budget: a page out of the back/forward cache
        // fires no `load`, and nothing here waits for one.
        const went = run.steps.find((step) => step.action === 'back');
        expect(went?.duration_ms, how).toBeLessThan(3000);
        expect(run.steps.find((step) => step.action === 'wait_for')?.duration_ms, how).toBeLessThan(
          2000,
        );
        // The page that came back, with its own answer: not the status,
        // the headers and the redirects of the page that was left.
        expect(back, how).toMatchObject({
          url: listing,
          final_url: listing,
          status: 200,
          title: 'Listing, page 2',
          redirects: [],
          error: null,
        });
        expect(back?.dom_html, how).toContain('<h1>Listing</h1>');
        expect(
          back?.headers.map((header) => header.name),
          how,
        ).toContain('content-type');
        expect(
          back?.headers.find((header) => header.name === 'x-fixture'),
          how,
        ).toBeUndefined();
        expect(run.final.result, how).toMatchObject({ final_url: listing, status: 200 });
      }
    });

    it('goes back to a page with a status of its own, to one from before the flow, and nowhere when there is none', async () => {
      await withContext('back-more', async (context) => {
        // A page that answered 404, left for one that answered 200.
        const missing = await flow(context, {
          timeout_ms: 60_000,
          actions: [
            { action: 'goto', url: named('/status/404') },
            { action: 'goto', url: named('/landing') },
            { action: 'back', human: false, timeout_ms: 10_000 },
            { action: 'extract', capture: DOM },
          ],
        });
        expect(missing.steps.filter((step) => !step.ok)).toEqual([]);
        expect(missing.steps.at(-1)?.capture).toMatchObject({
          final_url: named('/status/404'),
          status: 404,
          title: 'Not found',
        });

        // A page a navigation loaded before the flow began is remembered
        // from the moment the flow leaves it.
        await go(context, named('/listing?page=2'));
        const earlier = await flow(context, {
          timeout_ms: 60_000,
          actions: [
            { action: 'goto', url: named('/landing') },
            { action: 'back', human: false, timeout_ms: 10_000 },
            { action: 'extract', capture: DOM },
          ],
        });
        expect(earlier.steps.filter((step) => !step.ok)).toEqual([]);
        expect(earlier.steps.at(-1)?.capture).toMatchObject({
          final_url: named('/listing?page=2'),
          status: 200,
          title: 'Listing, page 2',
          redirects: [],
        });

        // A page the flow never saw arrive: its document is captured all
        // the same. What it was answered with is known only where the
        // browser asked again; never is it the answer of the page left.
        await go(context, named('/landing'));
        const unseen = await flow(context, {
          timeout_ms: 60_000,
          actions: [
            { action: 'back', human: false, timeout_ms: 10_000 },
            { action: 'extract', capture: DOM },
          ],
        });
        expect(unseen.steps.filter((step) => !step.ok)).toEqual([]);
        const capture = unseen.steps.at(-1)?.capture;
        expect(capture).toMatchObject({
          final_url: named('/listing?page=2'),
          title: 'Listing, page 2',
          redirects: [],
          error: null,
        });
        expect(capture?.dom_html).toContain('<h1>Listing</h1>');
        expect([200, null]).toContain(capture?.status);
        expect(capture?.headers.find((header) => header.name === 'x-fixture')).toBeUndefined();
      });

      // No entry to go back to: the step says so, soon, and the flow ends,
      // by the keyboard as by the driver (F38: the keys used to report
      // success after watching five seconds of nothing).
      // (The first of the two may still find the blank page the tab began with.)
      for (const human of [false, true]) {
        const how = human ? 'by the keyboard' : 'by the driver';
        const nowhere = await withContext(
          `back-nowhere-${human ? 'keyboard' : 'driver'}`,
          (context) =>
            flow(context, {
              timeout_ms: 60_000,
              actions: [
                { action: 'goto', url: named('/landing') },
                { action: 'back', human, timeout_ms: 10_000 },
                { action: 'back', human, timeout_ms: 10_000 },
                { action: 'extract', capture: DOM },
              ],
            }),
        );
        const failed = nowhere.steps.find((step) => !step.ok);
        expect(failed, how).toMatchObject({ action: 'back', capture: null });
        expect(failed?.error, how).toMatch(/^there is no page to go back to/);
        expect(failed?.duration_ms, how).toBeLessThan(4000);
        expect(nowhere.steps.at(-1), how).toBe(failed);
        expect(nowhere.final.ok, how).toBe(false);
      }
    });

    it('goes back to an entry with the address of the page it leaves, and knows that it left', async () => {
      // A page that links to itself with a campaign tag, which the
      // document that arrives takes out of its address again: two history
      // entries, one address. The browser kept the first document, so the
      // way back changes no address and asks for nothing; only the page
      // can say that it is another document (F38: the keys used to watch
      // five seconds of that and report that the page was never left).
      for (const human of [false, true]) {
        const how = human ? 'by the keyboard' : 'by the driver';
        await site.reset();
        const run = await withContext(
          `back-same-address-${human ? 'keyboard' : 'driver'}`,
          (context) =>
            flow(context, {
              timeout_ms: 120_000,
              actions: [
                { action: 'goto', url: named('/twice') },
                { action: 'click', selector: '#tagged' },
                { action: 'wait_for', url: '**/twice', timeout_ms: 5000 },
                { action: 'assert', text_contains: 'served 2' },
                { action: 'extract', capture: DOM },
                { action: 'back', human, timeout_ms: 10_000 },
                { action: 'extract', capture: DOM },
              ],
            }),
        );
        expect(
          run.steps.filter((step) => !step.ok),
          how,
        ).toEqual([]);
        const [second, first] = run.steps
          .filter((step) => step.action === 'extract')
          .map((step) => step.capture);
        const fixture = (capture: typeof second) =>
          capture?.headers.find((header) => header.name === 'x-fixture')?.value;
        expect(second, how).toMatchObject({ final_url: named('/twice'), status: 200 });
        expect(fixture(second), how).toBe('twice-2');
        // The page that came back is the first document, as the browser
        // kept it, with the answer it came with and not the one of the
        // page that was left at the same address.
        expect(first, how).toMatchObject({
          final_url: named('/twice'),
          status: 200,
          redirects: [],
          error: null,
        });
        expect(first?.dom_html, how).toContain('<h1 id="view">served 1</h1>');
        expect(fixture(first), how).toBe('twice-1');
        expect(run.steps.find((step) => step.action === 'back')?.duration_ms, how).toBeLessThan(
          3000,
        );
        expect(run.final.result, how).toMatchObject({ final_url: named('/twice'), status: 200 });
        // Nothing went over the wire for the way back.
        expect(
          (await site.requests())
            .map((request) => request.path)
            .filter((asked) => asked.startsWith('/twice')),
          how,
        ).toEqual(['/twice', '/twice?utm_source=fixture']);
      }
    });

    it('keeps the document of a page that only changes its address', async () => {
      const run = await withContext('single-page', (context) =>
        flow(context, {
          behavior: { seed: SEEDS.first },
          timeout_ms: 120_000,
          actions: [
            { action: 'goto', url: named('/spa') },
            // The page gives itself another address and loads nothing.
            { action: 'click', selector: '#push' },
            { action: 'wait_for', url: '/spa/second?view=2', timeout_ms: 5000 },
            { action: 'extract', capture: DOM },
            // Back is the same document again, under its first address.
            { action: 'back', timeout_ms: 10_000 },
            { action: 'wait_for', url: '**/spa', timeout_ms: 5000 },
            { action: 'extract', capture: DOM },
            { action: 'click', selector: '#push' },
            { action: 'back', human: false, timeout_ms: 10_000 },
            { action: 'extract', capture: DOM },
          ],
        }),
      );
      expect(run.steps.filter((step) => !step.ok)).toEqual([]);
      const [second, first, again] = run.steps
        .filter((step) => step.action === 'extract')
        .map((step) => step.capture);
      const own = { status: 200, title: 'Single page', redirects: [], error: null };
      expect(second).toMatchObject({ ...own, final_url: named('/spa/second?view=2') });
      expect(second?.dom_html).toContain('<h1 id="view">second view</h1>');
      for (const capture of [first, again]) {
        expect(capture).toMatchObject({ ...own, final_url: named('/spa') });
        expect(capture?.dom_html).toContain('<h1 id="view">first view</h1>');
      }
      // One document all along: the answer it came with is reported every time.
      for (const capture of [second, first, again]) {
        expect(capture?.headers.find((header) => header.name === 'x-fixture')?.value).toBe('spa');
      }
      for (const step of run.steps.filter((each) => each.action === 'back')) {
        expect(step.duration_ms).toBeLessThan(3000);
      }
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
