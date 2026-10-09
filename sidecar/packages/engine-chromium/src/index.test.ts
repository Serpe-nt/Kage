import { mkdtemp, mkdir, readFile, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';

import type { OpenContextRequest } from '@kage/contract';
import {
  EngineError,
  FingerprintDocumentSchema,
  type FingerprintDocument,
} from '@kage/engine-core';
import { describe, expect, it } from 'vitest';

import {
  DisplayPool,
  FIRST_DISPLAY,
  GENERATOR_ID,
  HOST_BOUND,
  LOOPBACK_THROUGH_PROXY,
  WEBRTC_POLICY,
  allowedCpus,
  buildInjection,
  chromeBrands,
  classifyNavigationError,
  cpuListOf,
  detectChromeVersion,
  formatCpuList,
  generateFingerprint,
  launchPlan,
  linuxChromeUserAgent,
  mergePreferences,
  parseChromeVersion,
  parseCpuList,
  personaCpus,
  prepareProfile,
  profilePaths,
  startXvfb,
  suiteVersion,
} from './index.js';

const require = createRequire(import.meta.url);

function installedVersion(packageName: string): string {
  const manifest = require(`${packageName}/package.json`) as { version: string };
  return manifest.version;
}

const SEED = '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac';
const OTHER_SEED = SEED.replace(/^5/, '6');
const facts = { browserVersion: '155.0.8059.39', allowCrossOsClaims: false };
const languages = ['de-DE', 'de', 'en-US', 'en'];

describe('engine-chromium pins (fingerprint-critical, see docs/versions.md)', () => {
  it('resolves patchright and the fingerprint suite at their exact pins', () => {
    expect(installedVersion('patchright')).toBe('1.63.0');
    expect(suiteVersion('fingerprint-generator')).toBe('2.1.88');
    expect(suiteVersion('fingerprint-injector')).toBe('2.1.88');
    expect(GENERATOR_ID).toBe('fingerprint-generator@2.1.88');
  });

  it('cannot resolve the Firefox engine dependencies: nothing is hoisted', () => {
    expect(() => require.resolve('playwright-core/package.json')).toThrow();
    expect(() => require.resolve('camoufox-js/package.json')).toThrow();
  });
});

describe('host facts', () => {
  it('reads and writes CPU lists the way the kernel does', () => {
    expect(parseCpuList('0-3,8,10-11\n')).toEqual([0, 1, 2, 3, 8, 10, 11]);
    expect(parseCpuList('5')).toEqual([5]);
    expect(parseCpuList('2,1,1-2')).toEqual([1, 2]);
    expect(formatCpuList([0, 1, 2, 3, 8, 10, 11])).toBe('0-3,8,10-11');
    expect(formatCpuList([7, 5, 6, 5])).toBe('5-7');
    expect(formatCpuList([4])).toBe('4');
    expect(formatCpuList([])).toBe('');
    for (const bad of ['', 'a', '3-1', '1-', '1,,2']) {
      expect(() => parseCpuList(bad)).toThrow(TypeError);
    }
  });

  it('takes the affinity mask of the process, and falls back to a count', () => {
    expect(allowedCpus('Name:\tnode\nCpus_allowed:\tff\nCpus_allowed_list:\t2-5\n')).toEqual([
      2, 3, 4, 5,
    ]);
    expect(allowedCpus('Cpus_allowed_list:\tgarbage\n').length).toBeGreaterThan(0);
    expect(allowedCpus('nothing here').length).toBeGreaterThan(0);
    expect(allowedCpus().length).toBeGreaterThan(0);
  });

  it('reads the version Chrome prints and refuses a binary that is none', async () => {
    expect(parseChromeVersion('Google Chrome 155.0.8059.39 \n')).toBe('155.0.8059.39');
    expect(parseChromeVersion('Chromium 155.0.8059.39 built on Debian')).toBe('155.0.8059.39');
    expect(parseChromeVersion('hello')).toBeNull();
    const missing: unknown = await detectChromeVersion('/nonexistent/chrome').catch(
      (error: unknown) => error,
    );
    expect(missing).toBeInstanceOf(EngineError);
    expect((missing as EngineError).code).toBe('browser_launch_failed');
    expect((missing as EngineError).message).toContain('just setup-chrome');
    // A program that runs but prints no version is not Chrome either.
    const notChrome: unknown = await detectChromeVersion('/bin/true').catch(
      (error: unknown) => error,
    );
    expect((notChrome as EngineError).code).toBe('browser_launch_failed');
  });

  it('knows the user agent and the brand list real Chrome 155 sends', () => {
    // Recorded from Chrome 155.0.8059.39 against the fingerprint echo
    // (tests/fixtures/fingerprints/browser/chromium-155.json).
    expect(linuxChromeUserAgent(155)).toBe(
      'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36',
    );
    expect(chromeBrands(155)).toEqual([
      { brand: 'Google Chrome', version: '155' },
      { brand: 'Chromium', version: '155' },
      { brand: 'Not(A:Brand', version: '24' },
    ]);
    // What fingerprint-generator's own data shows for Chrome 147.
    expect(chromeBrands(147)).toEqual([
      { brand: 'Google Chrome', version: '147' },
      { brand: 'Not.A/Brand', version: '8' },
      { brand: 'Chromium', version: '147' },
    ]);
  });
});

describe('generateFingerprint', () => {
  const request = { seed: SEED, claimed_os: 'linux', languages } as const;

  it('gives the same document for the same seed, byte for byte', () => {
    const first = generateFingerprint(request, facts);
    const second = generateFingerprint(request, facts);
    expect(JSON.stringify(second)).toBe(JSON.stringify(first));
    // Another persona in between does not disturb the stream.
    generateFingerprint({ ...request, seed: OTHER_SEED }, facts);
    expect(JSON.stringify(generateFingerprint(request, facts))).toBe(JSON.stringify(first));
  });

  it('gives another document for another seed', () => {
    const [a, b] = [
      generateFingerprint(request, facts),
      generateFingerprint({ ...request, seed: OTHER_SEED }, facts),
    ];
    expect(JSON.stringify(a.generated)).not.toBe(JSON.stringify(b.generated));
  });

  it('pins what one seed gives with this generator: a bump that changes it is seen here', () => {
    const document = generateFingerprint(request, facts);
    expect({
      screen: document.native.screen,
      hardware_concurrency: document.native.hardware_concurrency,
    }).toMatchInlineSnapshot(`
      {
        "hardware_concurrency": 12,
        "screen": {
          "color_depth": 24,
          "dpr": 1,
          "height": 1200,
          "width": 1800,
        },
      }
    `);
  });

  it("describes an honest Linux persona with the installed browser, not the generator's", () => {
    const document = generateFingerprint(request, facts);
    expect(FingerprintDocumentSchema.parse(document)).toEqual(document);
    expect(document).toMatchObject({
      schema: 1,
      engine: 'chromium',
      generator: GENERATOR_ID,
      seed: SEED,
      claimed_os: 'linux',
      browser_version: '155.0.8059.39',
      user_agent: linuxChromeUserAgent(155),
      platform: 'Linux x86_64',
      languages,
      host_bound: [...HOST_BOUND],
      inject: 'none',
    });
    expect(document.native.screen).toMatchObject({ dpr: 1, color_depth: 24 });
    expect(document.native.screen.width).toBeGreaterThanOrEqual(1280);
    expect(document.native.screen.width).toBeLessThanOrEqual(2560);
    expect(document.native.hardware_concurrency).toBeGreaterThanOrEqual(1);
    // The generator has no Chrome 155 and says so in what it generated.
    const generated = document.generated as { fingerprint: { navigator: { userAgent: string } } };
    expect(generated.fingerprint.navigator.userAgent).toContain('X11; Linux');
    expect(generated.fingerprint.navigator.userAgent).not.toContain('Chrome/155');
  });

  it('honors screen bounds and says when nothing fits them', () => {
    const narrow = generateFingerprint(
      {
        ...request,
        screen: { min_width: 1920, max_width: 1920, min_height: 1080, max_height: 1080 },
      },
      facts,
    );
    expect(narrow.native.screen).toEqual({ width: 1920, height: 1080, dpr: 1, color_depth: 24 });
    const error = (() => {
      try {
        generateFingerprint({ ...request, screen: { min_width: 641, max_width: 642 } }, facts);
        return null;
      } catch (thrown) {
        return thrown as EngineError;
      }
    })();
    expect(error?.code).toBe('invalid_request');
    expect(error?.message).toContain('no fingerprint satisfies the constraints');
  });

  it('refuses another operating system unless the engine allows such claims', () => {
    for (const os of ['windows', 'macos'] as const) {
      const error = (() => {
        try {
          generateFingerprint({ ...request, claimed_os: os }, facts);
          return null;
        } catch (thrown) {
          return thrown as EngineError;
        }
      })();
      expect(error?.code).toBe('cross_os_claim_disabled');
      expect(error?.message).toContain('ADR 0009');
    }
  });

  it('refuses a browser major the engine does not bundle', () => {
    expect(generateFingerprint({ ...request, browser_major: 155 }, facts).browser_version).toBe(
      '155.0.8059.39',
    );
    expect(() => generateFingerprint({ ...request, browser_major: 156 }, facts)).toThrow(
      expect.objectContaining({ code: 'browser_major_mismatch' }) as Error,
    );
    expect(() => generateFingerprint(request, { ...facts, browserVersion: 'unknown' })).toThrow(
      expect.objectContaining({ code: 'internal' }) as Error,
    );
  });

  it('writes a Windows claim for the installed major when claims are allowed', () => {
    const document = generateFingerprint(
      { ...request, claimed_os: 'windows' },
      { ...facts, allowCrossOsClaims: true },
    );
    expect(document).toMatchObject({ claimed_os: 'windows', platform: 'Win32', inject: 'window' });
    expect(document.user_agent).toMatch(
      /^Mozilla\/5\.0 \(Windows NT 10\.0; Win64; x64\) .* Chrome\/155\.0\.0\.0 Safari\/537\.36$/,
    );
  });
});

describe('buildInjection', () => {
  const windows = () =>
    generateFingerprint(
      { seed: SEED, claimed_os: 'windows', languages },
      { ...facts, allowCrossOsClaims: true },
    );

  it('builds the same script from the same document, with no fresh randomness', () => {
    const [a, b] = [buildInjection(windows()), buildInjection(windows())];
    expect(a.sha256).toBe(b.sha256);
    expect(a.script).toBe(b.script);
    expect(a.sha256).toMatch(/^[0-9a-f]{64}$/);
    expect(
      buildInjection(
        generateFingerprint(
          { seed: OTHER_SEED, claimed_os: 'windows', languages },
          { ...facts, allowCrossOsClaims: true },
        ),
      ).sha256,
    ).not.toBe(a.sha256);
  });

  it('never fakes history.length and names the installed browser everywhere', () => {
    const { script, userAgentOverride } = buildInjection(windows());
    expect(script).not.toContain('window.history');
    expect(script).not.toContain('"historyLength"');
    expect(script).toContain('Chrome/155.0.0.0');
    expect(script).not.toMatch(/Chrome\/14\d\.0\.0\.0/);
    expect(userAgentOverride).toMatchObject({
      platform: 'Win32',
      userAgentMetadata: {
        brands: chromeBrands(155),
        platform: 'Windows',
        mobile: false,
        model: '',
        wow64: false,
      },
    });
    expect(userAgentOverride.userAgent).toContain('Windows NT 10.0');
    expect(userAgentOverride.userAgentMetadata.fullVersionList).toEqual([
      { brand: 'Google Chrome', version: '155.0.8059.39' },
      { brand: 'Chromium', version: '155.0.8059.39' },
      { brand: 'Not(A:Brand', version: '24.0.0.0' },
    ]);
  });

  it('is a script that parses', () => {
    // eslint-disable-next-line @typescript-eslint/no-implied-eval -- only parsed, never run
    expect(() => new Function(buildInjection(windows()).script)).not.toThrow();
  });
});

describe('launchPlan', () => {
  const document: FingerprintDocument = generateFingerprint(
    {
      seed: SEED,
      claimed_os: 'linux',
      languages,
      screen: { min_width: 1920, max_width: 1920, min_height: 1080, max_height: 1080 },
    },
    facts,
  );
  const request: OpenContextRequest = {
    persona_id: '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e',
    fingerprint: document,
    proxy: { server: 'http://203.0.113.10:6540', username: 'u', password: 'p' },
    locale: 'de-DE',
    languages,
    timezone: 'Europe/Berlin',
    geolocation: { latitude: 52.52, longitude: 13.405, accuracy_m: 35 },
    screen: document.native.screen,
    profile_dir: 'personas/p1',
  };
  const host = { cpus: [0, 1, 2, 3, 4, 5, 6, 7], gl: 'llvmpipe' as const };

  it('is a pure function of the document, the request and the host', () => {
    expect(launchPlan(document, request, host)).toEqual(
      launchPlan(structuredClone(document), structuredClone(request), host),
    );
  });

  it('makes the persona with launch parameters only', () => {
    const plan = launchPlan(document, request, host);
    expect(plan).toMatchObject({
      display: { width: 1920, height: 1080, depth: 24 },
      window: { width: 1920, height: 1080 },
      env: { TZ: 'Europe/Berlin', LANG: 'de_DE.UTF-8', LANGUAGE: 'de_DE' },
      preferences: {
        intl: { accept_languages: 'de-DE,de,en-US,en', selected_languages: 'de-DE,de,en-US,en' },
        webrtc: { ip_handling_policy: WEBRTC_POLICY },
      },
      proxy: {
        server: 'http://203.0.113.10:6540',
        username: 'u',
        password: 'p',
        bypass: LOOPBACK_THROUGH_PROXY,
      },
      geolocation: { latitude: 52.52, longitude: 13.405, accuracy: 35 },
      gl: 'llvmpipe',
      inject: 'none',
    });
    expect(plan.args).toEqual([
      '--window-position=0,0',
      '--window-size=1920,1080',
      '--lang=de-DE',
      '--use-gl=angle',
      '--use-angle=gl',
      '--ignore-gpu-blocklist',
    ]);
    expect(LOOPBACK_THROUGH_PROXY).toBe('<-loopback>');
    expect(WEBRTC_POLICY).toBe('disable_non_proxied_udp');
  });

  it('never passes what patchright warns against or what would weaken the browser', () => {
    for (const gl of ['none', 'swiftshader', 'llvmpipe', 'host'] as const) {
      const { args } = launchPlan(document, request, { ...host, gl });
      for (const forbidden of [
        '--no-sandbox',
        '--user-agent',
        '--headless',
        '--disable-web-security',
        '--remote-debugging-port',
        '--disable-blink-features',
        '--enable-automation',
        '--proxy-server',
      ]) {
        expect(args.some((argument) => argument.startsWith(forbidden))).toBe(false);
      }
    }
    expect(launchPlan(document, request, { ...host, gl: 'none' }).args).toHaveLength(3);
    expect(launchPlan(document, request, { ...host, gl: 'swiftshader' }).args).toContain(
      '--enable-unsafe-swiftshader',
    );
    expect(launchPlan(document, request, { ...host, gl: 'host' }).args).toHaveLength(3);
  });

  it("gives the persona as many CPUs as its fingerprint says, at most the host's", () => {
    const wanted = document.native.hardware_concurrency;
    const plan = launchPlan(document, request, host);
    expect(plan.cpus).toHaveLength(Math.min(wanted, 8));
    expect(new Set(plan.cpus).size).toBe(plan.cpus.length);
    expect(cpuListOf(plan)).toBe(formatCpuList(plan.cpus));
    expect(launchPlan(document, request, { ...host, cpus: [3] }).cpus).toEqual([3]);
    expect(() => launchPlan(document, request, { ...host, cpus: [] })).toThrow(
      expect.objectContaining({ code: 'internal' }) as Error,
    );
  });

  it("spreads personas over the host's CPUs by seed, the same way every time", () => {
    const cpus = Array.from({ length: 16 }, (_unused, index) => index);
    expect(personaCpus(SEED, 4, cpus)).toEqual(personaCpus(SEED, 4, cpus));
    const starts = new Set(
      Array.from({ length: 40 }, (_unused, index) =>
        personaCpus(SEED.replace(/^../, index.toString(16).padStart(2, '0')), 4, cpus).join(','),
      ),
    );
    expect(starts.size).toBeGreaterThan(4);
    expect(personaCpus(SEED, 99, cpus)).toEqual(cpus);
    expect(personaCpus(SEED, 0, cpus)).toHaveLength(1);
    // A window that wraps around the end is still four distinct CPUs.
    for (let index = 0; index < 40; index += 1) {
      const chosen = personaCpus(
        SEED.replace(/^../, index.toString(16).padStart(2, '0')),
        4,
        [0, 1, 2, 3, 4],
      );
      expect(new Set(chosen).size).toBe(4);
    }
  });

  it("refuses a screen that is not the fingerprint's", () => {
    for (const change of [{ width: 1366 }, { height: 768 }, { dpr: 2 }, { color_depth: 30 }]) {
      const error = (() => {
        try {
          launchPlan(document, { ...request, screen: { ...request.screen, ...change } }, host);
          return null;
        } catch (thrown) {
          return thrown as EngineError;
        }
      })();
      expect(error?.code).toBe('invalid_request');
      expect(error?.message).toContain("is not the fingerprint's");
    }
  });

  it('reports no geolocation when the persona has none, and scales a dense display', () => {
    expect(launchPlan(document, { ...request, geolocation: null }, host).geolocation).toBeNull();
    const dense = {
      ...document,
      native: { ...document.native, screen: { ...document.native.screen, dpr: 2 } },
    };
    const plan = launchPlan(dense, { ...request, screen: dense.native.screen }, host);
    expect(plan.display).toEqual({ width: 3840, height: 2160, depth: 24 });
    expect(plan.args).toContain('--force-device-scale-factor=2');
    expect(plan.window).toEqual({ width: 1920, height: 1080 });
  });
});

describe('profile preferences', () => {
  const preferences = {
    intl: { accept_languages: 'de-DE,de', selected_languages: 'de-DE,de' },
    webrtc: { ip_handling_policy: WEBRTC_POLICY },
  };

  it("merges only Kage's keys and keeps what the profile accrued", () => {
    const existing = {
      intl: { accept_languages: 'en-US', charset_default: 'utf-8' },
      profile: { name: 'Person 1', content_settings: { exceptions: { cookies: {} } } },
      webrtc: 'not an object',
    };
    expect(mergePreferences(existing, preferences)).toEqual({
      intl: {
        accept_languages: 'de-DE,de',
        selected_languages: 'de-DE,de',
        charset_default: 'utf-8',
      },
      profile: { name: 'Person 1', content_settings: { exceptions: { cookies: {} } } },
      webrtc: { ip_handling_policy: WEBRTC_POLICY },
    });
    expect(mergePreferences(null, { a: 1 })).toEqual({ a: 1 });
    expect(mergePreferences([1], { a: { b: 2 } })).toEqual({ a: { b: 2 } });
  });

  it('creates a new profile with its directories and its preferences', async () => {
    const root = await mkdtemp(path.join(tmpdir(), 'kage-profile-'));
    const paths = await prepareProfile(path.join(root, 'personas', 'p1'), preferences);
    expect(paths).toEqual(profilePaths(path.join(root, 'personas', 'p1')));
    expect(paths.preferences.endsWith(path.join('user-data', 'Default', 'Preferences'))).toBe(true);
    expect(JSON.parse(await readFile(paths.preferences, 'utf8'))).toEqual(preferences);
  });

  it("keeps an existing profile's state and survives a preferences file that is not JSON", async () => {
    const root = await mkdtemp(path.join(tmpdir(), 'kage-profile-'));
    const paths = profilePaths(root);
    await mkdir(path.dirname(paths.preferences), { recursive: true });
    await writeFile(
      paths.preferences,
      JSON.stringify({ browser: { has_seen_welcome_page: true } }),
    );
    await prepareProfile(root, preferences);
    expect(JSON.parse(await readFile(paths.preferences, 'utf8'))).toEqual({
      browser: { has_seen_welcome_page: true },
      ...preferences,
    });
    await writeFile(paths.preferences, '{ not json');
    await prepareProfile(root, preferences);
    expect(JSON.parse(await readFile(paths.preferences, 'utf8'))).toEqual(preferences);
  });
});

describe('displays', () => {
  it('hands out free display numbers and takes them back', () => {
    const busy = new Set([FIRST_DISPLAY + 1]);
    const pool = new DisplayPool((display) => !busy.has(display));
    expect(pool.acquire()).toBe(FIRST_DISPLAY);
    // The next number is in use on the machine and is skipped.
    expect(pool.acquire()).toBe(FIRST_DISPLAY + 2);
    pool.release(FIRST_DISPLAY);
    expect(pool.acquire()).toBe(FIRST_DISPLAY);
  });

  it('gives up when no display number is free', () => {
    const pool = new DisplayPool(() => false);
    expect(() => pool.acquire()).toThrow(
      expect.objectContaining({ code: 'browser_launch_failed' }) as Error,
    );
    expect(new DisplayPool().acquire()).toBeGreaterThanOrEqual(FIRST_DISPLAY);
  });

  it('starts an X server with the asked geometry and stops it', async () => {
    const pool = new DisplayPool();
    const number = pool.acquire();
    const display = await startXvfb(number, { width: 1366, height: 768, depth: 24 });
    expect(display.name).toBe(`:${number}`);
    await display.stop();
    await display.stop();
    pool.release(number);
  });

  it('reports an X server that cannot start', async () => {
    const number = new DisplayPool().acquire();
    const error: unknown = await startXvfb(number, { width: 0, height: 0, depth: 7 }).catch(
      (thrown: unknown) => thrown,
    );
    expect(error).toBeInstanceOf(EngineError);
    expect((error as EngineError).code).toBe('browser_launch_failed');
    expect((error as EngineError).message).toContain('Xvfb');
  });
});

describe('classifyNavigationError', () => {
  const timeout = Object.assign(
    new Error('page.goto: Timeout 1000ms exceeded.\nCall log:\n  - navigating'),
    {
      name: 'TimeoutError',
    },
  );

  it.each([
    [timeout, { code: 'timeout', message: 'page.goto: Timeout 1000ms exceeded.', net_error: null }],
    [
      new Error('page.goto: net::ERR_PROXY_CONNECTION_FAILED at https://x.test/'),
      { code: 'proxy_failed', net_error: 'ERR_PROXY_CONNECTION_FAILED' },
    ],
    [
      new Error('page.goto: net::ERR_TUNNEL_CONNECTION_FAILED at https://x.test/'),
      { code: 'proxy_failed', net_error: 'ERR_TUNNEL_CONNECTION_FAILED' },
    ],
    [
      new Error('page.goto: net::ERR_NO_SUPPORTED_PROXIES at https://x.test/'),
      { code: 'proxy_failed', net_error: 'ERR_NO_SUPPORTED_PROXIES' },
    ],
    [
      new Error('page.goto: net::ERR_NAME_NOT_RESOLVED at https://x.test/'),
      { code: 'navigation_failed', net_error: 'ERR_NAME_NOT_RESOLVED' },
    ],
    [
      new Error('page.goto: net::ERR_ABORTED at https://x.test/file'),
      { code: 'aborted', net_error: 'ERR_ABORTED' },
    ],
    [new Error('page.goto: Download is starting'), { code: 'aborted', net_error: null }],
    [new Error('page.goto: Target closed'), { code: 'aborted', net_error: null }],
    [new Error('something else'), { code: 'navigation_failed', net_error: null }],
    ['a thrown string', { code: 'navigation_failed', message: 'a thrown string', net_error: null }],
  ])('classifies %s', (error, expected) => {
    expect(classifyNavigationError(error)).toMatchObject(expected);
  });
});
