import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import path from 'node:path';

import type { ClaimedOs, GeneratePersonaRequest } from '@kage/contract';
import {
  EngineError,
  FINGERPRINT_SCHEMA,
  hintPlatformOf,
  majorOfVersion,
  withSeededMathRandom,
  type FingerprintDocument,
} from '@kage/engine-core';
import { FingerprintGenerator } from 'fingerprint-generator';
import { FingerprintInjector } from 'fingerprint-injector';

import { chromeBrands, linuxChromeUserAgent } from './host.js';

const require = createRequire(import.meta.url);

/**
 * The installed version of a package of the fingerprint suite. Their
 * manifests are not exported, so the file is read next to the entry point.
 */
export function suiteVersion(name: 'fingerprint-generator' | 'fingerprint-injector'): string {
  const manifest = path.join(path.dirname(require.resolve(name)), 'package.json');
  return (JSON.parse(readFileSync(manifest, 'utf8')) as { version: string }).version;
}

/** Name and version of the generator, as the fingerprint document records it. */
export const GENERATOR_ID = `fingerprint-generator@${suiteVersion('fingerprint-generator')}`;

/**
 * Values no launch parameter changes: every persona on one host shows the
 * same ones, and only other hardware (a GPU, another amount of memory, a
 * sound card) changes them. Recorded in the document so that nobody takes
 * them for per-persona values (ADR 0009).
 */
export const HOST_BOUND = [
  'device_memory',
  'webgl',
  'fonts',
  'audio_devices',
  'work_area',
] as const;

/** Screen bounds applied when the request names none: common desktop sizes. */
const DEFAULT_SCREEN_BOUNDS = {
  min_width: 1280,
  max_width: 2560,
  min_height: 720,
  max_height: 1600,
};

/** What generation needs to know about the engine it runs in. */
export interface GenerationFacts {
  /** Full version of the installed Chrome. */
  readonly browserVersion: string;
  /** Whether a persona may claim another operating system than Linux. */
  readonly allowCrossOsClaims: boolean;
}

interface Generated {
  fingerprint: {
    navigator: {
      userAgent: string;
      platform: string;
      hardwareConcurrency: number;
      userAgentData?: Record<string, unknown> | null;
      [key: string]: unknown;
    };
    screen: { width: number; height: number; [key: string]: unknown };
    [key: string]: unknown;
  };
  headers: Record<string, string>;
}

let generator: FingerprintGenerator | undefined;

function userAgentFor(os: ClaimedOs, major: number, generatedUserAgent: string): string {
  if (os === 'linux') {
    return linuxChromeUserAgent(major);
  }
  // The generator's string is for whatever Chrome its data knows; the
  // persona runs the installed one.
  return generatedUserAgent.replace(/Chrome\/\d+\.0\.0\.0/, `Chrome/${major}.0.0.0`);
}

/**
 * Generates the fingerprint document of a persona (spec §6.1 step 3).
 *
 * The generator draws from `Math.random()` and takes no seed, so the draw
 * runs with `Math.random` replaced by a stream from the persona's seed: the
 * same seed, constraints and generator version give the same document, byte
 * for byte. The generator's idea of the browser version is never used: the
 * document carries the installed Chrome's.
 *
 * @throws {EngineError} `cross_os_claim_disabled`, `browser_major_mismatch`,
 *   or `invalid_request` when no fingerprint satisfies the constraints.
 */
export function generateFingerprint(
  request: GeneratePersonaRequest,
  facts: GenerationFacts,
): FingerprintDocument {
  const major = majorOfVersion(facts.browserVersion);
  if (major === null) {
    throw new EngineError(
      'internal',
      `the installed browser has no version: ${facts.browserVersion}`,
    );
  }
  if (request.claimed_os !== 'linux' && !facts.allowCrossOsClaims) {
    throw new EngineError(
      'cross_os_claim_disabled',
      `the chromium engine runs on Linux and claims linux only; a ${request.claimed_os} claim is contradicted by the browser's own workers (ADR 0009)`,
    );
  }
  if (request.browser_major !== undefined && request.browser_major !== major) {
    throw new EngineError(
      'browser_major_mismatch',
      `the engine bundles Chrome ${major}, the request expects ${request.browser_major}`,
    );
  }
  const bounds = {
    min_width: request.screen?.min_width ?? DEFAULT_SCREEN_BOUNDS.min_width,
    max_width: request.screen?.max_width ?? DEFAULT_SCREEN_BOUNDS.max_width,
    min_height: request.screen?.min_height ?? DEFAULT_SCREEN_BOUNDS.min_height,
    max_height: request.screen?.max_height ?? DEFAULT_SCREEN_BOUNDS.max_height,
  };
  // The generator's own types describe more than is read here.
  let drawn: unknown;
  try {
    drawn = withSeededMathRandom(request.seed, 'fingerprint-generator', () => {
      generator ??= new FingerprintGenerator();
      return generator.getFingerprint({
        browsers: [{ name: 'chrome' }],
        operatingSystems: [request.claimed_os],
        devices: ['desktop'],
        locales: [...request.languages],
        screen: {
          minWidth: bounds.min_width,
          maxWidth: bounds.max_width,
          minHeight: bounds.min_height,
          maxHeight: bounds.max_height,
        },
        strict: true,
      });
    });
  } catch (error) {
    throw new EngineError(
      'invalid_request',
      `no fingerprint satisfies the constraints: ${error instanceof Error ? error.message : String(error)}`,
      { cause: error },
    );
  }
  const generated = drawn as Generated;
  const { navigator, screen } = generated.fingerprint;
  return {
    schema: FINGERPRINT_SCHEMA,
    engine: 'chromium',
    generator: GENERATOR_ID,
    seed: request.seed,
    claimed_os: request.claimed_os,
    browser_version: facts.browserVersion,
    user_agent: userAgentFor(request.claimed_os, major, navigator.userAgent),
    platform: request.claimed_os === 'linux' ? 'Linux x86_64' : navigator.platform,
    languages: [...request.languages],
    native: {
      // A persona's display is an X display: one device pixel per CSS pixel
      // and 24 bits, whatever the generator drew.
      screen: { width: screen.width, height: screen.height, dpr: 1, color_depth: 24 },
      hardware_concurrency: Math.max(1, Math.min(256, Math.trunc(navigator.hardwareConcurrency))),
    },
    host_bound: [...HOST_BOUND],
    inject: request.claimed_os === 'linux' ? 'none' : 'window',
    // A plain JSON copy: what is stored is what was generated.
    generated: JSON.parse(JSON.stringify(generated)) as Record<string, unknown>,
  };
}

