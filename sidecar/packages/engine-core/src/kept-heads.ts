import type { ShownHead } from './session.js';

/** One header line. */
interface Line {
  readonly name: string;
  readonly value: string;
}

/** The head of a document's answer as the browser kept it with the page. */
export interface KeptHead {
  /** The document's address, without its fragment. */
  readonly url: string;
  readonly status: number;
  readonly headers: readonly Line[];
  /** When it was last answered, in milliseconds of the clock. */
  readonly at: number;
}

/** A proxy's request for credentials, which the browser answers and asks again. */
const PROXY_CHALLENGE = 407;

/** How many heads a persona keeps at most: the ones last answered stay. */
export const KEPT_HEADS_LIMIT = 300;

/**
 * How long a head is kept after its page was last answered: 30 days. The
 * browser drops a cached page by the room its cache needs, not by age, and
 * one a persona has not asked for in a month is rarely still there; where
 * it is, the head that was dropped costs only the wire's `304`, never a
 * wrong answer.
 */
export const KEPT_HEADS_AGE_MS = 30 * 24 * 60 * 60 * 1000;

/**
 * Fields no stored head holds (RFC 9111 §3.1) and none a `304` updates
 * (§3.2): what belongs to one connection, and `Content-Length`. Nor does it
 * hold what sets state: `Set-Cookie` belongs to the answer that carried it
 * (Chrome keeps its cache without it, too).
 */
const NOT_KEPT = new Set([
  'connection',
  'keep-alive',
  'proxy-connection',
  'te',
  'transfer-encoding',
  'upgrade',
  'set-cookie',
  'set-cookie2',
]);
const NOT_UPDATED = new Set([...NOT_KEPT, 'content-length']);

const withoutFragment = (url: string) => url.split('#')[0] ?? '';

const named = (lines: readonly Line[], name: string) =>
  lines.some((line) => line.name.toLowerCase() === name);

/**
 * The heads of the documents a persona's browser has kept with a validator,
 * so that a page it revalidates is reported as the page it shows (F178).
 * Firefox's driver hands over only the wire's `304` and its few fields; the
 * page the tab shows is the kept answer with those fields brought up to
 * date (RFC 9111 §3.2), which is what Chrome's debugger announces. Bounded
 * by {@link KEPT_HEADS_LIMIT} and {@link KEPT_HEADS_AGE_MS}; kept in the
 * persona's profile, next to the browser's own cache that holds the same.
 */
export class KeptHeads {
  /** By address, the one answered last at the end. */
  private readonly heads = new Map<string, KeptHead>();

  constructor(
    heads: readonly KeptHead[] = [],
    private readonly limit = KEPT_HEADS_LIMIT,
    private readonly ageMs = KEPT_HEADS_AGE_MS,
  ) {
    for (const head of [...heads].sort((a, b) => a.at - b.at)) {
      this.put(head);
    }
  }

  /**
   * Reads what {@link KeptHeads.serialize} wrote. Anything else, or nothing,
   * is no heads at all: the cost is the wire's `304`, never a failure.
   */
  static parse(text: string | null): KeptHeads {
    if (text === null) {
      return new KeptHeads();
    }
    try {
      const parsed = JSON.parse(text) as { heads?: unknown };
      const heads = Array.isArray(parsed.heads) ? parsed.heads.filter(isKeptHead) : [];
      return new KeptHeads(heads);
    } catch {
      return new KeptHeads();
    }
  }

  /** What is kept, as a file holds it, without the heads older than the bound. */
  serialize(now: number): string {
    this.forget(now);
    return `${JSON.stringify({ version: 1, heads: [...this.heads.values()] })}\n`;
  }

  /** How many heads are kept. */
  get size(): number {
    return this.heads.size;
  }

  /**
   * Notes a document's answer as the browser keeps it: a page with a
   * validator (`ETag`, `Last-Modified`) is kept with its head; a `304`
   * brings a kept head up to date; a redirect or a page without a
   * validator is nothing the browser revalidates, and a proxy's `407` is
   * no answer at all.
   */
  note(url: string, status: number, headers: readonly Line[], now: number): void {
    if (status === PROXY_CHALLENGE) {
      // The proxy asks for the persona's credentials: no answer of the site.
      return;
    }
    if (status === 304) {
      const kept = this.shown(url, headers, now);
      if (kept !== null) {
        this.put({
          url: withoutFragment(url),
          status: kept.status,
          headers: kept.headers,
          at: now,
        });
      }
      return;
    }
    if (status >= 300 && status < 400) {
      return;
    }
    const lines = headers.filter((line) => !NOT_KEPT.has(line.name.toLowerCase()));
    if (!named(lines, 'etag') && !named(lines, 'last-modified')) {
      this.heads.delete(withoutFragment(url));
      return;
    }
    this.put({ url: withoutFragment(url), status, headers: lines, at: now });
  }

  /**
   * The page the tab shows for a `304` at `url`: the kept status, and the
   * kept head with every field of the 304 put in place of the kept one of
   * that name, or added (RFC 9111 §3.2). `null` when no head of it is kept.
   */
  shown(url: string, headers: readonly Line[], now: number): ShownHead | null {
    this.forget(now);
    const kept = this.heads.get(withoutFragment(url));
    if (kept === undefined) {
      return null;
    }
    const fresh = headers.filter((line) => !NOT_UPDATED.has(line.name.toLowerCase()));
    const replaced = new Set(fresh.map((line) => line.name.toLowerCase()));
    const merged: Line[] = [];
    const placed = new Set<string>();
    for (const line of kept.headers) {
      const name = line.name.toLowerCase();
      if (!replaced.has(name)) {
        merged.push(line);
      } else if (!placed.has(name)) {
        placed.add(name);
        merged.push(...fresh.filter((other) => other.name.toLowerCase() === name));
      }
    }
    merged.push(...fresh.filter((line) => !placed.has(line.name.toLowerCase())));
    return { status: kept.status, headers: merged };
  }

  private put(head: KeptHead): void {
    this.heads.delete(head.url);
    this.heads.set(head.url, head);
    while (this.heads.size > this.limit) {
      const oldest = this.heads.keys().next().value;
      if (oldest === undefined) {
        break;
      }
      this.heads.delete(oldest);
    }
  }

  private forget(now: number): void {
    for (const [url, head] of this.heads) {
      if (now - head.at > this.ageMs) {
        this.heads.delete(url);
      }
    }
  }
}

function isKeptHead(value: unknown): value is KeptHead {
  if (typeof value !== 'object' || value === null) {
    return false;
  }
  const head = value as Partial<Record<keyof KeptHead, unknown>>;
  return (
    typeof head.url === 'string' &&
    typeof head.status === 'number' &&
    typeof head.at === 'number' &&
    Array.isArray(head.headers) &&
    head.headers.every(
      (line: unknown) =>
        typeof line === 'object' &&
        line !== null &&
        typeof (line as Partial<Line>).name === 'string' &&
        typeof (line as Partial<Line>).value === 'string',
    )
  );
}
