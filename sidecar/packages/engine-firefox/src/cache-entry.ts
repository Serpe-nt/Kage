import { createHash } from 'node:crypto';
import { open } from 'node:fs/promises';
import path from 'node:path';
import {
  brotliDecompressSync,
  gunzipSync,
  inflateRawSync,
  inflateSync,
  zstdDecompressSync,
} from 'node:zlib';

/** One header line. */
interface Line {
  readonly name: string;
  readonly value: string;
}

/** The size of a chunk of an entry's data: its metadata holds a hash of each (`kChunkSize`). */
const CHUNK = 256 * 1024;

/** The metadata versions whose header this reads: Firefox 156 writes 4. */
const VERSIONS = new Set([2, 3, 4]);

/** The largest body read out of the cache: the contract's largest `max_body_bytes`. */
export const MAX_KEPT_BODY = 100_000_000;

/**
 * The keys under which Firefox's HTTP cache may hold the top-level document
 * at `url`: partitioned by the site of the document itself (network
 * partitioning, `O^partitionKey=(scheme,site),:url`), the site being one of
 * the host's own suffixes, longest first; and unpartitioned. Each is looked
 * up by its file name and confirmed by the key the entry holds.
 */
export function cacheKeys(url: string): string[] {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return [];
  }
  parsed.hash = '';
  const address = parsed.href;
  const scheme = parsed.protocol.replace(/:$/, '');
  const labels = parsed.hostname.split('.');
  const numeric = /^[\d.]+$/.test(parsed.hostname) || parsed.hostname.startsWith('[');
  const sites = numeric
    ? [parsed.hostname]
    : labels.map((_, index) => labels.slice(index).join('.')).filter((site) => site !== '');
  return [
    ...sites.map((site) => `O^partitionKey=%28${scheme}%2C${site}%29,:${address}`),
    `:${address}`,
  ];
}

/** The file of an entry: the SHA-1 of its key, in upper-case hex (`CacheFileUtils`). */
export function entryFileName(key: string): string {
  return createHash('sha1').update(key, 'latin1').digest('hex').toUpperCase();
}

/**
 * The data of the cache entry in `file` when it holds `key`, at most
 * `limit` bytes of it, and its whole length; `null` when the file is not
 * there, is not an entry of this format, or holds another key. An entry is
 * its data, then its metadata, then the offset of the metadata (32 bits,
 * big-endian); the metadata is a hash, one 16-bit hash per chunk of data,
 * then its header (version, fetch count, last fetched, last modified,
 * frecency, expiration, key size, flags), then the key.
 */
export async function readEntry(
  file: string,
  key: string,
  limit: number,
): Promise<{ data: Buffer; length: number } | null> {
  const handle = await open(file, 'r').catch(() => null);
  if (handle === null) {
    return null;
  }
  try {
    const { size } = await handle.stat();
    if (size < 4) {
      return null;
    }
    const tail = Buffer.alloc(4);
    await handle.read(tail, 0, 4, size - 4);
    const length = tail.readUInt32BE(0);
    if (length > size - 4) {
      return null;
    }
    const meta = Buffer.alloc(size - 4 - length);
    await handle.read(meta, 0, meta.length, length);
    const header = 4 + Math.ceil(length / CHUNK) * 2;
    if (meta.length < header + 32) {
      return null;
    }
    const keySize = meta.readUInt32BE(header + 24);
    if (
      !VERSIONS.has(meta.readUInt32BE(header)) ||
      meta.length < header + 32 + keySize ||
      meta.toString('latin1', header + 32, header + 32 + keySize) !== key
    ) {
      return null;
    }
    const data = Buffer.alloc(Math.min(length, limit));
    await handle.read(data, 0, data.length, 0);
    return { data, length };
  } finally {
    await handle.close();
  }
}

/**
 * Undoes the content codings of a kept body, last applied first, the ones
 * Firefox asks for (`gzip, deflate, br, zstd`); `null` for any other (a
 * compression dictionary's) or a body that does not decode.
 */
export function decoded(data: Buffer, encoding: string | null): Buffer | null {
  const codings = (encoding ?? '')
    .split(',')
    .map((coding) => coding.trim().toLowerCase())
    .filter((coding) => coding !== '' && coding !== 'identity');
  let body = data;
  const bound = { maxOutputLength: MAX_KEPT_BODY };
  try {
    for (const coding of codings.reverse()) {
      if (coding === 'gzip' || coding === 'x-gzip') {
        body = gunzipSync(body, bound);
      } else if (coding === 'deflate') {
        // A `deflate` body is zlib's format, though some servers send it raw.
        body = body[0] === 0x78 ? inflateSync(body, bound) : inflateRawSync(body, bound);
      } else if (coding === 'br') {
        body = brotliDecompressSync(body, bound);
      } else if (coding === 'zstd') {
        body = zstdDecompressSync(body, bound);
      } else {
        return null;
      }
    }
  } catch {
    return null;
  }
  return body;
}

const field = (lines: readonly Line[], name: string) =>
  lines.find((line) => line.name.toLowerCase() === name)?.value ?? null;

/**
 * The body of the page Firefox kept for `url` and shows where it
 * revalidated it (QA round three, browser-05): its driver gives only the
 * wire's `304`, which has none, and refuses a body for any `3xx`. Read
 * from the persona's own HTTP cache (`cache2/entries` of its profile), as
 * Firefox serves it to the tab, never through the page. `head` is the kept
 * head of the page: its `Content-Encoding` is undone, and an entry shorter
 * or longer than its `Content-Length` is not the whole page and gives
 * nothing. `null` wherever the entry cannot be read as the whole page.
 */
export async function keptBody(
  entries: string,
  url: string,
  head: readonly Line[],
): Promise<Buffer | null> {
  const encoding = field(head, 'content-encoding');
  const declared = field(head, 'content-length');
  // Read whole, to be decoded; the capture cuts it at its bound.
  for (const key of cacheKeys(url)) {
    const entry = await readEntry(path.join(entries, entryFileName(key)), key, MAX_KEPT_BODY);
    if (entry === null) {
      continue;
    }
    if (declared !== null && /^\d+$/.test(declared) && Number(declared) !== entry.length) {
      return null;
    }
    if (entry.data.length < entry.length) {
      return null;
    }
    return decoded(entry.data, encoding);
  }
  return null;
}
