import type { Cookie, CookieRejection, SameSite } from '@kage/contract';

/**
 * Cookies between the contract's form (a row of Kage's `http_cookies`
 * table) and the form browsers use in their debugging protocols.
 */

/** A cookie as a browser's protocol reports it (CDP `Network.Cookie`, Playwright's `Cookie`). */
export interface BrowserCookie {
  readonly name: string;
  readonly value: string;
  /** With a leading dot for a cookie that had a `Domain` attribute. */
  readonly domain: string;
  readonly path: string;
  /** Seconds since the epoch; `-1` or absent for a session cookie. */
  readonly expires?: number;
  readonly httpOnly: boolean;
  readonly secure: boolean;
  /** `true` for a session cookie, when the protocol says so. */
  readonly session?: boolean;
  /** Absent when the cookie carried no `SameSite` attribute. */
  readonly sameSite?: string;
}

/** A cookie as a browser's protocol takes it (CDP `Network.CookieParam`). */
export interface BrowserCookieParam {
  name: string;
  value: string;
  /** Present for a host-only cookie: the browser derives the host from it. */
  url?: string;
  /** Present for a cookie with a `Domain` attribute. */
  domain?: string;
  path: string;
  secure: boolean;
  httpOnly: boolean;
  sameSite?: 'Strict' | 'Lax' | 'None';
  /** Seconds since the epoch; absent for a session cookie. */
  expires?: number;
}

const SAME_SITE_IN: Readonly<Record<string, SameSite>> = {
  strict: 'strict',
  lax: 'lax',
  none: 'none',
};
const SAME_SITE_OUT = { strict: 'Strict', lax: 'Lax', none: 'None' } as const;

/** A browser's cookie in contract form. */
export function toContractCookie(cookie: BrowserCookie): Cookie {
  const session = cookie.session === true || cookie.expires === undefined || cookie.expires < 0;
  return {
    domain: cookie.domain.replace(/^\./, '').toLowerCase(),
    host_only: !cookie.domain.startsWith('.'),
    path: cookie.path === '' ? '/' : cookie.path,
    name: cookie.name,
    value: cookie.value,
    secure: cookie.secure,
    http_only: cookie.httpOnly,
    same_site: SAME_SITE_IN[cookie.sameSite?.toLowerCase() ?? ''] ?? null,
    expires_at: session ? null : new Date(Math.round((cookie.expires ?? 0) * 1000)).toISOString(),
  };
}

/** Host and scheme of an origin such as `https://example.org:8443`. */
function parseOrigin(origin: string): { host: string; secure: boolean } {
  const url = new URL(origin);
  return { host: url.hostname.toLowerCase(), secure: url.protocol === 'https:' };
}

/** Whether a host is the cookie's domain or, for a domain cookie, below it (RFC 6265 §5.1.3). */
function domainMatches(host: string, cookie: Pick<Cookie, 'domain' | 'host_only'>): boolean {
  const domain = cookie.domain.toLowerCase();
  if (host === domain) {
    return true;
  }
  return !cookie.host_only && host.endsWith(`.${domain}`);
}

/**
 * The cookies a browser would send to an origin, whatever the path: the
 * host matches, a `Secure` cookie only goes to a secure origin, and an
 * expired cookie is not sent.
 */
export function cookiesForOrigin(cookies: readonly Cookie[], origin: string, now: Date): Cookie[] {
  const { host, secure } = parseOrigin(origin);
  return cookies.filter(
    (cookie) =>
      domainMatches(host, cookie) &&
      (secure || !cookie.secure) &&
      (cookie.expires_at === null || new Date(cookie.expires_at).getTime() > now.getTime()),
  );
}

/**
 * Why a cookie may not be imported for an origin, or `null` when it may.
 *
 * A cookie belongs to the origin when the origin's host is the cookie's
 * domain or lies below it. A cookie for a parent domain (`example.org` for
 * the origin `https://shop.example.org`) belongs; one for a sibling or for
 * another site does not.
 */
export function importRejection(cookie: Cookie, origin: string, now: Date): CookieRejection | null {
  const { host } = parseOrigin(origin);
  if (!domainMatches(host, cookie)) {
    return 'foreign_domain';
  }
  if (cookie.expires_at !== null && new Date(cookie.expires_at).getTime() <= now.getTime()) {
    return 'expired';
  }
  return null;
}

/** A contract cookie in the form a browser's protocol takes. */
export function toBrowserCookieParam(cookie: Cookie): BrowserCookieParam {
  const param: BrowserCookieParam = {
    name: cookie.name,
    value: cookie.value,
    path: cookie.path,
    secure: cookie.secure,
    httpOnly: cookie.http_only,
  };
  if (cookie.host_only) {
    // No domain: the browser takes the host from the URL and keeps the
    // cookie host-only, as a response without a Domain attribute does.
    param.url = `${cookie.secure ? 'https' : 'http'}://${cookie.domain}${cookie.path}`;
  } else {
    param.domain = `.${cookie.domain}`;
  }
  if (cookie.same_site !== null) {
    param.sameSite = SAME_SITE_OUT[cookie.same_site];
  }
  if (cookie.expires_at !== null) {
    param.expires = new Date(cookie.expires_at).getTime() / 1000;
  }
  return param;
}

/** The identity of a cookie in a jar: two cookies with the same key are the same cookie. */
export function cookieKey(cookie: Pick<Cookie, 'domain' | 'host_only' | 'path' | 'name'>): string {
  return [cookie.domain.toLowerCase(), cookie.host_only ? 'h' : 'd', cookie.path, cookie.name].join(
    '\u0000',
  );
}
