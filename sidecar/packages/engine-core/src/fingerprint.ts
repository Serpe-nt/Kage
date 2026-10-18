import {
  ClaimedOsSchema,
  EngineNameSchema,
  InjectModeSchema,
  ScreenSchema,
  type PersonaSummary,
} from '@kage/contract';
import { z } from 'zod';

import { EngineError } from './engine.js';
import { majorOfVersion } from './identity.js';

/** Version of the persisted fingerprint document. */
export const FINGERPRINT_SCHEMA = 1;

/**
 * The persisted fingerprint of a persona (spec §6.1 step 3).
 *
 * Kage stores it as `personas.fingerprint` and sends it back on every
 * context open; only the sidecar reads it. It says what the persona is
 * (`claimed_os`, `browser_version`, `user_agent`), what launch parameters
 * make true (`native`), what stays the host's whatever the persona
 * (`host_bound`), and keeps the generator's full output (`generated`) for
 * the record and for the injection path.
 */
export const FingerprintDocumentSchema = z.strictObject({
  schema: z.literal(FINGERPRINT_SCHEMA),
  engine: EngineNameSchema,
  /** Name and version of the generator, for example `fingerprint-generator@2.1.88`. */
  generator: z.string().min(1),
  seed: z.string().regex(/^[0-9a-f]{64}$/),
  claimed_os: ClaimedOsSchema,
  /**
   * The installed browser's full version when the fingerprint was generated,
   * or last moved to a new browser ({@link moveToVersion}); never the generator's.
   */
  browser_version: z.string().regex(/^\d+(\.\d+){1,3}$/),
  user_agent: z.string().min(1),
  platform: z.string().min(1),
  languages: z.array(z.string().min(2)).min(1).max(8),
  /** What launch parameters apply: these values are true in window, frames and workers. */
  native: z.strictObject({
    screen: ScreenSchema,
    hardware_concurrency: z.number().int().min(1).max(256),
  }),
  /** Values every persona on one host shares, because nothing short of other hardware changes them. */
  host_bound: z.array(z.string()),
  inject: InjectModeSchema,
  /** The generator's output, unchanged. */
  generated: z.record(z.string(), z.unknown()),
});
export type FingerprintDocument = z.infer<typeof FingerprintDocumentSchema>;

/**
 * Reads a fingerprint document Kage sent.
 *
 * @throws {EngineError} `invalid_request`, naming what is wrong.
 */
export function parseFingerprint(value: unknown): FingerprintDocument {
  const parsed = FingerprintDocumentSchema.safeParse(value);
  if (!parsed.success) {
    const problems = parsed.error.issues
      .slice(0, 5)
      .map((issue) => `${issue.path.join('.') || 'fingerprint'}: ${issue.message}`)
      .join('; ');
    throw new EngineError(
      'invalid_request',
      `not a fingerprint document of this sidecar: ${problems}`,
    );
  }
  return parsed.data;
}

/**
 * Moves a fingerprint document to the browser an engine runs, as a
 * browser's own update moves a person's (contract 1.4.0, spec §6.2): the
 * browser's version and its `User-Agent` change; the device, the seeds,
 * the screen and everything the generator drew stay as they were, and so
 * does the profile the document belongs to.
 *
 * Deterministic: the same document and version give the same result, and a
 * document of that version comes back unchanged (`upgraded` false).
 *
 * @param rewrite turns the document's `User-Agent` of one major into the
 *   same browser's of another; it throws when it does not recognize it.
 * @throws {EngineError} `browser_major_mismatch` when the document is of a
 *   newer major than the engine's (a browser does not go back), `internal`
 *   when either version has no major.
 */
export function moveToVersion(
  document: FingerprintDocument,
  version: string,
  rewrite: (userAgent: string, from: number, to: number) => string,
): { document: FingerprintDocument; upgraded: boolean } {
  const from = majorOfVersion(document.browser_version);
  const to = majorOfVersion(version);
  if (from === null || to === null) {
    throw new EngineError(
      'internal',
      `no browser major in ${document.browser_version} or in ${version}`,
    );
  }
  if (from > to) {
    throw new EngineError(
      'browser_major_mismatch',
      `the fingerprint is of browser ${document.browser_version}, newer than the engine's ${version}: a browser does not go back to an older major`,
    );
  }
  if (document.browser_version === version) {
    return { document, upgraded: false };
  }
  return {
    document: {
      ...document,
      browser_version: version,
      user_agent: from === to ? document.user_agent : rewrite(document.user_agent, from, to),
    },
    upgraded: true,
  };
}

/** The summary of a fingerprint, as `POST /v1/personas/generate` returns it. */
export function summarize(document: FingerprintDocument): PersonaSummary {
  return {
    engine: document.engine,
    claimed_os: document.claimed_os,
    browser_version: document.browser_version,
    browser_major: majorOfVersion(document.browser_version) ?? 0,
    user_agent: document.user_agent,
    platform: document.platform,
    screen: document.native.screen,
    hardware_concurrency: document.native.hardware_concurrency,
    languages: document.languages,
    generator: document.generator,
    seed: document.seed,
    inject: document.inject,
  };
}
