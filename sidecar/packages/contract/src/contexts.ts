import { z } from 'zod';

import {
  ClaimedOsSchema,
  GeolocationSchema,
  ProxySchema,
  ScreenSchema,
  languageTag,
  timestamp,
  uuid,
} from './common.js';
import { GlModeSchema, InjectModeSchema } from './personas.js';

/** An IANA time zone name such as `Europe/Berlin`. */
const timezone = (description: string) =>
  z
    .string()
    .regex(/^[A-Za-z][A-Za-z0-9_+-]*(\/[A-Za-z0-9_+-]+){0,2}$/)
    .max(64)
    .describe(description);

/** Body of `POST /v1/contexts`. */
export const OpenContextRequestSchema = z
  .strictObject({
    persona_id: uuid('The persona. One persona has at most one open context.'),
    fingerprint: z
      .record(z.string(), z.unknown())
      .describe('The fingerprint document exactly as `POST /v1/personas/generate` returned it.'),
    proxy: ProxySchema.describe(
      "The persona's proxy. Every request of the context leaves through it.",
    ),
    locale: languageTag("The language of the browser's own interface, for example `de-DE`."),
    languages: z
      .array(languageTag('A BCP 47 language tag.'))
      .min(1)
      .max(8)
      .describe('`navigator.languages` and `Accept-Language`, most preferred first.'),
    timezone: timezone("The persona's IANA time zone."),
    geolocation: GeolocationSchema.nullable().describe(
      'The point the geolocation API reports once a site has permission; null to report none. The permission itself is never granted automatically.',
    ),
    screen: ScreenSchema.describe("The persona's screen. It must equal the fingerprint's."),
    profile_dir: z
      .string()
      .regex(/^[A-Za-z0-9][A-Za-z0-9._-]*(\/[A-Za-z0-9][A-Za-z0-9._-]*)*$/)
      .max(200)
      .describe(
        "The persona's browser profile directory, relative to the personas volume. Created when missing; cookies, storage and history accrue in it.",
      ),
  })
  .meta({ id: 'OpenContextRequest', description: 'Everything a persona context is opened with.' });
export type OpenContextRequest = z.infer<typeof OpenContextRequestSchema>;

/** What the engine really applied when it opened a context. */
export const AppliedIdentitySchema = z
  .strictObject({
    claimed_os: ClaimedOsSchema.describe('The operating system the context presents.'),
    user_agent: z.string().describe('The `User-Agent` the context sends.'),
    screen: ScreenSchema.describe("The screen of the context's own display."),
    timezone: z.string().describe('The time zone the browser process runs in.'),
    languages: z.array(z.string()).describe('The languages written into the profile.'),
    hardware_concurrency: z
      .number()
      .int()
      .positive()
      .describe("Logical processors the browser sees: the fingerprint's, at most the host's."),
    gl: GlModeSchema.describe('Where WebGL comes from in this context.'),
    inject: InjectModeSchema.describe('How the fingerprint reaches pages in this context.'),
    script_sha256: z
      .string()
      .regex(/^[0-9a-f]{64}$/)
      .nullable()
      .describe(
        'SHA-256 of the injected script; null when nothing is injected. The same fingerprint always gives the same value.',
      ),
  })
  .meta({
    id: 'AppliedIdentity',
    description: 'What the engine applied when it opened a context.',
  });
export type AppliedIdentity = z.infer<typeof AppliedIdentitySchema>;

/** Response of `POST /v1/contexts`. */
export const OpenContextResponseSchema = z
  .strictObject({
    context_id: uuid('The context. It names the context in every later call.'),
    persona_id: uuid('The persona the context belongs to.'),
    reused: z
      .boolean()
      .describe(
        'True when the persona already had an open context with the same parameters and that one is returned.',
      ),
    browser_version: z.string().describe('Full version of the browser that runs the context.'),
    applied: AppliedIdentitySchema.describe('What the engine applied.'),
    opened_at: timestamp('When the context was opened.'),
  })
  .meta({ id: 'OpenContextResponse', description: 'An open persona context.' });
export type OpenContextResponse = z.infer<typeof OpenContextResponseSchema>;

/** Path parameters of every `/v1/contexts/{id}` route. */
export const ContextParamsSchema = z.strictObject({
  id: uuid('The context id returned by `POST /v1/contexts`.'),
});
export type ContextParams = z.infer<typeof ContextParamsSchema>;
