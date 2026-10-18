import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, readFile, stat, symlink, writeFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { deflateRawSync } from 'node:zlib';

import type { ClaimedOs, OpenContextRequest } from '@kage/contract';
import {
  EngineError,
  FingerprintDocumentSchema,
  SeededRandom,
  silentLog,
  type FingerprintDocument,
} from '@kage/engine-core';
import { beforeAll, describe, expect, it } from 'vitest';

import {
  CAMOUFOX_OMNI_SHA256,
  CAMOUFOX_RELEASE,
  CAMOUFOX_VERSION,
  FirefoxEngine,
  NEVER_APPLIED,
  OSCPU,
  basePreferences,
  classifyNavigationError,
  configEnvironment,
  cpuListOf,
  detectHost,
  drawWebgl,
  firefoxUiLocale,
  firefoxUserAgentOf,
  generateFingerprint,
  generatedOf,
  generatorId,
  keepBeforeUpgrade,
  keptProfileDir,
  launchPlan,
  launcherVersion,
  localeConfig,
  parseLocales,
  parseProperties,
  parseRemovedProperties,
  personaFontconfig,
  plausibleRenderer,
  preferenceEnvironment,
  profileMajor,
  profilePaths,
  readArchiveEntry,
  upgradeFingerprint,
  validateConfig,
  webgpuPreferences,
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

/** Whether a file or directory exists. */
const exists = (file: string) =>
  stat(file).then(
    () => true,
    () => false,
  );

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
    expect([CAMOUFOX_VERSION, CAMOUFOX_RELEASE]).toEqual(['156.0.1', 'beta.39']);
    expect(host.properties.get('navigator.userAgent')).toBe('str');
    expect(host.properties.size).toBeGreaterThan(100);
    // The image and the setup script fetch the build the engine names.
    const dockerfile = await readFile(
      new URL('../../../../deploy/docker/sidecar.Dockerfile', import.meta.url),
      'utf8',
    );
    expect(/^ARG CAMOUFOX_VERSION=(.+)$/m.exec(dockerfile)?.[1]).toBe(CAMOUFOX_VERSION);
    expect(/^ARG CAMOUFOX_RELEASE=(.+)$/m.exec(dockerfile)?.[1]).toBe(CAMOUFOX_RELEASE);
    expect(/^ARG CAMOUFOX_SHA256=(.+)$/m.exec(dockerfile)?.[1]).toMatch(/^[0-9a-f]{64}$/);
    // Nothing is patched into the build any more (ADR 0010 is superseded).
    expect(dockerfile).not.toMatch(/CAMOUFOX_OMNI|patch-camoufox/);

    const empty = await mkdtemp(path.join(tmpdir(), 'kage-nocamoufox-'));
    await expect(detectHost(empty)).rejects.toMatchObject({ code: 'browser_launch_failed' });
    // Another build, even another one of the same Firefox, is refused: the
    // launcher's own download installs exactly such a build.
    await writeFile(
      path.join(empty, 'version.json'),
      JSON.stringify({ version: '156.0.1', release: 'beta.38' }),
    );
    const refused: unknown = await detectHost(empty).catch((error: unknown) => error);
    expect(refused).toBeInstanceOf(EngineError);
    expect((refused as EngineError).message).toContain('156.0.1-beta.38');
    await writeFile(path.join(empty, 'version.json'), 'not json');
    await expect(detectHost(empty)).rejects.toMatchObject({ code: 'browser_launch_failed' });
  });

  it("runs that build's own archive of its scripts and no other", async () => {
    expect(host.omniSha256).toBe(CAMOUFOX_OMNI_SHA256);
    // The pinned build with anything else in its archive: the driver inside
    // could grant user activation again, or show itself to a page.
    const changed = await mkdtemp(path.join(tmpdir(), 'kage-changed-'));
    await writeFile(
      path.join(changed, 'version.json'),
      JSON.stringify({ version: CAMOUFOX_VERSION, release: CAMOUFOX_RELEASE }),
    );
    const missing: unknown = await detectHost(changed).catch((error: unknown) => error);
    expect(missing).toMatchObject({ code: 'browser_launch_failed' });
    expect((missing as EngineError).message).toContain('omni.ja cannot be read');
    await writeFile(path.join(changed, 'omni.ja'), 'the archive of a release');
    const refused: unknown = await detectHost(changed).catch((error: unknown) => error);
    expect(refused).toMatchObject({ code: 'browser_launch_failed' });
    expect((refused as EngineError).message).toContain("is not the pinned build's");
    // What the archive is, so that a log line says which one somebody installed.
    expect((refused as EngineError).message).toContain(
      '96d5c9f90ccaf996e7b1ccc75831cfdeca1e6cb11586709bbc3407387e467116',
    );
    expect((refused as EngineError).message).toContain('just setup-camoufox');
  });

  it('knows which keys the build no longer reads and which languages it ships', () => {
    // Declared with "removed" so that older documents still validate.
    for (const key of [
      'canvas:seed',
      'canvas:aaOffset',
      'canvas:aaCapOffset',
      'fonts:spacing_seed',
      'disableTheming',
      'webGl:contextAttributes',
      'webGl2:contextAttributes',
      'navigator.product',
      'navigator.appCodeName',
      'navigator.appName',
    ]) {
      expect(host.removedProperties.has(key), key).toBe(true);
      expect(host.properties.has(key), key).toBe(true);
    }
    expect(host.removedProperties.has('navigator.userAgent')).toBe(false);
    expect(host.removedProperties.has('audio:seed')).toBe(false);
    // The build carries Firefox's language packs (152 had en-US only).
    for (const locale of ['en-US', 'de', 'fr', 'it', 'en-GB', 'es-ES', 'pt-BR', 'zh-CN']) {
      expect(host.locales, locale).toContain(locale);
    }
    expect(
      parseRemovedProperties(
        '[{"property":"a","type":"str"},{"property":"b","type":"bool","removed":"156.0.1-beta.32"},{"x":1}]',
      ),
    ).toEqual(new Set(['b']));
    expect(parseLocales('en-US,de, fr\n')).toEqual(['en-US', 'de', 'fr']);
  });

  it('reads a file out of a zip archive, stored or deflated', () => {
    // A zip of two entries made by hand: "a.txt" stored, "b.txt" deflated.
    const entry = (name: string, data: Buffer, method: number, offset: number) => {
      const body = method === 8 ? deflateRawSync(data) : data;
      const local = Buffer.alloc(30 + name.length);
      local.writeUInt32LE(0x04034b50, 0);
      local.writeUInt16LE(method, 8);
      local.writeUInt32LE(body.length, 18);
      local.writeUInt32LE(data.length, 22);
      local.writeUInt16LE(name.length, 26);
      local.write(name, 30);
      const central = Buffer.alloc(46 + name.length);
      central.writeUInt32LE(0x02014b50, 0);
      central.writeUInt16LE(method, 10);
      central.writeUInt32LE(body.length, 20);
      central.writeUInt32LE(data.length, 24);
      central.writeUInt16LE(name.length, 28);
      central.writeUInt32LE(offset, 42);
      central.write(name, 46);
      return { local: Buffer.concat([local, body]), central };
    };
    const a = entry('a.txt', Buffer.from('stored text'), 0, 0);
    const b = entry('b.txt', Buffer.from('deflated text '.repeat(20)), 8, a.local.length);
    const directory = Buffer.concat([a.central, b.central]);
    const end = Buffer.alloc(22);
    end.writeUInt32LE(0x06054b50, 0);
    end.writeUInt16LE(2, 10);
    end.writeUInt32LE(directory.length, 12);
    end.writeUInt32LE(a.local.length + b.local.length, 16);
    const zip = Buffer.concat([a.local, b.local, directory, end]);
    expect(readArchiveEntry(zip, 'a.txt')?.toString()).toBe('stored text');
    expect(readArchiveEntry(zip, 'b.txt')?.toString()).toBe('deflated text '.repeat(20));
    expect(readArchiveEntry(zip, 'c.txt')).toBeNull();
    expect(() => readArchiveEntry(Buffer.from('no zip at all'), 'a.txt')).toThrow(/no end/);
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
      generator: 'camoufox-js@0.12.1+camoufox-156.0.1-beta.39',
      seed: SEED,
      claimed_os: 'windows',
      browser_version: '156.0.1',
      user_agent:
        'Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:156.0) Gecko/20100101 Firefox/156.0',
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
      'webGl:renderer':
        'ANGLE (NVIDIA, NVIDIA GeForce GTX 480 Direct3D11 vs_5_0 ps_5_0), or similar',
      'fonts:spacing_seed': 3_482_387_260,
      'audio:seed': 3_024_434_807,
      'canvas:seed': 104_999_401,
      'canvas:aaOffset': 6,
      'canvas:aaCapOffset': true,
      disableTheming: true,
      'window.outerWidth': 1616,
      'window.outerHeight': 916,
      'mediaDevices:enabled': true,
      'mediaDevices:micros': 1,
      'mediaDevices:webcams': 1,
      'AudioContext:sampleRate': 48_000,
      'AudioContext:maxChannelCount': 2,
    });
    expect(webgl2).toBe(true);
  });

  it('never draws a graphics card of another system, or a renderer that is a program', () => {
    // What the launcher's table offers and a persona must not get.
    const direct3d = 'ANGLE (NVIDIA, NVIDIA GeForce GTX 980 Direct3D11 vs_5_0 ps_5_0), or similar';
    expect(plausibleRenderer('windows', direct3d)).toBe(true);
    expect(plausibleRenderer('macos', direct3d)).toBe(false);
    expect(plausibleRenderer('linux', direct3d)).toBe(false);
    expect(plausibleRenderer('macos', 'Apple M1, or similar')).toBe(true);
    expect(plausibleRenderer('windows', 'Apple M1, or similar')).toBe(false);
    // A card's name says nothing about the system.
    expect(plausibleRenderer('macos', 'Intel(R) HD Graphics 400, or similar')).toBe(true);
    expect(plausibleRenderer('linux', 'NVIDIA GeForce GTX 980, or similar')).toBe(true);
    for (const software of [
      'ANGLE (Google, Vulkan 1.3.0 (SwiftShader Device (Subzero) (0x0000C0DE)), SwiftShader driver)',
      'llvmpipe (LLVM 15.0.7, 256 bits)',
      'ANGLE (Microsoft, Microsoft Basic Render Driver Direct3D11 vs_5_0 ps_5_0)',
    ]) {
      for (const os of SYSTEMS) {
        expect(plausibleRenderer(os, software), `${os}: ${software}`).toBe(false);
      }
    }
    // The table has such rows for macOS (0.85 % of the draws) and Windows
    // (0.27 %): four hundred draws would meet them.
    for (const os of SYSTEMS) {
      const drawn = new Set<string>();
      for (let index = 0; index < 400; index += 1) {
        const { config } = drawWebgl(os, new SeededRandom(SEED, `webgl ${index}`));
        const renderer = config['webGl:renderer'];
        expect(typeof renderer).toBe('string');
        drawn.add(renderer as string);
      }
      expect(drawn.size).toBeGreaterThan(2);
      for (const renderer of drawn) {
        expect(plausibleRenderer(os, renderer), `${os}: ${renderer}`).toBe(true);
      }
    }
  });

  it('describes each system it may claim in every value that names one', () => {
    const marks: Record<ClaimedOs, [RegExp, string, RegExp, string]> = {
      windows: [/\(Windows NT 10\.0; Win64; x64; rv:156\.0\)/, 'Win32', /Direct3D/, 'Segoe UI'],
      macos: [
        /\(Macintosh; Intel Mac OS X [\d.]+; rv:156\.0\)/,
        'MacIntel',
        /Apple|Intel|AMD/,
        'Helvetica Neue',
      ],
      linux: [/\(X11; Linux x86_64; rv:156\.0\)/, 'Linux x86_64', /.+/, 'Arimo'],
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

/** Fewest distinct (system, screen, processors) among forty personas of the tables. */
const MIN_PAIRS = 16;

/** The n-th of a series of persona seeds. */
const seedOf = (n: number) => createHash('sha256').update(`persona-${n}`).digest('hex');

// The screen classes Kage draws a Firefox persona's screen from and asks
// for by exact bounds (`crates/kage-core/src/persona/screen.rs`, tables v1).
const SCREEN_CLASSES: Record<'windows' | 'macos', [number, number][]> = {
  windows: [
    [1920, 1080],
    [2560, 1440],
    [1600, 900],
    [3440, 1440],
    [3840, 1080],
  ],
  macos: [
    [2560, 1440],
    [1920, 1080],
    [3440, 1440],
    [2560, 1600],
    [1680, 1050],
  ],
};

describe('what Kage asks the generator for (spec §6.1)', () => {
  it('gives every persona the oscpu of the system it claims, whatever its device record lacks', () => {
    const kind: Record<ClaimedOs, RegExp> = {
      windows: /^Windows NT \d+\.\d+; Win64; x64$/u,
      macos: /^Intel Mac OS X \d+[._]\d+/u,
      linux: /^Linux /u,
    };
    let recordsWithout = 0;
    for (const [os, count] of [
      ['windows', 240],
      ['macos', 40],
      ['linux', 40],
    ] as const) {
      for (let n = 0; n < count; n += 1) {
        const document = generate(os, seedOf(n));
        const { config, fingerprint } = generatedOf(document);
        const recorded = (fingerprint['navigator'] as Record<string, unknown> | undefined)?.[
          'oscpu'
        ];
        if (typeof recorded !== 'string' || recorded === '') {
          recordsWithout += 1;
          // The record is silent: the stock value of the claimed system.
          expect(config['navigator.oscpu'], `${os} ${n}`).toBe(OSCPU[os]);
        }
        expect(config['navigator.oscpu'], `${os} ${n}`).toMatch(kind[os]);
        expect(() => {
          validateConfig(config, host.properties);
        }).not.toThrow();
      }
    }
    // The sample holds device records without the value, or this test
    // would prove nothing.
    expect(recordsWithout).toBeGreaterThan(0);
    for (const os of SYSTEMS) {
      expect(OSCPU[os]).toMatch(kind[os]);
    }
    // Hundreds of fingerprints take a few seconds.
  }, 60_000);

  it('generates every screen class of the tables by exact bounds, on varied hardware', () => {
    const pairs = new Set<string>();
    const processors = new Set<number>();
    let n = 0;
    for (const os of ['windows', 'macos'] as const) {
      for (const [width, height] of SCREEN_CLASSES[os]) {
        for (let repeat = 0; repeat < 4; repeat += 1) {
          n += 1;
          const document = generateFingerprint(
            {
              seed: seedOf(n),
              claimed_os: os,
              languages: LANGUAGES,
              screen: {
                min_width: width,
                max_width: width,
                min_height: height,
                max_height: height,
              },
            },
            host,
          );
          expect(document.native.screen, `${os} ${width}x${height}`).toMatchObject({
            width,
            height,
            dpr: 1,
          });
          const { config } = generatedOf(document);
          expect(config['screen.width']).toBe(width);
          expect(config['screen.height']).toBe(height);
          pairs.add(`${os} ${width}x${height} ${document.native.hardware_concurrency}`);
          processors.add(document.native.hardware_concurrency);
        }
      }
    }
    // Forty personas over ten classes: the processors are the
    // generator's own draw and differ within a class.
    expect(processors.size).toBeGreaterThanOrEqual(3);
    expect(pairs.size).toBeGreaterThanOrEqual(MIN_PAIRS);
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
    const sent = Object.fromEntries(
      Object.entries(generatedOf(document).config).filter(
        ([key]) => !host.removedProperties.has(key) && !NEVER_APPLIED.includes(key),
      ),
    );
    expect(plan.config).toMatchObject({
      ...sent,
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
    // The configuration and the preferences reach the browser whole, in
    // chunks it can read.
    const joined = (prefix: string) =>
      Object.entries(plan.configEnv)
        .filter(([name]) => name.startsWith(prefix))
        .sort(([a], [b]) => Number(a.split('_').at(-1)) - Number(b.split('_').at(-1)))
        .map(([, value]) => value)
        .join('');
    expect(JSON.parse(joined('CAMOU_CONFIG_'))).toEqual(plan.config);
    expect(JSON.parse(joined('CAMOU_PREFS_'))).toEqual(plan.preferences);
    expect(Object.keys(configEnvironment({ fonts: ['x'.repeat(70_000)] }))).toEqual([
      'CAMOU_CONFIG_1',
      'CAMOU_CONFIG_2',
      'CAMOU_CONFIG_3',
    ]);
    expect(Object.keys(preferenceEnvironment({ a: 'x'.repeat(40_000) }))).toEqual([
      'CAMOU_PREFS_1',
      'CAMOU_PREFS_2',
    ]);
    expect(preferenceEnvironment({})).toEqual({});

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
    // Sound waits for the user, as in a Firefox nobody configured; the
    // build ships that default now, and a build that changes it is still
    // answered this way.
    expect(plan.preferences['media.autoplay.default']).toBe(1);
    // Two of the build's choices are back at Firefox's own (the audit).
    expect(plan.preferences).toMatchObject({
      'browser.cache.memory.enable': true,
      'network.http.dictionaries.enable': true,
    });
    // Nothing of the launcher's own input simulation is switched on.
    expect(plan.config).not.toHaveProperty(['humanize']);
  });

  it('sends only what the build reads of a document, and neither DNT nor GPC', () => {
    for (const os of SYSTEMS) {
      const document = generate(os);
      const { config } = generatedOf(document);
      const plan = launchPlan(document, request(document), host, '/p/page.pdf');
      const dropped = Object.keys(config).filter((key) => !(key in plan.config));
      // The ten keys Camoufox 156 no longer reads, and the two Firefox owns.
      expect(dropped.sort(), os).toEqual(
        [
          'canvas:aaCapOffset',
          'canvas:aaOffset',
          'canvas:seed',
          'disableTheming',
          'fonts:spacing_seed',
          'navigator.appCodeName',
          'navigator.appName',
          'navigator.product',
          'webGl2:contextAttributes',
          'webGl:contextAttributes',
          ...NEVER_APPLIED.filter((key) => key in config),
        ].sort(),
      );
      for (const key of Object.keys(plan.config)) {
        expect(host.removedProperties.has(key), key).toBe(false);
      }
      // What stays is what was generated: the document is not touched.
      for (const [key, value] of Object.entries(plan.config)) {
        if (key in config) {
          expect(value, key).toEqual(config[key]);
        }
      }
    }
    // The generator's records say DNT "1" for every persona; the sample
    // must hold one, or this test proves nothing.
    expect(generatedOf(generate('windows')).config['navigator.doNotTrack']).toBe('1');
  });

  it('starts the browser in the interface language a person of the persona’s country installs', () => {
    // Every locale the persona tables give (kage_core::persona::locale).
    const expected: Record<string, string> = {
      'de-AT': 'de',
      'de-DE': 'de',
      'de-CH': 'de',
      'fr-FR': 'fr',
      'fr-BE': 'fr',
      'fr-CH': 'fr',
      'fr-LU': 'fr',
      'fr-CA': 'fr',
      'it-IT': 'it',
      'it-CH': 'it',
      'en-GB': 'en-GB',
      'en-IE': 'en-GB',
      'en-CA': 'en-CA',
      'en-US': 'en-US',
      'nl-NL': 'nl',
      'nl-BE': 'nl',
      'es-ES': 'es-ES',
      'pl-PL': 'pl',
      'pt-PT': 'pt-PT',
      'ro-RO': 'ro',
      'sv-SE': 'sv-SE',
    };
    for (const [locale, ui] of Object.entries(expected)) {
      expect(firefoxUiLocale(locale, host.locales), locale).toBe(ui);
      expect(host.locales, locale).toContain(ui);
    }
    expect(firefoxUiLocale('pt-BR', host.locales)).toBe('pt-BR');
    expect(firefoxUiLocale('es-MX', host.locales)).toBe('es-MX');
    // A language the table does not know, and a build without the language.
    expect(firefoxUiLocale('xx-YY', host.locales)).toBe('en-US');
    expect(firefoxUiLocale('de-DE', ['en-US'])).toBe('en-US');
    const document = generate('windows');
    expect(
      launchPlan(document, request(document, { locale: 'en-GB' }), host, '/p/page.pdf').preferences[
        'intl.locale.requested'
      ],
    ).toBe('en-GB');
    expect(
      launchPlan(document, request(document), host, '/p/page.pdf').preferences[
        'intl.locale.requested'
      ],
    ).toBe('de');
  });

  it('gives WebGPU to the systems whose Firefox has it, with a blocked adapter', () => {
    const blocked = { 'dom.webgpu.enabled': true, 'gfx.blacklist.webgpu': 4 };
    expect(webgpuPreferences('windows', {})).toEqual(blocked);
    expect(webgpuPreferences('macos', { 'webGl:vendor': 'Apple Inc.' })).toEqual(blocked);
    expect(webgpuPreferences('macos', { 'webGl:renderer': 'Apple M2, or similar' })).toEqual(
      blocked,
    );
    expect(webgpuPreferences('macos', { 'webGl:vendor': 'Intel Inc.' })).toEqual({});
    expect(webgpuPreferences('linux', {})).toEqual({});
    const document = generate('windows');
    expect(launchPlan(document, request(document), host, '/p/page.pdf').preferences).toMatchObject(
      blocked,
    );
  });

  it('gives each claimed system its own fonts by an absolute path, unhinted except for Linux', () => {
    for (const os of SYSTEMS) {
      const document = generate(os);
      const { fontconfig } = launchPlan(document, request(document), host, '/p/page.pdf');
      expect(fontconfig).toContain(`<dir>${path.join(host.installDir, 'fonts')}</dir>`);
      expect(fontconfig).not.toContain('prefix="cwd"');
      // A Linux claim keeps the build's own setting, the light autohinter
      // since 156 (whole-pixel advances were a tell there too).
      expect(fontconfig).toContain(
        os === 'linux' ? '<const>hintslight</const>' : '<const>hintnone</const>',
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
    expect(engine.status()).toEqual({ browserVersion: '156.0.1' });
    const generated = await engine.generatePersona({
      seed: SEED,
      claimed_os: 'macos',
      languages: LANGUAGES,
    });
    expect(generated.summary).toMatchObject({
      engine: 'firefox',
      claimed_os: 'macos',
      browser_major: 156,
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
    const older = await engine
      .openContext(request({ ...document, browser_version: '152.0.4' }))
      .catch((error: unknown) => error);
    expect(older).toMatchObject({ code: 'browser_major_mismatch' });
    expect((older as EngineError).message).toContain('POST /v1/personas/upgrade');
    await engine.close();
  });
});

/** A document of this build as Firefox `major` would have made it: the same device, an older browser. */
const asOf = (document: FingerprintDocument, version: string): FingerprintDocument => {
  const major = Number(version.split('.')[0]);
  const userAgent = firefoxUserAgentOf(document.user_agent, 156, major);
  const generated = generatedOf(document);
  return {
    ...document,
    browser_version: version,
    user_agent: userAgent,
    generated: {
      ...generated,
      config: { ...generated.config, 'navigator.userAgent': userAgent },
    },
  };
};

describe('moving a persona to a newer Firefox (contract 1.4.0)', () => {
  it("rewrites the major in both places of a Firefox User-Agent, and nothing that isn't one", () => {
    for (const os of SYSTEMS) {
      const userAgent = generate(os).user_agent;
      const older = firefoxUserAgentOf(userAgent, 156, 152);
      expect(older).toContain('rv:152.0)');
      expect(older.endsWith('Firefox/152.0')).toBe(true);
      expect(firefoxUserAgentOf(older, 152, 156)).toBe(userAgent);
    }
    expect(() =>
      firefoxUserAgentOf(
        'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36',
        152,
        156,
      ),
    ).toThrow(expect.objectContaining({ code: 'invalid_request' }) as Error);
  });

  it('gives a persona moved from Firefox 152 the document a new one of this build has', () => {
    for (const os of SYSTEMS) {
      const made = generate(os);
      const moved = upgradeFingerprint(asOf(made, '152.0.4'), host.version);
      expect(moved.upgraded, os).toBe(true);
      expect(moved.document, os).toEqual(made);
      // A document of this build stays as it is.
      expect(upgradeFingerprint(made, host.version)).toEqual({ document: made, upgraded: false });
    }
    const made = generate('windows');
    expect(() => upgradeFingerprint({ ...made, engine: 'chromium' }, host.version)).toThrow(
      expect.objectContaining({ code: 'invalid_request' }) as Error,
    );
    // A browser does not go back to an older major.
    expect(() => upgradeFingerprint(asOf(made, '157.0'), host.version)).toThrow(
      expect.objectContaining({ code: 'browser_major_mismatch' }) as Error,
    );
  });

  it('moves a document through the engine', async () => {
    const engine = new FirefoxEngine(
      {
        personasDir: await mkdtemp(path.join(tmpdir(), 'kage-firefox-up-')),
        allowCrossOsClaims: false,
        gl: 'llvmpipe',
        selftestPages: [],
        log: silentLog,
      },
      host,
    );
    const made = generate('macos');
    const answer = await engine.upgradePersona({ fingerprint: asOf(made, '152.0.4') });
    expect(answer).toMatchObject({ upgraded: true, summary: { browser_major: 156 } });
    expect(answer.fingerprint).toEqual(made);
    await expect(
      engine.upgradePersona({ fingerprint: { not: 'a document' } }),
    ).rejects.toMatchObject({ code: 'invalid_request' });
    await engine.close();
  });

  it('keeps the profile as it was before its first start under a newer major, once', async () => {
    const personas = await mkdtemp(path.join(tmpdir(), 'kage-firefox-keep-'));
    const profileDir = path.join(personas, 'firefox', 'p1');
    const { userData } = profilePaths(profileDir);
    await mkdir(path.join(userData, 'cache2'), { recursive: true });
    await mkdir(path.join(userData, 'startupCache'), { recursive: true });
    await writeFile(
      path.join(userData, 'compatibility.ini'),
      '[Compatibility]\nLastVersion=152.0.4-beta.30_20260831224106/20260831224106\n',
    );
    await writeFile(path.join(userData, 'cookies.sqlite'), 'the cookies');
    await writeFile(path.join(userData, 'cache2', 'entry'), 'a cached page');
    await symlink('127.0.0.1:+4242', path.join(userData, 'lock'));
    expect(await profileMajor(userData)).toBe(152);
    const keep = (hostVersion = host.version, dir = profileDir) =>
      keepBeforeUpgrade({
        personasDir: personas,
        personaId: '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e',
        profileDir: dir,
        relativeProfileDir: 'firefox/p1',
        userData: profilePaths(dir).userData,
        hostVersion,
        now: new Date('2026-10-18T20:00:00Z'),
      });

    // Not for a profile of this major or a newer one.
    expect(await keep('152.0.4')).toBeNull();
    const kept = await keep();
    expect(kept).toBe(keptProfileDir(personas, '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e'));
    expect(kept).toBe(path.join(personas, '.upgrade', '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e'));
    const keptData = path.join(kept ?? '', 'user-data');
    expect(await readFile(path.join(keptData, 'cookies.sqlite'), 'utf8')).toBe('the cookies');
    expect(await readFile(path.join(keptData, 'compatibility.ini'), 'utf8')).toContain('152.0.4');
    // Caches Firefox builds again, and a lock of a browser, are left out.
    for (const left of ['cache2', 'startupCache', 'lock']) {
      expect(await exists(path.join(keptData, left)), left).toBe(false);
    }
    expect(JSON.parse(await readFile(path.join(kept ?? '', 'upgrade.json'), 'utf8'))).toEqual({
      persona_id: '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e',
      profile_dir: 'firefox/p1',
      from_major: 152,
      to_version: '156.0.1',
      copied_at: '2026-10-18T20:00:00.000Z',
    });
    // The first copy is the profile as it was: a later open takes none.
    await writeFile(path.join(userData, 'cookies.sqlite'), 'cookies of the new browser');
    expect(await keep()).toBeNull();
    expect(await readFile(path.join(keptData, 'cookies.sqlite'), 'utf8')).toBe('the cookies');
    // A new profile has nothing to keep.
    const fresh = path.join(personas, 'firefox', 'p2');
    expect(await keep(host.version, fresh)).toBeNull();
    expect(await profileMajor(profilePaths(fresh).userData)).toBeNull();
    // A copy that cannot be written leaves nothing half written behind.
    const blocked = await mkdtemp(path.join(tmpdir(), 'kage-firefox-blocked-'));
    await writeFile(path.join(blocked, '.upgrade'), 'a file where the directory goes');
    await expect(
      keepBeforeUpgrade({
        personasDir: blocked,
        personaId: 'p1',
        profileDir,
        relativeProfileDir: 'firefox/p1',
        userData,
        hostVersion: host.version,
        now: new Date(),
      }),
    ).rejects.toThrow();
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
