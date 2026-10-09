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
  /** The installed browser's full version when the fingerprint was generated; never the generator's. */
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