/** What a cross-OS persona overrides besides what its script does. */
export interface Injection {
  /** The script that runs in every document before the page's own. */
  readonly script: string;
  /** SHA-256 of the script; the same document always gives the same value. */
  readonly sha256: string;
  /** Parameters of the browser's own user-agent override. */
  readonly userAgentOverride: {
    readonly userAgent: string;
    readonly platform: string;
    readonly userAgentMetadata: {
      readonly brands: { brand: string; version: string }[];
      readonly fullVersionList: { brand: string; version: string }[];
      readonly platform: string;
      readonly platformVersion: string;
      readonly architecture: string;
      readonly model: string;
      readonly mobile: boolean;
      readonly bitness: string;
      readonly wow64: boolean;
    };
  };
}

const text = (value: unknown, fallback: string): string =>
  typeof value === 'string' ? value : fallback;

/** The statement of the injector's script that fakes `history.length`. */
const HISTORY_OVERRIDE =
  /overrideInstancePrototype\(window\.history,\s*\{\s*length:\s*historyLength,?\s*\}\);/;

/**
 * Builds what a persona with `inject: window` applies: one script from the
 * persisted document, and the parameters of the browser's own user-agent
 * override, so that headers and scripts name the same browser.
 *
 * Only a persona that claims another operating system than the host's uses
 * this path, and only when the engine allows such claims; it is known to be
 * contradicted by workers (ADR 0009). Of fingerprint-injector only the
 * script is used. Its Playwright attachment is not: it rewrites headers
 * through the debugger, forces a dark color scheme and delivers the script
 * in a way that switches the HTTP cache off. The script's fake
 * `history.length`, a fresh random number per attach, is taken out.
 *
 * @throws {EngineError} `internal` when the injector's script no longer has
 *   the shape this function edits: a bump of the pin must be looked at.
 */
export function buildInjection(document: FingerprintDocument): Injection {
  const major = majorOfVersion(document.browser_version) ?? 0;
  const generated = structuredClone(document.generated) as unknown as Generated;
  const { navigator, screen } = generated.fingerprint;
  const brands = chromeBrands(major);
  const fullVersionList = brands.map((entry) => ({
    brand: entry.brand,
    version: entry.brand.startsWith('Not') ? `${entry.version}.0.0.0` : document.browser_version,
  }));
  const generatedHints: Record<string, unknown> = navigator.userAgentData ?? {};
  const metadata = {
    ...generatedHints,
    brands,
    fullVersionList,
    uaFullVersion: document.browser_version,
    platform: hintPlatformOf(document.claimed_os),
    mobile: false,
  };
  navigator.userAgent = document.user_agent;
  navigator['appVersion'] = document.user_agent.replace(/^Mozilla\//, '');
  navigator.platform = document.platform;
  navigator['language'] = document.languages[0];
  navigator['languages'] = document.languages;
  navigator.userAgentData = metadata;
  screen.width = document.native.screen.width;
  screen.height = document.native.screen.height;
  screen['devicePixelRatio'] = document.native.screen.dpr;

  // The injector adds one random number of its own; the seed makes even
  // that reproducible before the statement that uses it is removed.
  const raw = withSeededMathRandom(document.seed, 'fingerprint-injector', () =>
    new FingerprintInjector().getInjectableScript(generated as never),
  );
  if (!HISTORY_OVERRIDE.test(raw)) {
    throw new EngineError(
      'internal',
      'fingerprint-injector changed the script it emits: review buildInjection before using this version',
    );
  }
  const script = raw.replace(HISTORY_OVERRIDE, '').replace(/"historyLength":\d+,?/, '');
  return {
    script,
    sha256: createHash('sha256').update(script).digest('hex'),
    userAgentOverride: {
      userAgent: document.user_agent,
      platform: document.platform,
      userAgentMetadata: {
        brands,
        fullVersionList,
        platform: metadata.platform,
        platformVersion: text(generatedHints['platformVersion'], ''),
        architecture: text(generatedHints['architecture'], 'x86'),
        model: '',
        mobile: false,
        bitness: text(generatedHints['bitness'], '64'),
        wow64: false,
      },
    },
  };
}
