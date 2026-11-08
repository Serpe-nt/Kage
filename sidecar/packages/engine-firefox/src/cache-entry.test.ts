import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import {
  brotliCompressSync,
  deflateRawSync,
  deflateSync,
  gzipSync,
  zstdCompressSync,
} from 'node:zlib';

import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { cacheKeys, decoded, entryFileName, keptBody, readEntry } from './cache-entry.js';

/**
 * An entry as Firefox's HTTP cache writes one (`CacheFileMetadata`,
 * version 4): the data, the metadata (a hash, a 16-bit hash per 256 kB
 * chunk, the header, the key and its NUL, the elements), the offset of the
 * metadata.
 */
function entry(key: string, data: Buffer, version = 4): Buffer {
  const chunks = Math.ceil(data.length / (256 * 1024));
  const header = Buffer.alloc(32);
  header.writeUInt32BE(version, 0);
  header.writeUInt32BE(1, 4);
  header.writeUInt32BE(Buffer.byteLength(key, 'latin1'), 24);
  const metadata = Buffer.concat([
    Buffer.alloc(4 + chunks * 2),
    header,
    Buffer.from(`${key}\0`, 'latin1'),
    Buffer.from('response-head\0HTTP/1.1 200 OK\r\n\0', 'latin1'),
  ]);
  const offset = Buffer.alloc(4);
  offset.writeUInt32BE(data.length, 0);
  return Buffer.concat([data, metadata, offset]);
}

const page = 'http://qa.kage.test:8091/b05483494/firefox';
const html = Buffer.from(
  '<html><head><title>A kept page</title></head><body><h1>A kept page</h1></body></html>',
);

let dir = '';
beforeAll(async () => {
  dir = await mkdtemp(path.join(tmpdir(), 'kage-cache-'));
});
afterAll(async () => {
  await rm(dir, { recursive: true, force: true });
});

const put = async (key: string, bytes: Buffer) => {
  await writeFile(path.join(dir, entryFileName(key)), bytes);
};

describe('the HTTP cache of a Firefox persona (browser-05)', () => {
  it('names an entry as Camoufox 156 does', () => {
    // Seen in a persona's profile after a level 2 page of the QA world.
    const key = 'O^partitionKey=%28http%2Ckage.test%29,:http://qa.kage.test:8091/b05483494/firefox';
    expect(entryFileName(key)).toBe('F666FEAC338500F71325C9F8E03497FA70C5C3C8');
    expect(cacheKeys(`${page}#top`)).toEqual([
      'O^partitionKey=%28http%2Cqa.kage.test%29,:http://qa.kage.test:8091/b05483494/firefox',
      key,
      'O^partitionKey=%28http%2Ctest%29,:http://qa.kage.test:8091/b05483494/firefox',
      ':http://qa.kage.test:8091/b05483494/firefox',
    ]);
    expect(cacheKeys('https://127.0.0.1/a(b)')).toEqual([
      'O^partitionKey=%28https%2C127.0.0.1%29,:https://127.0.0.1/a(b)',
      ':https://127.0.0.1/a(b)',
    ]);
    expect(cacheKeys('not a url')).toEqual([]);
  });

  it('reads the data of the entry that holds the key, and no other', async () => {
    const key = cacheKeys(page)[1] ?? '';
    await put(key, entry(key, html));
    const file = path.join(dir, entryFileName(key));
    expect(await readEntry(file, key, 1000)).toEqual({ data: html, length: html.length });
    expect((await readEntry(file, key, 10))?.data).toEqual(html.subarray(0, 10));
    expect(await readEntry(file, `${key}x`, 1000)).toBeNull();
    expect(await readEntry(path.join(dir, 'missing'), key, 1000)).toBeNull();
    // Not an entry of this format: too short, an offset past the end, an
    // unknown version.
    const odd = path.join(dir, 'odd');
    for (const bytes of [
      Buffer.from([1, 2]),
      Buffer.from([0, 0, 0, 0, 0, 0, 0, 9]),
      entry(key, html, 9),
    ]) {
      await writeFile(odd, bytes);
      expect(await readEntry(odd, key, 1000)).toBeNull();
    }
  });

  it('gives the kept page as the tab shows it: decoded, whole, or nothing', async () => {
    const head = (lines: Record<string, string>) =>
      Object.entries(lines).map(([name, value]) => ({ name, value }));
    const at = (name: string) => `http://qa.kage.test:8091/kept/${name}`;
    const keyOf = (url: string) => cacheKeys(url)[1] ?? '';
    const cases: [string, Buffer, Record<string, string>][] = [
      ['plain', html, { 'Content-Type': 'text/html', 'Content-Length': String(html.length) }],
      ['gzip', gzipSync(html), { 'Content-Encoding': 'gzip' }],
      ['br', brotliCompressSync(html), { 'content-encoding': 'br' }],
      ['deflate', deflateSync(html), { 'Content-Encoding': 'deflate' }],
      ['raw-deflate', deflateRawSync(html), { 'Content-Encoding': 'deflate' }],
      ['zstd', zstdCompressSync(html), { 'Content-Encoding': 'zstd' }],
      ['both', brotliCompressSync(gzipSync(html)), { 'Content-Encoding': 'gzip, br' }],
    ];
    for (const [name, data, lines] of cases) {
      await put(keyOf(at(name)), entry(keyOf(at(name)), data));
      expect(await keptBody(dir, at(name), head(lines)), name).toEqual(html);
    }
    // An entry that is not the whole page, a coding it cannot undo, a page
    // the cache does not hold.
    await put(keyOf(at('short')), entry(keyOf(at('short')), html.subarray(0, 20)));
    expect(
      await keptBody(dir, at('short'), head({ 'Content-Length': String(html.length) })),
    ).toBeNull();
    await put(keyOf(at('dict')), entry(keyOf(at('dict')), html));
    expect(await keptBody(dir, at('dict'), head({ 'Content-Encoding': 'dcb' }))).toBeNull();
    expect(await keptBody(dir, at('gone'), [])).toBeNull();
    expect(await keptBody(path.join(dir, 'nowhere'), at('plain'), [])).toBeNull();
  });

  it('undoes what the site applied, last first', () => {
    expect(decoded(html, null)).toEqual(html);
    expect(decoded(html, 'identity')).toEqual(html);
    expect(decoded(gzipSync(html), 'x-gzip')).toEqual(html);
    expect(decoded(Buffer.from('not gzip'), 'gzip')).toBeNull();
  });
});
