import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';

import type { ClaimedOs, OpenContextRequest } from '@kage/contract';
import {
  EngineError,
  FingerprintDocumentSchema,
  silentLog,
  type FingerprintDocument,
} from '@kage/engine-core';
import { beforeAll, describe, expect, it } from 'vitest';

import {
  CAMOUFOX_RELEASE,
  CAMOUFOX_VERSION,
  FirefoxEngine,
  basePreferences,
  classifyNavigationError,
  configEnvironment,
  cpuListOf,
  detectHost,
  generateFingerprint,
  generatedOf,
  generatorId,
  launchPlan,
  launcherVersion,
  localeConfig,
  parseProperties,
  personaFontconfig,
  profilePaths,
  validateConfig,
  type CamoufoxConfig,
  type HostFacts,
} from './index.js';

const require = createRequire(import.meta.url);

function installedVersion(packageName: string): string {
  const manifest = require(`${packageName}/package.json`) as { version: string };
  return manifest.version;
}

const SEED = '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac';
const OTHER_SEED = '0b6d1f4e8a2c5397b1d3f5a7c9e0b2d4f6a8c0e2b4d6f8a1c3e5a7c9e1b3d5f7';
const LANGUAGES = ['de-DE', 'de'];
const SYSTEMS: ClaimedOs[] = ['windows', 'macos', 'linux'];

let host: HostFacts;
const generate = (os: ClaimedOs, seed = SEED) =>
  generateFingerprint({ seed, claimed_os: os, languages: LANGUAGES }, host);
const request = (document: FingerprintDocument, change: Partial<OpenContextRequest> = {}) =>
  ({
    persona_id: '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e',
    fingerprint: document,
    proxy: { server: 'http://203.0.113.10:6540', username: 'u', password: 'p' },
    locale: 'de-DE',
    languages: ['de-DE', 'de', 'en'],
    timezone: 'Europe/Berlin',
    geolocation: { latitude: 52.52, longitude: 13.405, accuracy_m: 35 },
    screen: document.native.screen,
    profile_dir: 'personas/p1',
    ...change,
  }) satisfies OpenContextRequest;

/** Whether the process has loaded ua-parser-js, by whatever way. */
const uaParserLoaded = () =>
  Object.keys(require.cache).some((file) => file.includes('ua-parser-js'));

beforeAll(async () => {
  host = await detectHost();
});

describe('engine-firefox pins (fingerprint-critical, see docs/versions.md)', () => {
  it('resolves camoufox-js at its exact pin', () => {
    expect(installedVersion('camoufox-js')).toBe('0.12.1');
    expect(launcherVersion()).toBe('0.12.1');
  });

  it('resolves a playwright-core below 1.63.0, as camoufox-js requires', () => {
    const version = installedVersion('playwright-core');
    const [major, minor] = version.split('.').map(Number);

    expect(major).toBe(1);
    expect(minor).toBeLessThan(63);
    expect(version).toBe('1.62.1');
  });

  it('cannot resolve the Chromium engine dependencies: nothing is hoisted', () => {
    expect(() => require.resolve('patchright/package.json')).toThrow();
    expect(() => require.resolve('patchright-core/package.json')).toThrow();
  });

  it('runs the pinned Camoufox build and no other', async () => {
    expect([host.version, host.release]).toEqual([CAMOUFOX_VERSION, CAMOUFOX_RELEASE]);
    expect([CAMOUFOX_VERSION, CAMOUFOX_RELEASE]).toEqual(['152.0.4', 'beta.30']);
    expect(host.properties.get('navigator.userAgent')).toBe('str');
    expect(host.properties.size).toBeGreaterThan(100);

    const empty = await mkdtemp(path.join(tmpdir(), 'kage-nocamoufox-'));
    await expect(detectHost(empty)).rejects.toMatchObject({ code: 'browser_launch_failed' });
    // Another build, even a newer one of the same Firefox, is refused: the
    // launcher's own download installs exactly such a build.
    await writeFile(
      path.join(empty, 'version.json'),
      JSON.stringify({ version: '152.0.4', release: 'beta.31' }),
    );
    const refused: unknown = await detectHost(empty).catch((error: unknown) => error);
    expect(refused).toBeInstanceOf(EngineError);
    expect((refused as EngineError).message).toContain('152.0.4-beta.31');
    await writeFile(path.join(empty, 'version.json'), 'not json');
    await expect(detectHost(empty)).rejects.toMatchObject({ code: 'browser_launch_failed' });
  });
});

