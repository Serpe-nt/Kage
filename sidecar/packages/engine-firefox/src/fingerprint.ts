import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';

import type { ClaimedOs, GeneratePersonaRequest } from '@kage/contract';
import {
  EngineError,
  FINGERPRINT_SCHEMA,
  SeededRandom,
  majorOfVersion,
  moveToVersion,
  osOfWebglRenderer,
  withSeededMathRandom,
  type FingerprintDocument,
} from '@kage/engine-core';
import Database from 'better-sqlite3';
// The launcher's parts, taken one by one. Its entry point is never
// imported: `launchOptions` draws unseeded random values, points fontconfig
// at a directory this build does not have, and decides the claimed system
// by parsing the User-Agent (ADR 0008).
import {
  fromBrowserforge,
  generateFingerprint as drawFingerprint,
} from 'camoufox-js/dist/fingerprints.js';
import FONTS from 'camoufox-js/dist/mappings/fonts.config.js';

import type { PropertyType } from './host.js';

const require = createRequire(import.meta.url);

/** The installed version of the launcher. Its manifest is read next to its entry point. */
export function launcherVersion(): string {
  const manifest = path.join(path.dirname(require.resolve('camoufox-js')), '..', 'package.json');
  return (JSON.parse(readFileSync(manifest, 'utf8')) as { version: string }).version;
}

/** The launcher's table of real WebGL fingerprints, one row per graphics card. */
const WEBGL_TABLE = path.join(
  path.dirname(require.resolve('camoufox-js')),
  'data-files',
  'webgl_data.db',
);

/** How the launcher's data names the three systems. */
const SHORT: Readonly<Record<ClaimedOs, 'win' | 'mac' | 'lin'>> = {
  windows: 'win',
  macos: 'mac',
  linux: 'lin',
};

/** Screen bounds applied when the request names none: common desktop sizes. */
const DEFAULT_SCREEN_BOUNDS = {
  min_width: 1280,
  max_width: 1920,
  min_height: 720,
  max_height: 1200,
};

/**
 * What stays the host's whatever the persona claims: the strings and
 * parameters of WebGL and the font files are the claimed system's, but
 * pixels are still drawn by the image's Mesa and text by its FreeType.
 */
export const HOST_BOUND = ['webgl_rendering', 'text_rendering'] as const;

/**
 * Keys of the browser's configuration a fingerprint must not carry: the
 * size of the page area is the real window's (the browser's own toolbar
 * decides it), and the history of a tab is the tab's own.
 */
const NEVER_SET = [
  'window.innerWidth',
  'window.innerHeight',
  'document.body.clientWidth',
  'document.body.clientHeight',
  'window.history.length',
] as const;

/** A value of the browser's configuration. */
export type ConfigValue = string | number | boolean | unknown[] | Record<string, unknown>;
/** The browser's configuration: what the launcher passes as `CAMOU_CONFIG`. */
export type CamoufoxConfig = Record<string, ConfigValue>;

/** What the persisted document keeps of a generated Firefox fingerprint. */
export interface GeneratedFirefox {
  /** The fingerprint of a real device, as the generator drew it. */
  readonly fingerprint: Record<string, unknown>;
  /** The complete configuration of the browser, without what an open request supplies. */
  readonly config: CamoufoxConfig;
  /** Whether the drawn graphics card supports WebGL 2. */
  readonly webgl2: boolean;
}

/** What generation needs to know about the engine it runs in. */
export interface GenerationFacts {
  /** Firefox version of the installed build. */
  readonly version: string;
  /** Camoufox release of the installed build. */
  readonly release: string;
  /** The configuration keys the build knows. */
  readonly properties: ReadonlyMap<string, PropertyType>;
}

function typeMatches(value: unknown, type: PropertyType): boolean {
  switch (type) {
    case 'str':
      return typeof value === 'string';
    case 'int':
      return Number.isInteger(value);
    case 'uint':
      return Number.isInteger(value) && (value as number) >= 0;
    case 'double':
      return typeof value === 'number';
    case 'bool':
      return typeof value === 'boolean';
    case 'array':
      return Array.isArray(value);
    case 'dict':
      return typeof value === 'object' && value !== null && !Array.isArray(value);
  }
}

