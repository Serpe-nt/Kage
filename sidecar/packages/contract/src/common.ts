import { z } from 'zod';

/**
 * Rules every schema of the contract follows, because the OpenAPI document
 * is emitted from them and Rust types are generated from that document:
 *
 * - every object is a `strictObject` with an `id` and a description, and
 *   every property has a description (they become doc comments in Rust);
 * - no `.default()` and no transform: what a client sends and what a server
 *   reads have one shape. An optional field's default is applied in code
 *   and named in the description;
 * - field names are `snake_case`, enum values are lower case.
 *
 * `openapi.test.ts` enforces all three.
 */

/** Version of the sidecar HTTP contract. It is also the path prefix. */
export const CONTRACT_VERSION = 'v1';

/**
 * Semantic version of the contract document. A breaking change needs a new
 * major here and a new path prefix; `breaking.ts` checks it.
 */
export const CONTRACT_SEMVER = '1.3.0';

/** Header that carries the shared secret (spec §10.1). */
export const TOKEN_HEADER = 'x-kage-sidecar-token';

/** Browser engines the sidecar can run. One container runs exactly one. */
export const EngineNameSchema = z.enum(['chromium', 'firefox']).meta({
  id: 'EngineName',
  description: 'A browser engine. One sidecar container runs exactly one.',
});
export type EngineName = z.infer<typeof EngineNameSchema>;

/** The operating system a persona claims to run. */
export const ClaimedOsSchema = z.enum(['windows', 'macos', 'linux']).meta({
  id: 'ClaimedOs',
  description: 'The operating system a persona claims to run.',
});
export type ClaimedOs = z.infer<typeof ClaimedOsSchema>;

/** Stable error codes. A new code is an addition; a code never changes meaning. */
export const ErrorCodeSchema = z
  .enum([
    'unauthorized',
    'invalid_request',
    'not_found',
    'context_not_found',
    'context_limit',
    'context_conflict',
    'context_busy',
    'cross_os_claim_disabled',
    'browser_major_mismatch',
    'browser_launch_failed',
    'not_implemented',
    'payload_too_large',
    'internal',
  ])
  .meta({
    id: 'ErrorCode',
    description:
      'Stable error code of a refused or failed request. `unauthorized`: the shared secret is missing or wrong. `invalid_request`: the body, a parameter or the URL does not match the contract. `not_found`: no such route. `context_not_found`: no open context has this id. `context_limit`: the container already runs its maximum number of contexts. `context_conflict`: the persona has an open context that was opened with different parameters. `context_busy`: the context is running another operation. `cross_os_claim_disabled`: the engine may only claim the operating system it runs on. `browser_major_mismatch`: the request names a browser major the engine does not bundle. `browser_launch_failed`: the browser or its display did not start. `not_implemented`: the engine does not implement this endpoint yet. `payload_too_large`: the request body is over the limit. `internal`: an unexpected failure, see the logs.',
  });
export type ErrorCode = z.infer<typeof ErrorCodeSchema>;

/** Body of every non-2xx answer. */
export const ErrorResponseSchema = z
  .strictObject({
    code: ErrorCodeSchema.describe('What went wrong, as a stable code.'),
    message: z.string().describe('A sentence for the log. Never parse it.'),
  })
  .meta({ id: 'ErrorResponse', description: 'Body of every answer that is not a success.' });
export type ErrorResponse = z.infer<typeof ErrorResponseSchema>;

/** The screen a persona reports. Same fields as `kage_core::domain::Screen`. */
export const ScreenSchema = z
  .strictObject({
    width: z.number().int().min(640).max(7680).describe('Width in CSS pixels.'),
    height: z.number().int().min(480).max(4320).describe('Height in CSS pixels.'),
    dpr: z.number().min(1).max(4).describe('Device pixel ratio.'),
    color_depth: z.number().int().min(24).max(32).describe('Colour depth in bits.'),
  })
  .meta({ id: 'Screen', description: 'The screen a persona reports.' });
export type Screen = z.infer<typeof ScreenSchema>;

/** A point near the proxy's city. Same fields as `kage_core::domain::Geolocation`. */
export const GeolocationSchema = z
  .strictObject({
    latitude: z.number().min(-90).max(90).describe('Degrees north.'),
    longitude: z.number().min(-180).max(180).describe('Degrees east.'),
    accuracy_m: z.number().min(0).max(100_000).describe('Reported accuracy radius in metres.'),
  })
  .meta({
    id: 'Geolocation',
    description: 'A point the geolocation API reports once a site has been given permission.',
  });
export type Geolocation = z.infer<typeof GeolocationSchema>;

/** The authenticated proxy of one persona. */
export const ProxySchema = z
  .strictObject({
    server: z
      .string()
      .regex(/^http:\/\/[A-Za-z0-9.-]+:\d{1,5}$/)
      .describe('The proxy as `http://host:port`. HTTP CONNECT only.'),
    username: z.string().min(1).max(256).describe('Proxy user name. Never logged.'),
    password: z.string().min(1).max(256).describe('Proxy password. Never logged.'),
  })
  .meta({
    id: 'Proxy',
    description: 'The authenticated proxy every request of a context leaves through.',
  });
export type Proxy = z.infer<typeof ProxySchema>;

/** One header as it was received. A list keeps order and repeated names. */
export const HeaderSchema = z
  .strictObject({
    name: z.string().describe('Header name, lower case.'),
    value: z.string().describe('Header value.'),
  })
  .meta({ id: 'Header', description: 'One response header.' });
export type Header = z.infer<typeof HeaderSchema>;

/** A file the sidecar wrote to the shared artifacts volume. */
export const ArtifactRefSchema = z
  .strictObject({
    path: z
      .string()
      .describe('Path of the file relative to the artifacts volume, with forward slashes.'),
    bytes: z.number().int().nonnegative().describe('Size of the file in bytes.'),
    sha256: z
      .string()
      .regex(/^[0-9a-f]{64}$/)
      .describe('SHA-256 of the file, hexadecimal.'),
    content_type: z.string().describe('Media type of the file.'),
  })
  .meta({
    id: 'ArtifactRef',
    description: 'A file the sidecar wrote to the artifacts volume it shares with Kage.',
  });
export type ArtifactRef = z.infer<typeof ArtifactRefSchema>;

/** A UUID as Kage mints them (version 7), in the usual hyphenated form. */
export const uuid = (description: string) => z.uuid().describe(description);

/** An RFC 3339 timestamp in UTC. */
export const timestamp = (description: string) => z.iso.datetime().describe(description);

/** A BCP 47 language tag such as `de-DE` or `en`. */
export const languageTag = (description: string) =>
  z
    .string()
    .regex(/^[A-Za-z]{2,3}(-[A-Za-z0-9]{2,8})*$/)
    .describe(description);