describe('ua-parser-js (ADR 0008)', () => {
  // Order matters in this block: the first test must run before anything
  // has imported the launcher's entry point.
  it('is never loaded by anything the engine does, for any claimed system', () => {
    for (const os of SYSTEMS) {
      const document = generate(os);
      const plan = launchPlan(document, request(document), host, '/tmp/page.pdf');
      expect(plan.config['navigator.userAgent']).toBe(document.user_agent);
    }
    expect(uaParserLoaded()).toBe(false);
  });

  it('is the MIT line, which the launcher reads a macOS User-Agent wrong with', async () => {
    // The launcher's entry point is what loads the package. Kage never
    // imports it; this test does, to pin what would go wrong if it did.
    await import('camoufox-js');
    expect(uaParserLoaded()).toBe(true);
    const fromLauncher = createRequire(require.resolve('camoufox-js'));
    const manifest = fromLauncher('ua-parser-js/package.json') as {
      version: string;
      license: string;
    };
    expect(manifest.version).toBe('1.0.41');
    expect(manifest.license).toBe('MIT');
    const { UAParser } = fromLauncher('ua-parser-js') as {
      UAParser: new (userAgent: string) => { getOS(): { name?: string } };
    };
    // `launchOptions` asks whether this name starts with "macOS". With the
    // 1.x line it does not, so the launcher would take a macOS persona for
    // a Linux one. The engine names the claimed system itself.
    expect(new UAParser(generate('macos').user_agent).getOS().name).toBe('Mac OS');
    expect(new UAParser(generate('windows').user_agent).getOS().name).toBe('Windows');
  });
});