/**
 * Checks a configuration against the keys and types the build knows.
 *
 * @throws {EngineError} `internal`, naming the key: the build would reject it at launch.
 */
export function validateConfig(
  config: CamoufoxConfig,
  properties: ReadonlyMap<string, PropertyType>,
): void {
  for (const [key, value] of Object.entries(config)) {
    const type = properties.get(key);
    if (type === undefined) {
      throw new EngineError('internal', `the Camoufox build does not know the key ${key}`);
    }
    if (!typeMatches(value, type)) {
      throw new EngineError('internal', `the Camoufox key ${key} must be ${type}`);
    }
  }
}

interface WebglRow {
  readonly renderer: string;
  readonly data: string;
  readonly weight: number;
}

/** A renderer that is a program and no graphics card: what a browser falls back to on a machine without one. */
const SOFTWARE_RENDERER = /SwiftShader|llvmpipe|softpipe|Basic Render Driver/i;

/**
 * Whether a renderer of the launcher's table can be the graphics card of a
 * persona that claims a system (spec §6.2).
 *
 * The table is measured on real traffic and carries its noise: it offers a
 * Direct3D renderer for macOS (0.85 % of the draws), which only Windows
 * has, and for Windows a SwiftShader string (0.27 %), which is Chrome's
 * software fallback and nothing a Firefox reports. Neither may become a
 * persona.
 */
export function plausibleRenderer(os: ClaimedOs, renderer: string): boolean {
  const named = osOfWebglRenderer(renderer);
  return (named === null || named === os) && !SOFTWARE_RENDERER.test(renderer);
}

/**
 * Draws a graphics card for a claimed system from the launcher's table,
 * each in proportion to how often real devices of that system have it,
 * leaving out the rows that cannot be that system's
 * ({@link plausibleRenderer}).
 */
export function drawWebgl(
  os: ClaimedOs,
  random: SeededRandom,
): { config: CamoufoxConfig; webgl2: boolean } {
  const column = SHORT[os];
  const database = new Database(WEBGL_TABLE, { readonly: true, fileMustExist: true });
  let rows: WebglRow[];
  try {
    rows = database
      .prepare(
        `SELECT renderer, data, ${column} AS weight FROM webgl_fingerprints WHERE ${column} > 0 ORDER BY vendor, renderer`,
      )
      .all() as WebglRow[];
  } finally {
    database.close();
  }
  rows = rows.filter((row) => plausibleRenderer(os, row.renderer));
  const total = rows.reduce((sum, row) => sum + row.weight, 0);
  let threshold = random.next() * total;
  const chosen =
    rows.find((row) => {
      threshold -= row.weight;
      return threshold <= 0;
    }) ?? rows.at(-1);
  if (chosen === undefined) {
    throw new EngineError('internal', `the launcher has no WebGL data for ${os}`);
  }
  const { webGl2Enabled, ...config } = JSON.parse(chosen.data) as CamoufoxConfig;
  return { config, webgl2: webGl2Enabled === true };
}

/** Name and version of what generated a fingerprint, as the document records it. */
export function generatorId(facts: GenerationFacts): string {
  return `camoufox-js@${launcherVersion()}+camoufox-${facts.version}-${facts.release}`;
}

/** `navigator.oscpu` of a stock Firefox on each system a persona may claim. */
export const OSCPU: Record<ClaimedOs, string> = {
  windows: 'Windows NT 10.0; Win64; x64',
  macos: 'Intel Mac OS X 10.15',
  linux: 'Linux x86_64',
};

/**
 * Generates the fingerprint document of a Firefox persona (spec §6.1 step 3).
 *
 * The document holds the complete configuration the browser is started
 * with, so that opening a context never generates anything: the same
 * document gives the same browser every time. Everything random comes from
 * the persona's seed: the real-device fingerprint (the generator's
 * `Math.random` is replaced for the draw), the graphics card, and the seeds
 * of the browser's own canvas, audio and text-metric variation, which make
 * a persona one stable device and twenty personas twenty.
 *
 * @throws {EngineError} `browser_major_mismatch`, or `invalid_request` when
 *   no fingerprint satisfies the constraints.
 */
