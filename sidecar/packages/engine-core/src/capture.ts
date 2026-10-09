import type { CapturedBody, Header, XhrCapture, XhrKind, XhrResponse } from '@kage/contract';

/** Default of `XhrCapture.max_responses`. */
export const DEFAULT_MAX_XHR_RESPONSES = 100;

/** The media type of a `Content-Type` value: lower case, without parameters. */
export function mediaType(contentType: string | null | undefined): string | null {
  const type = contentType?.split(';')[0]?.trim().toLowerCase();
  return type === undefined || type === '' ? null : type;
}

/**
 * Whether a response's media type is one a capture filter asks for.
 *
 * A pattern is compared without parameters and without case. Three forms:
 * an exact type (`application/json`), a prefix (`text/*`, and `*\/*` for
 * everything), and a structured-syntax suffix (`+json` matches
 * `application/ld+json`).
 */
export function matchesContentType(
  contentType: string | null | undefined,
  patterns: readonly string[],
): boolean {
  const type = mediaType(contentType);
  if (type === null) {
    return false;
  }
  return patterns.some((raw) => {
    const pattern = raw.trim().toLowerCase();
    if (pattern === '*/*') {
      return true;
    }
    if (pattern.endsWith('/*')) {
      return type.startsWith(pattern.slice(0, -1));
    }
    if (pattern.startsWith('+')) {
      return type.endsWith(pattern);
    }
    return type === pattern;
  });
}

const utf8 = new TextDecoder('utf-8', { fatal: true });

function decodeUtf8(bytes: Uint8Array): string | null {
  try {
    return utf8.decode(bytes);
  } catch {
    return null;
  }
}

/**
 * Turns a response body into its contract form: at most `maxBytes` of it,
 * as text when it is valid UTF-8 and as base64 otherwise.
 *
 * A cut can fall inside a multi-byte character; up to three trailing bytes
 * are then dropped so that a text body stays text.
 */
export function captureBody(body: Uint8Array, maxBytes: number): CapturedBody {
  const truncated = body.byteLength > maxBytes;
  const kept = truncated ? body.subarray(0, maxBytes) : body;
  const attempts = truncated ? 4 : 1;
  for (let trim = 0; trim < attempts && trim <= kept.byteLength; trim += 1) {
    const text = decodeUtf8(kept.subarray(0, kept.byteLength - trim));
    if (text !== null) {
      return { bytes: body.byteLength, truncated, encoding: 'utf8', data: text };
    }
  }
  return {
    bytes: body.byteLength,
    truncated,
    encoding: 'base64',
    data: Buffer.from(kept).toString('base64'),
  };
}

/** A text cut to at most `maxBytes` of UTF-8, never inside a character. */
export function truncateText(text: string, maxBytes: number): { text: string; truncated: boolean } {
  const bytes = Buffer.from(text, 'utf8');
  if (bytes.byteLength <= maxBytes) {
    return { text, truncated: false };
  }
  let end = maxBytes;
  // Step back over continuation bytes (10xxxxxx) to a character boundary.
  while (end > 0 && ((bytes[end] ?? 0) & 0xc0) === 0x80) {
    end -= 1;
  }
  return { text: bytes.subarray(0, end).toString('utf8'), truncated: true };
}

/** Response headers in contract form: order kept, names in lower case. */
export function toHeaders(headers: readonly { name: string; value: string }[]): Header[] {
  return headers.map(({ name, value }) => ({ name: name.toLowerCase(), value }));
}

/** The first value of a header, compared without case. */
export function headerValue(headers: readonly Header[], name: string): string | null {
  const wanted = name.toLowerCase();
  return headers.find((header) => header.name.toLowerCase() === wanted)?.value ?? null;
}

/** The host of a URL, lower case; `null` for anything without one. */
export function hostOf(url: string): string | null {
  try {
    const { hostname } = new URL(url);
    return hostname === '' ? null : hostname.toLowerCase();
  } catch {
    return null;
  }
}

/** The origin of an `http` or `https` URL; `null` for anything else. */
export function httpOrigin(url: string): string | null {
  try {
    const parsed = new URL(url);
    return parsed.protocol === 'http:' || parsed.protocol === 'https:' ? parsed.origin : null;
  } catch {
    return null;
  }
}

/** One XHR or fetch() response as an engine saw it. */
export interface ObservedXhr {
  readonly url: string;
  readonly method: string;
  readonly kind: XhrKind;
  readonly status: number;
  readonly contentType: string | null;
  /** The body; `null` when the browser no longer held it. */
  readonly body: Uint8Array | null;
}

/**
 * Applies a capture filter to the XHR and fetch() responses of one page:
 * which ones are kept, how much of each body, and how many were dropped
 * because the page had more than the filter allows.
 */
export class XhrCollector {
  private readonly kept: XhrResponse[] = [];
  private droppedCount = 0;
  private readonly limit: number;

  /** @param filter the request's filter; `null` keeps nothing. */
  constructor(private readonly filter: XhrCapture | null) {
    this.limit = filter?.max_responses ?? DEFAULT_MAX_XHR_RESPONSES;
  }

  /**
   * Whether a response with this media type is wanted and there is still
   * room for it. An engine asks before it reads a body, so a body nobody
   * wants is never fetched. A wanted response without room counts as dropped.
   */
  wants(contentType: string | null | undefined): boolean {
    if (this.filter === null || !matchesContentType(contentType, this.filter.content_types)) {
      return false;
    }
    if (this.kept.length >= this.limit) {
      this.droppedCount += 1;
      return false;
    }
    return true;
  }

  /** Keeps a response that {@link wants} accepted. */
  add(observed: ObservedXhr): void {
    if (this.filter === null || this.kept.length >= this.limit) {
      this.droppedCount += 1;
      return;
    }
    this.kept.push({
      url: observed.url,
      method: observed.method,
      kind: observed.kind,
      status: observed.status,
      content_type: mediaType(observed.contentType),
      body: observed.body === null ? null : captureBody(observed.body, this.filter.max_bytes),
    });
  }

  /** The kept responses, in the order they were added. */
  responses(): XhrResponse[] {
    return [...this.kept];
  }

  /** Matching responses that were not kept. */
  get dropped(): number {
    return this.droppedCount;
  }
}