describe('generateFingerprint', () => {
  it('gives the same document for the same seed, byte for byte, and another for another', () => {
    const first = generate('windows');
    expect(JSON.stringify(generate('windows'))).toBe(JSON.stringify(first));
    expect(FingerprintDocumentSchema.parse(first)).toEqual(first);
    const other = generate('windows', OTHER_SEED);
    expect(JSON.stringify(other.generated)).not.toBe(JSON.stringify(first.generated));
  });

  it('draws a pinned seed to pinned values, so that a bump of the launcher or its data is seen', () => {
    const document = generate('windows');
    expect(document).toMatchObject({
      schema: 1,
      engine: 'firefox',
      generator: 'camoufox-js@0.12.1+camoufox-152.0.4-beta.30',
      seed: SEED,
      claimed_os: 'windows',
      browser_version: '152.0.4',
      user_agent:
        'Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:152.0) Gecko/20100101 Firefox/152.0',
      platform: 'Win32',
      languages: LANGUAGES,
      native: {
        screen: { width: 1600, height: 900, dpr: 1, color_depth: 24 },
        hardware_concurrency: 16,
      },
      host_bound: ['webgl_rendering', 'text_rendering'],
      inject: 'none',
    });
    expect(generatorId(host)).toBe(document.generator);
    const { config, webgl2 } = generatedOf(document);
    expect(config).toMatchObject({
      'navigator.oscpu': 'Windows NT 10.0; Win64; x64',
      'webGl:renderer': 'ANGLE (NVIDIA, NVIDIA GeForce 8800 GTX Direct3D11 vs_4_0 ps_4_0)',
      'fonts:spacing_seed': 3_482_387_260,
      'audio:seed': 3_024_434_807,
      'canvas:seed': 104_999_401,
      'canvas:aaOffset': 6,
      'canvas:aaCapOffset': true,
      'window.outerWidth': 1616,
      'window.outerHeight': 916,
      'mediaDevices:enabled': true,
      'mediaDevices:micros': 1,
      'mediaDevices:webcams': 1,
      'AudioContext:sampleRate': 48_000,
      'AudioContext:maxChannelCount': 2,
    });
    expect(webgl2).toBe(false);
  });

  it('describes each system it may claim in every value that names one', () => {
    const marks: Record<ClaimedOs, [RegExp, string, RegExp, string]> = {
      windows: [/\(Windows NT 10\.0; Win64; x64; rv:152\.0\)/, 'Win32', /Direct3D/, 'Segoe UI'],
      macos: [
        /\(Macintosh; Intel Mac OS X [\d.]+; rv:152\.0\)/,
        'MacIntel',
        /Apple|Intel|AMD/,
        'Helvetica Neue',
      ],
      linux: [/\(X11; Linux x86_64; rv:152\.0\)/, 'Linux x86_64', /.+/, 'Arimo'],
    };
    for (const os of SYSTEMS) {
      const document = generate(os);
      const { config } = generatedOf(document);
      const [userAgent, platform, renderer, font] = marks[os];
      expect(document.user_agent).toMatch(userAgent);
      expect(document.platform).toBe(platform);
      expect(config['webGl:renderer']).toMatch(renderer);
      expect(config['fonts']).toContain(font);
      expect(config['navigator.hardwareConcurrency']).toBe(document.native.hardware_concurrency);
      expect(config['screen.width']).toBe(document.native.screen.width);
      // What an open request supplies, and what must stay the real window's, is not in the document.
      for (const key of [
        'timezone',
        'locale:language',
        'geolocation:latitude',
        'window.innerWidth',
        'window.history.length',
        'humanize',
      ]) {
        expect(config, key).not.toHaveProperty([key]);
      }
      expect(() => {
        validateConfig(config, host.properties);
      }).not.toThrow();
    }
  });

  it('keeps the screen inside the bounds, and says so when nothing fits or the major is another', () => {
    const bounded = generateFingerprint(
      {
        seed: SEED,
        claimed_os: 'windows',
        languages: LANGUAGES,
        screen: { min_width: 1900, max_width: 1920, min_height: 1000, max_height: 1100 },
      },
      host,
    );
    expect(bounded.native.screen).toMatchObject({ width: 1920, height: 1080 });
    for (const seed of [SEED, OTHER_SEED, 'a'.repeat(64), '7'.repeat(64)]) {
      const { screen } = generate('macos', seed).native;
      expect(screen.width).toBeGreaterThanOrEqual(1280);
      expect(screen.width).toBeLessThanOrEqual(1920);
      expect(screen.height).toBeLessThanOrEqual(1200);
    }
    expect(() =>
      generateFingerprint(
        {
          seed: SEED,
          claimed_os: 'windows',
          languages: LANGUAGES,
          screen: { min_width: 7000, max_width: 7001 },
        },
        host,
      ),
    ).toThrow(expect.objectContaining({ code: 'invalid_request' }) as Error);
    expect(() =>
      generateFingerprint(
        { seed: SEED, claimed_os: 'windows', languages: LANGUAGES, browser_major: 151 },
        host,
      ),
    ).toThrow(expect.objectContaining({ code: 'browser_major_mismatch' }) as Error);
    expect(() =>
      generateFingerprint(
        { seed: SEED, claimed_os: 'windows', languages: LANGUAGES },
        { ...host, version: 'x' },
      ),
    ).toThrow(expect.objectContaining({ code: 'internal' }) as Error);
  });

  it('refuses a configuration the build would reject, and a document that is not its own', () => {
    expect(() => {
      validateConfig({ 'no.such.key': 1 }, host.properties);
    }).toThrow(/does not know the key no\.such\.key/);
    expect(() => {
      validateConfig({ 'screen.width': '1920' }, host.properties);
    }).toThrow(/screen\.width must be uint/);
    for (const [key, value] of [
      ['navigator.userAgent', 1],
      ['window.screenX', 1.5],
      ['screen.width', -1],
      ['battery:level', 'full'],
      ['pdfViewerEnabled', 1],
      ['fonts', 'Arial'],
      ['webGl:parameters', []],
    ] as const) {
      expect(() => {
        validateConfig({ [key]: value } as CamoufoxConfig, host.properties);
      }, key).toThrow(EngineError);
    }
    expect(parseProperties('[{"property":"a","type":"str"},{"other":1}]')).toEqual(
      new Map([['a', 'str']]),
    );
    expect(() => parseProperties('{}')).toThrow(TypeError);
    expect(() => generatedOf({ ...generate('windows'), generated: { fingerprint: {} } })).toThrow(
      expect.objectContaining({ code: 'invalid_request' }) as Error,
    );
  });
});