export function generateFingerprint(
  request: GeneratePersonaRequest,
  facts: GenerationFacts,
): FingerprintDocument {
  const major = majorOfVersion(facts.version);
  if (major === null) {
    throw new EngineError('internal', `the installed browser has no version: ${facts.version}`);
  }
  if (request.browser_major !== undefined && request.browser_major !== major) {
    throw new EngineError(
      'browser_major_mismatch',
      `the engine bundles Firefox ${major}, the request expects ${request.browser_major}`,
    );
  }
  const bounds = {
    min_width: request.screen?.min_width ?? DEFAULT_SCREEN_BOUNDS.min_width,
    max_width: request.screen?.max_width ?? DEFAULT_SCREEN_BOUNDS.max_width,
    min_height: request.screen?.min_height ?? DEFAULT_SCREEN_BOUNDS.min_height,
    max_height: request.screen?.max_height ?? DEFAULT_SCREEN_BOUNDS.max_height,
  };
  let fingerprint: Record<string, unknown>;
  let config: CamoufoxConfig;
  try {
    [fingerprint, config] = withSeededMathRandom(request.seed, 'camoufox-fingerprint', () => {
      const drawn = drawFingerprint(undefined, {
        operatingSystems: [request.claimed_os],
        screen: {
          minWidth: bounds.min_width,
          maxWidth: bounds.max_width,
          minHeight: bounds.min_height,
          maxHeight: bounds.max_height,
        },
        strict: true,
      });
      return [
        JSON.parse(JSON.stringify(drawn)) as Record<string, unknown>,
        fromBrowserforge(drawn, String(major)) as CamoufoxConfig,
      ];
    });
  } catch (error) {
    throw new EngineError(
      'invalid_request',
      `no fingerprint satisfies the constraints: ${error instanceof Error ? error.message : String(error)}`,
      { cause: error },
    );
  }
  for (const key of NEVER_SET) {
    // eslint-disable-next-line @typescript-eslint/no-dynamic-delete -- the keys are a fixed list
    delete config[key];
  }

  // Some recorded devices carry no `oscpu` (about one Windows record in
  // thirty). The browser would then answer with its own, the engine
  // host's `Linux x86_64`, in the window of a persona that claims another
  // system: the one value that contradicts everything else it says.
  const oscpu = config['navigator.oscpu'];
  if (typeof oscpu !== 'string' || oscpu === '') {
    config['navigator.oscpu'] = OSCPU[request.claimed_os];
  }

  const random = new SeededRandom(request.seed, 'camoufox-config');
  const webgl = drawWebgl(request.claimed_os, random);
  const seed = () => random.integer(1, 4_294_967_295);
  Object.assign(config, webgl.config, {
    fonts: [...FONTS[SHORT[request.claimed_os]]],
    // Zero would switch the variation off; every persona gets its own.
    'fonts:spacing_seed': seed(),
    'audio:seed': seed(),
    'canvas:seed': seed(),
    'canvas:aaOffset': random.integer(-50, 50),
    'canvas:aaCapOffset': true,
    // The build's own look of the browser window is not Firefox's: its
    // toolbar is 51 pixels high where a stock Firefox has 85, which a page
    // reads as outerHeight minus innerHeight. Without it the window is
    // the stock one.
    disableTheming: true,
    // What a Firefox lists before a site has permission: one microphone,
    // and a camera on two machines in three; names and ids stay empty.
    'mediaDevices:enabled': true,
    'mediaDevices:micros': 1,
    'mediaDevices:webcams': random.chance(2 / 3) ? 1 : 0,
    'mediaDevices:speakers': 1,
    'AudioContext:sampleRate': random.chance(0.8) ? 48_000 : 44_100,
    'AudioContext:maxChannelCount': 2,
  } satisfies CamoufoxConfig);
  validateConfig(config, facts.properties);

  const text = (key: string) => {
    const value = config[key];
    if (typeof value !== 'string' || value === '') {
      throw new EngineError('internal', `the generated fingerprint has no ${key}`);
    }
    return value;
  };
  const number = (key: string, fallback: number) => {
    const value = config[key];
    return typeof value === 'number' ? value : fallback;
  };
  const generated: GeneratedFirefox = { fingerprint, config, webgl2: webgl.webgl2 };
  return {
    schema: FINGERPRINT_SCHEMA,
    engine: 'firefox',
    generator: generatorId(facts),
    seed: request.seed,
    claimed_os: request.claimed_os,
    browser_version: facts.version,
    user_agent: text('navigator.userAgent'),
    platform: text('navigator.platform'),
    languages: [...request.languages],
    native: {
      screen: {
        width: number('screen.width', bounds.min_width),
        height: number('screen.height', bounds.min_height),
        // The build advises against any other ratio than one.
        dpr: 1,
        color_depth: Math.max(24, Math.min(32, number('screen.colorDepth', 24))),
      },
      hardware_concurrency: Math.max(
        1,
        Math.min(256, Math.trunc(number('navigator.hardwareConcurrency', 4))),
      ),
    },
    host_bound: [...HOST_BOUND],
    inject: 'none',
    generated: generated as unknown as Record<string, unknown>,
  };
}

