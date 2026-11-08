import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { open } from 'node:fs/promises';
import path from 'node:path';
import type { Transform } from 'node:stream';
import {
  createBrotliDecompress,
  createGunzip,
  createInflate,
  createInflateRaw,
  createZstdDecompress,
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

/**
 * The most metadata read of an entry besides its chunk hashes: Firefox's own
 * bound on an entry's metadata is far below it (its key, the response head
 * and a few elements).
 */
const MAX_METADATA = 1024 * 1024;

/**
 * The keys under which Firefox's HTTP cache may hold the top-level document
 * at `url`: partitioned by the site of the document itself (network
 * partitioning, `O^partitionKey=(scheme,site),:url`), the site being one of
 * the host's own suffixes, longest first; and unpartitioned. `url` is the
 * address as Firefox gave it (the driver's), used as it is but for its
 * fragment: Firefox's spelling of an address is not always the one a URL
 * parser here writes (a quote in a query). Each key is looked up by its file
 * name and confirmed by the key the entry holds. A host whose partition key
 * Firefox writes otherwise (an IP literal with a port of its own, a host
 * with no dot, an IPv6 literal) finds no entry.
 */
export function cacheKeys(url: string): string[] {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return [];
  }
  const address = url.split('#')[0] ?? url;
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
 * The length of the data of the cache entry in `file` when it holds `key`;
 * `null` when the file is not there, is not an entry of this format, or
 * holds another key. An entry is its data, then its metadata, then the
 * offset of the metadata (32 bits, big-endian), which is the data's length;
 * the metadata is a hash, one 16-bit hash per chunk of data, then its
 * header (version, fetch count, last fetched, last modified, frecency,
 * expiration, key size, flags), then the key. Every read is checked for
 * what it got, and the metadata is read only up to a bound.
 */
export async function entryLength(file: string, key: string): Promise<number | null> {
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
    if ((await handle.read(tail, 0, 4, size - 4)).bytesRead !== 4) {
      return null;
    }
    const length = tail.readUInt32BE(0);
    if (length > size - 4) {
      return null;
    }
    const header = 4 + Math.ceil(length / CHUNK) * 2;
    const metaLength = size - 4 - length;
    if (metaLength < header + 32 || metaLength > header + MAX_METADATA) {
      return null;
    }
    const meta = Buffer.alloc(metaLength);
    if ((await handle.read(meta, 0, metaLength, length)).bytesRead !== metaLength) {
      return null;
    }
    const keySize = meta.readUInt32BE(header + 24);
    if (
      !VERSIONS.has(meta.readUInt32BE(header)) ||
      metaLength < header + 32 + keySize ||
      meta.toString('latin1', header + 32, header + 32 + keySize) !== key
    ) {
      return null;
    }
    return length;
  } finally {
    await handle.close();
  }
}

/** The content codings undone, the ones Firefox asks for (`gzip, deflate, br, zstd`). */
type Coding = 'gzip' | 'deflate' | 'deflate-raw' | 'br' | 'zstd';

/** The codings of a `Content-Encoding`, last applied first; `null` for one this cannot undo. */
export function codingsOf(encoding: string | null): Coding[] | null {
  const codings: Coding[] = [];
  for (const raw of (encoding ?? '').split(',').reverse()) {
    const coding = raw.trim().toLowerCase();
    if (coding === '' || coding === 'identity') {
      continue;
    }
    if (coding === 'gzip' || coding === 'x-gzip') {
      codings.push('gzip');
    } else if (coding === 'deflate' || coding === 'br' || coding === 'zstd') {
      codings.push(coding);
    } else {
      // A compression dictionary's (`dcb`, `dcz`) or any other.
      return null;
    }
  }
  return codings;
}

const decoder = (coding: Coding): Transform => {
  switch (coding) {
    case 'gzip':
      return createGunzip();
    case 'deflate':
      return createInflate();
    case 'deflate-raw':
      return createInflateRaw();
    case 'br':
      return createBrotliDecompress();
    case 'zstd':
      return createZstdDecompress();
  }
};

/**
 * The first `limit` bytes of the entry's data (`length` bytes at the start
 * of `file`) with `codings` undone, read and decoded as a stream, off the
 * event loop's critical path; reading stops once `limit` bytes came out.
 * Rejects where the data does not decode.
 */
function streamed(
  file: string,
  length: number,
  codings: readonly Coding[],
  limit: number,
): Promise<Buffer> {
  return new Promise<Buffer>((resolve, reject) => {
    if (length === 0) {
      resolve(Buffer.alloc(0));
      return;
    }
    const stages: NodeJS.ReadableStream[] = [createReadStream(file, { start: 0, end: length - 1 })];
    for (const coding of codings) {
      const next = decoder(coding);
      const last = stages[stages.length - 1];
      last?.pipe(next);
      stages.push(next);
    }
    const chunks: Buffer[] = [];
    let got = 0;
    let done = false;
    const stop = (error: Error | null) => {
      if (done) {
        return;
      }
      done = true;
      for (const stage of stages) {
        (stage as NodeJS.ReadableStream & { destroy(): void }).destroy();
      }
      if (error === null) {
        resolve(Buffer.concat(chunks).subarray(0, limit));
      } else {
        reject(error);
      }
    };
    for (const stage of stages) {
      stage.on('error', (error: Error) => {
        stop(error);
      });
    }
    const out = stages[stages.length - 1];
    out?.on('data', (chunk: Buffer) => {
      chunks.push(chunk);
      got += chunk.length;
      if (got >= limit) {
        stop(null);
      }
    });
    out?.on('end', () => {
      stop(null);
    });
  });
}

const field = (lines: readonly Line[], name: string) =>
  lines.find((line) => line.name.toLowerCase() === name)?.value ?? null;

/**
 * The body of the page Firefox kept for `url` and shows where it
 * revalidated it (QA round three, browser-05): its driver gives only the
 * wire's `304`, which has none, and refuses a body for any `3xx`. Read
 * from the persona's own HTTP cache (`cache2/entries` of its profile), as
 * Firefox serves it to the tab, never through the page. `head` is the kept
 * head of the page: its `Content-Encoding` is undone (a `deflate` body as
 * zlib's format, else raw), and an entry whose length is not its
 * `Content-Length` is not the whole page and gives nothing. At most `limit`
 * bytes come back: the capture cuts at its bound and says so when it got
 * more. `null` wherever the entry cannot be read as the page.
 */
export async function keptBody(
  entries: string,
  url: string,
  head: readonly Line[],
  limit: number,
): Promise<Buffer | null> {
  const codings = codingsOf(field(head, 'content-encoding'));
  if (codings === null) {
    return null;
  }
  const declared = field(head, 'content-length');
  for (const key of cacheKeys(url)) {
    const file = path.join(entries, entryFileName(key));
    const length = await entryLength(file, key);
    if (length === null) {
      continue;
    }
    if (declared !== null && /^\d+$/.test(declared) && Number(declared) !== length) {
      return null;
    }
    const body = await streamed(file, length, codings, limit).catch(async () =>
      codings.includes('deflate')
        ? await streamed(
            file,
            length,
            codings.map((coding) => (coding === 'deflate' ? 'deflate-raw' : coding)),
            limit,
          ).catch(() => null)
        : null,
    );
    return body;
  }
  return null;
}