describe('launchPlan', () => {
  it('is a pure function of the document, the request and the host', () => {
    const document = generate('windows');
    expect(launchPlan(document, request(document), host, '/p/page.pdf')).toEqual(
      launchPlan(
        structuredClone(document),
        structuredClone(request(document)),
        host,
        '/p/page.pdf',
      ),
    );
  });

  it('adds what Kage says about language, time and place, and nothing it looked up', () => {
    const document = generate('windows');
    const plan = launchPlan(document, request(document), host, '/p/page.pdf');
    expect(plan.config).toMatchObject({
      ...generatedOf(document).config,
      'locale:language': 'de',
      'locale:region': 'DE',
      'locale:all': 'de-DE, de, en',
      timezone: 'Europe/Berlin',
      'geolocation:latitude': 52.52,
      'geolocation:longitude': 13.405,
      'geolocation:accuracy': 35,
    });
    expect(plan.layout).toBe('de');
    // The display holds the screen and a maximized window that is larger than it.
    expect(plan.display).toEqual({ width: 1616, height: 916, depth: 24 });
    expect(plan.proxy).toEqual({
      server: 'http://203.0.113.10:6540',
      username: 'u',
      password: 'p',
    });
    expect(plan.cpus.length).toBe(Math.min(16, host.cpus.length));
    expect(cpuListOf(plan)).toMatch(/^\d+([-,]\d+)*$/);
    // The configuration reaches the browser whole, in chunks it can read.
    const joined = Object.entries(plan.configEnv)
      .sort(([a], [b]) => Number(a.split('_').at(-1)) - Number(b.split('_').at(-1)))
      .map(([, value]) => value)
      .join('');
    expect(JSON.parse(joined)).toEqual(plan.config);
    expect(Object.keys(configEnvironment({ fonts: ['x'.repeat(70_000)] }))).toEqual([
      'CAMOU_CONFIG_1',
      'CAMOU_CONFIG_2',
      'CAMOU_CONFIG_3',
    ]);

    const nowhere = launchPlan(
      document,
      request(document, { geolocation: null, locale: 'sr-Latn-RS', languages: ['sr-Latn-RS'] }),
      host,
      '/p/page.pdf',
    );
    expect(nowhere.config).not.toHaveProperty(['geolocation:latitude']);
    expect(nowhere.config).toMatchObject({
      'locale:language': 'sr',
      'locale:region': 'RS',
      'locale:script': 'Latn',
    });
    expect(localeConfig(request(document, { locale: 'en-GB' }))).toMatchObject({
      'locale:region': 'GB',
    });
  });

  it('sends loopback through the proxy, keeps WebRTC inside it, and prints to a file', () => {
    const document = generate('macos');
    const plan = launchPlan(document, request(document), host, '/home/p/print/page.pdf');
    expect(plan.preferences).toMatchObject({
      'network.proxy.allow_hijacking_localhost': true,
      'media.peerconnection.ice.proxy_only_if_behind_proxy': true,
      'network.http.http3.enable': false,
      'print.always_print_silent': true,
      'print.printer_Mozilla_Save_to_PDF.print_to_filename': '/home/p/print/page.pdf',
      'webgl.enable-webgl2': true,
    });
    expect(basePreferences('/x.pdf')['print_printer']).toBe('Mozilla Save to PDF');
    // Nothing of the launcher's own input simulation is switched on.
    expect(plan.config).not.toHaveProperty(['humanize']);
  });

  it('gives each claimed system its own fonts by an absolute path, unhinted except for Linux', () => {
    for (const os of SYSTEMS) {
      const document = generate(os);
      const { fontconfig } = launchPlan(document, request(document), host, '/p/page.pdf');
      expect(fontconfig).toContain(`<dir>${path.join(host.installDir, 'fonts')}</dir>`);
      expect(fontconfig).not.toContain('prefix="cwd"');
      expect(fontconfig).toContain(
        os === 'linux' ? '<const>hintfull</const>' : '<const>hintnone</const>',
      );
      expect(
        fontconfig.includes('<edit name="hinting" mode="assign"><bool>false</bool></edit>'),
      ).toBe(os !== 'linux');
    }
    // The generic families are the claimed system's own.
    expect(personaFontconfig('windows', host.fontconfig.windows, '/opt/camoufox')).toMatch(
      /<string>monospace<\/string>\s*<\/test>\s*<edit[^>]*>\s*<string>Consolas<\/string>/,
    );
    // A build whose file changed must be looked at, not launched.
    expect(() => personaFontconfig('windows', '<fontconfig/>', '/opt/camoufox')).toThrow(/changed/);
    expect(() =>
      personaFontconfig('macos', '<dir prefix="cwd">fonts</dir>', '/opt/camoufox'),
    ).toThrow(/hinting/);
  });

  it('refuses another screen than the fingerprint has, a locale without a region, a host without CPUs', () => {
    const document = generate('windows');
    const plan =
      (change: Partial<OpenContextRequest>, facts: HostFacts = host) =>
      () =>
        launchPlan(document, request(document, change), facts, '/p/page.pdf');
    expect(plan({ screen: { ...document.native.screen, height: 901 } })).toThrow(
      expect.objectContaining({ code: 'invalid_request' }) as Error,
    );
    expect(plan({ locale: 'de' })).toThrow(/needs a locale with a region/);
    expect(plan({}, { ...host, cpus: [] })).toThrow(
      expect.objectContaining({ code: 'internal' }) as Error,
    );
  });
});