/**
 * A Firefox `User-Agent` of one major as the same browser's of another:
 * Firefox names its major twice (`rv:152.0` and `Firefox/152.0`).
 *
 * @throws {EngineError} `invalid_request` when it is not a Firefox
 *   `User-Agent` of that major.
 */
export function firefoxUserAgentOf(userAgent: string, from: number, to: number): string {
  const moved = userAgent
    .replace(`rv:${from}.0)`, `rv:${to}.0)`)
    .replace(new RegExp(`Firefox/${from}\\.0$`), `Firefox/${to}.0`);
  if (!moved.includes(`rv:${to}.0)`) || !moved.endsWith(`Firefox/${to}.0`)) {
    throw new EngineError('invalid_request', `not a User-Agent of Firefox ${from}: ${userAgent}`);
  }
  return moved;
}

/**
 * Moves a Firefox persona's document to the build the engine runs, as
 * Firefox's own update moves a person's browser (contract 1.4.0): the
 * version, and the `User-Agent` in the two places it is, the document's and
 * the configuration's. The device the generator drew, the seeds and the
 * graphics card stay; the keys the new build no longer reads stay in the
 * document too, and the launch plan leaves them out.
 *
 * Generating from the same seed on the new build gives the same document as
 * moving one from the old build (the generator is the same; the unit tests
 * hold it), so a persona that moves and one made new are alike.
 *
 * @throws {EngineError} `invalid_request` for a document of another engine,
 *   `browser_major_mismatch` for one of a newer major.
 */
export function upgradeFingerprint(
  document: FingerprintDocument,
  version: string,
): { document: FingerprintDocument; upgraded: boolean } {
  if (document.engine !== 'firefox') {
    throw new EngineError(
      'invalid_request',
      `a ${document.engine} fingerprint is not moved by the firefox engine`,
    );
  }
  const generated = generatedOf(document);
  const moved = moveToVersion(document, version, firefoxUserAgentOf);
  if (!moved.upgraded) {
    return moved;
  }
  const next: GeneratedFirefox = {
    ...generated,
    config: { ...generated.config, 'navigator.userAgent': moved.document.user_agent },
  };
  return {
    document: { ...moved.document, generated: next as unknown as Record<string, unknown> },
    upgraded: true,
  };
}

/**
 * Reads the generated part of a Firefox fingerprint document back.
 *
 * @throws {EngineError} `invalid_request` when the document was not generated by this engine.
 */
export function generatedOf(document: FingerprintDocument): GeneratedFirefox {
  const generated = document.generated as Partial<GeneratedFirefox>;
  if (
    typeof generated.config !== 'object' ||
    typeof generated.fingerprint !== 'object' ||
    typeof generated.webgl2 !== 'boolean'
  ) {
    throw new EngineError(
      'invalid_request',
      'not a fingerprint document of the firefox engine: generate it again from its seed',
    );
  }
  return generated as GeneratedFirefox;
}
