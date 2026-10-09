import { z } from 'zod';

import { ClaimedOsSchema, EngineNameSchema, ScreenSchema, languageTag } from './common.js';

/** A seed as Kage records it: 256 bits, 64 hexadecimal characters. */
export const SeedSchema = z
  .string()
  .regex(/^[0-9a-f]{64}$/)
  .describe('A recorded seed: 64 lower-case hexadecimal characters (256 bits).');

/**
 * How a persisted fingerprint reaches a page.
 *
 * `none`: nothing is injected; the persona is what the browser natively is,
 * shaped by launch parameters only. `window`: one script, built from the
 * persisted fingerprint without fresh randomness, runs in every document
 * before the page's own scripts. Only a persona that claims another
 * operating system than the host's uses it, and only when the engine allows
 * such claims.
 */
export const InjectModeSchema = z.enum(['none', 'window']).meta({
  id: 'InjectMode',
  description:
    'How a persisted fingerprint reaches a page. `none`: nothing is injected, launch parameters make the persona. `window`: one script built from the fingerprint runs in every document before the page (cross-OS claims only).',
});
export type InjectMode = z.infer<typeof InjectModeSchema>;

/** How the browser gets its graphics stack. */
export const GlModeSchema = z.enum(['none', 'swiftshader', 'llvmpipe', 'host']).meta({
  id: 'GlMode',
  description:
    "Where WebGL comes from. `none`: no WebGL. `swiftshader`: Chrome's bundled software renderer. `llvmpipe`: Mesa's software renderer through ANGLE. `host`: a GPU of the host mapped into the container.",
});
export type GlMode = z.infer<typeof GlModeSchema>;

/** Bounds for the screen the generator may pick. */
export const ScreenConstraintsSchema = z
  .strictObject({
    min_width: z
      .number()
      .int()
      .min(640)
      .max(7680)
      .optional()
      .describe('Smallest width in CSS pixels.'),
    max_width: z
      .number()
      .int()
      .min(640)
      .max(7680)
      .optional()
      .describe('Largest width in CSS pixels.'),
    min_height: z
      .number()
      .int()
      .min(480)
      .max(4320)
      .optional()
      .describe('Smallest height in CSS pixels.'),
    max_height: z
      .number()
      .int()
      .min(480)
      .max(4320)
      .optional()
      .describe('Largest height in CSS pixels.'),
  })
  .meta({
    id: 'ScreenConstraints',
    description: 'Bounds for the screen of a generated persona. A missing bound is not applied.',
  });
export type ScreenConstraints = z.infer<typeof ScreenConstraintsSchema>;

/** Body of `POST /v1/personas/generate`. */
export const GeneratePersonaRequestSchema = z
  .strictObject({
    seed: SeedSchema.describe(
      "The persona's recorded seed. The same seed, constraints and sidecar build give the same fingerprint, byte for byte.",
    ),
    claimed_os: ClaimedOsSchema.describe(
      "The operating system the persona claims. An engine that may not claim another system than its host's answers `cross_os_claim_disabled`.",
    ),
    languages: z
      .array(languageTag('A BCP 47 language tag.'))
      .min(1)
      .max(8)
      .describe("The persona's languages, most preferred first; the first is its locale."),
    browser_major: z
      .number()
      .int()
      .positive()
      .optional()
      .describe(
        "The browser major Kage expects the engine to bundle. When given and different from the installed browser's, the answer is `browser_major_mismatch`.",
      ),
    screen: ScreenConstraintsSchema.optional().describe('Bounds for the generated screen.'),
  })
  .meta({
    id: 'GeneratePersonaRequest',
    description: 'Constraints for a new persona fingerprint.',
  });
export type GeneratePersonaRequest = z.infer<typeof GeneratePersonaRequestSchema>;

/** What a fingerprint says in a few fields, for logs and the persona table. */
export const PersonaSummarySchema = z
  .strictObject({
    engine: EngineNameSchema.describe('The engine that generated the fingerprint.'),
    claimed_os: ClaimedOsSchema.describe('The operating system the persona claims.'),
    browser_version: z.string().describe('Full version of the installed browser.'),
    browser_major: z.number().int().positive().describe('Major version of the installed browser.'),
    user_agent: z.string().describe('The `User-Agent` the persona presents.'),
    platform: z.string().describe('`navigator.platform` as the persona presents it.'),
    screen: ScreenSchema.describe('The screen the persona reports.'),
    hardware_concurrency: z
      .number()
      .int()
      .positive()
      .describe(
        "Logical processors the persona would report. A context reports at most the host's count.",
      ),
    languages: z.array(z.string()).describe("The persona's languages, most preferred first."),
    generator: z
      .string()
      .describe('Name and version of the generator, for example `fingerprint-generator@2.1.88`.'),
    seed: SeedSchema.describe('The seed the fingerprint was generated from.'),
    inject: InjectModeSchema.describe('How the fingerprint will reach pages.'),
  })
  .meta({ id: 'PersonaSummary', description: 'What a fingerprint says, in a few fields.' });
export type PersonaSummary = z.infer<typeof PersonaSummarySchema>;

/** Response of `POST /v1/personas/generate`. */
export const GeneratePersonaResponseSchema = z
  .strictObject({
    fingerprint: z
      .record(z.string(), z.unknown())
      .describe(
        'The fingerprint document. Kage stores it unchanged and sends it back on every context open; only the sidecar reads it.',
      ),
    summary: PersonaSummarySchema.describe('The same fingerprint in a few fields.'),
  })
  .meta({ id: 'GeneratePersonaResponse', description: 'A generated persona fingerprint.' });
export type GeneratePersonaResponse = z.infer<typeof GeneratePersonaResponseSchema>;