describe('the engine before it launches anything', () => {
  it('names the directories of a profile', () => {
    expect(profilePaths('/var/lib/kage/personas/p1')).toEqual({
      userData: '/var/lib/kage/personas/p1/user-data',
      home: '/var/lib/kage/personas/p1/home',
      fontconfig: '/var/lib/kage/personas/p1/home/fontconfig',
      pdf: '/var/lib/kage/personas/p1/home/print/page.pdf',
    });
  });

  it('generates through the engine, and refuses to open what is not its own', async () => {
    const root = await mkdtemp(path.join(tmpdir(), 'kage-firefox-unit-'));
    await mkdir(path.join(root, 'personas'), { recursive: true });
    const engine = new FirefoxEngine(
      {
        personasDir: path.join(root, 'personas'),
        allowCrossOsClaims: false,
        gl: 'llvmpipe',
        selftestPages: [],
        log: silentLog,
      },
      host,
    );
    expect(engine.status()).toEqual({ browserVersion: '152.0.4' });
    const generated = await engine.generatePersona({
      seed: SEED,
      claimed_os: 'macos',
      languages: LANGUAGES,
    });
    expect(generated.summary).toMatchObject({
      engine: 'firefox',
      claimed_os: 'macos',
      browser_major: 152,
      platform: 'MacIntel',
      inject: 'none',
    });
    await expect(
      engine.generatePersona({
        seed: SEED,
        claimed_os: 'macos',
        languages: LANGUAGES,
        browser_major: 1,
      }),
    ).rejects.toMatchObject({ code: 'browser_major_mismatch' });
    const document = generated.fingerprint as FingerprintDocument;
    await expect(
      engine.openContext(request({ ...document, engine: 'chromium' })),
    ).rejects.toMatchObject({ code: 'invalid_request' });
    await expect(
      engine.openContext(request({ ...document, browser_version: '151.0.1' })),
    ).rejects.toMatchObject({ code: 'browser_major_mismatch' });
    await engine.close();
  });
});

describe('classifyNavigationError', () => {
  const error = (message: string, name = 'Error') => Object.assign(new Error(message), { name });

  it("names a failure by Firefox's own error", () => {
    expect(classifyNavigationError(error('page.goto: NS_ERROR_UNKNOWN_HOST'))).toEqual({
      code: 'navigation_failed',
      message: 'page.goto: NS_ERROR_UNKNOWN_HOST',
      net_error: 'NS_ERROR_UNKNOWN_HOST',
    });
    expect(
      classifyNavigationError(error('page.goto: NS_ERROR_PROXY_CONNECTION_REFUSED')).code,
    ).toBe('proxy_failed');
    expect(
      classifyNavigationError(error('page.goto: NS_ERROR_PROXY_FORBIDDEN\nCall log:')).net_error,
    ).toBe('NS_ERROR_PROXY_FORBIDDEN');
    expect(classifyNavigationError(error('page.goto: NS_ERROR_UNKNOWN_PROXY_HOST')).code).toBe(
      'proxy_failed',
    );
    expect(classifyNavigationError(error('page.goto: NS_BINDING_ABORTED')).code).toBe('aborted');
    expect(classifyNavigationError(error('page.goto: Download is starting')).code).toBe('aborted');
    expect(
      classifyNavigationError(error('page.goto: Target page, context or browser has been closed'))
        .code,
    ).toBe('aborted');
    expect(classifyNavigationError(error('Timeout 1000ms exceeded.', 'TimeoutError')).code).toBe(
      'timeout',
    );
    // Where Firefox shows its own error page the driver gives no name.
    expect(
      classifyNavigationError(
        error('page.goto: Navigation to "x" is interrupted by another navigation to "x"'),
      ),
    ).toMatchObject({ code: 'navigation_failed', net_error: null });
    expect(classifyNavigationError('odd')).toMatchObject({
      code: 'navigation_failed',
      message: 'odd',
    });
  });
});
