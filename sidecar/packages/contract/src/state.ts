import { z } from 'zod';

import { timestamp } from './common.js';

/** A web origin: scheme, host and optional port, nothing else. */
const origin = (description: string) =>
  z
    .string()
    .max(300)
    .regex(/^https?:\/\/[A-Za-z0-9.-]+(:\d{1,5})?$/)
    .describe(description);

/** The `SameSite` attribute of a cookie. */
export const SameSiteSchema = z.enum(['strict', 'lax', 'none']).meta({
  id: 'SameSite',
  description: 'The `SameSite` attribute of a cookie.',
});
export type SameSite = z.infer<typeof SameSiteSchema>;

/**
 * One cookie. The fields are the ones of a row of Kage's `http_cookies`
 * table (`kage_core::domain::HttpCookie`), so that state moves between the
 * browser and the http path without a mapping that could lose something.
 * The two bookkeeping times of a row are Kage's own and do not cross.
 */
export const CookieSchema = z
  .strictObject({
    domain: z
      .string()
      .min(1)
      .max(255)
      .describe(
        'The host that set the cookie, or its `Domain` attribute. Lower case, no leading dot.',
      ),
    host_only: z
      .boolean()
      .describe('True when the cookie had no `Domain` attribute and goes to exactly `domain`.'),
    path: z.string().min(1).max(2048).describe('The path the cookie is sent under.'),
    name: z.string().max(4096).describe("The cookie's name."),
    value: z.string().max(4096).describe("The cookie's value."),
    secure: z.boolean().describe('Sent only to secure origins.'),
    http_only: z.boolean().describe('Hidden from scripts.'),
    same_site: SameSiteSchema.nullable().describe(
      'The `SameSite` attribute; null when the cookie carried none, which browsers treat as `lax`.',
    ),
    expires_at: timestamp('When the cookie expires.')
      .nullable()
      .describe('When the cookie expires; null for a session cookie.'),
  })
  .meta({
    id: 'Cookie',
    description: "One cookie, with the fields of a row of Kage's `http_cookies` table.",
  });
export type Cookie = z.infer<typeof CookieSchema>;

/** One key of a web storage area. */
export const StorageEntrySchema = z
  .strictObject({
    name: z.string().describe('The key.'),
    value: z.string().describe('The value.'),
  })
  .meta({ id: 'StorageEntry', description: 'One key of a web storage area.' });
export type StorageEntry = z.infer<typeof StorageEntrySchema>;

/** Body of `POST /v1/contexts/{id}/state/export`. */
export const ExportStateRequestSchema = z
  .strictObject({
    origin: origin('The origin whose state is exported, for example `https://example.org`.'),
    include_storage: z
      .boolean()
      .optional()
      .describe('Also export `localStorage` and `sessionStorage`; default false.'),
  })
  .meta({ id: 'ExportStateRequest', description: 'Which state to export from a context.' });
export type ExportStateRequest = z.infer<typeof ExportStateRequestSchema>;

/** Response of `POST /v1/contexts/{id}/state/export`. */
export const ExportStateResponseSchema = z
  .strictObject({
    origin: z.string().describe('The origin that was asked for.'),
    cookies: z
      .array(CookieSchema)
      .describe('Every cookie the browser would send to that origin, session cookies included.'),
    local_storage: z
      .array(StorageEntrySchema)
      .nullable()
      .describe(
        "The origin's `localStorage`; null when not requested, or when the context's page is on another origin and the storage cannot be read without loading something.",
      ),
    session_storage: z
      .array(StorageEntrySchema)
      .nullable()
      .describe("The origin's `sessionStorage`, under the same conditions."),
  })
  .meta({ id: 'ExportStateResponse', description: 'Cookies and storage of one origin.' });
export type ExportStateResponse = z.infer<typeof ExportStateResponseSchema>;

/** Body of `POST /v1/contexts/{id}/state/import`. */
export const ImportStateRequestSchema = z
  .strictObject({
    origin: origin('The origin the cookies belong to. A cookie for another site is refused.'),
    cookies: z.array(CookieSchema).max(3000).describe('The cookies to set.'),
  })
  .meta({ id: 'ImportStateRequest', description: 'Cookies to put into a context.' });
export type ImportStateRequest = z.infer<typeof ImportStateRequestSchema>;

/** Why a cookie was not imported. */
export const CookieRejectionSchema = z
  .enum(['foreign_domain', 'expired', 'refused_by_browser'])
  .meta({
    id: 'CookieRejection',
    description:
      "Why a cookie was not imported. `foreign_domain`: it does not belong to the origin. `expired`: its expiry is in the past. `refused_by_browser`: the browser's own cookie rules refused it.",
  });
export type CookieRejection = z.infer<typeof CookieRejectionSchema>;

/** A cookie that was not imported. */
export const RejectedCookieSchema = z
  .strictObject({
    name: z.string().describe("The cookie's name."),
    domain: z.string().describe("The cookie's domain."),
    reason: CookieRejectionSchema.describe('Why it was refused.'),
  })
  .meta({ id: 'RejectedCookie', description: 'A cookie that was not imported.' });
export type RejectedCookie = z.infer<typeof RejectedCookieSchema>;

/** Response of `POST /v1/contexts/{id}/state/import`. */
export const ImportStateResponseSchema = z
  .strictObject({
    imported: z.number().int().nonnegative().describe('Cookies now in the browser.'),
    rejected: z.array(RejectedCookieSchema).describe('Cookies that were refused, with the reason.'),
  })
  .meta({ id: 'ImportStateResponse', description: 'What an import did.' });
export type ImportStateResponse = z.infer<typeof ImportStateResponseSchema>;
