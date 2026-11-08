import { randomBytes } from 'node:crypto';
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

import { cacheKeys, codingsOf, entryFileName, entryLength, keptBody } from './cache-entry.js';

/**
 * An entry as Firefox's HTTP cache writes one (`CacheFileMetadata`,
 * version 4): the data, the metadata (a hash, a 16-bit hash per 256 kB
 * chunk, the header, the key and its NUL, the elements), the offset of the
 * metadata.
 */
function entry(key: string, data: Buffer, version = 4, elements = 32): Buffer {
  const chunks = Math.ceil(data.length / (256 * 1024));
  const header = Buffer.alloc(32);
  header.writeUInt32BE(version, 0);
  header.writeUInt32BE(1, 4);
  header.writeUInt32BE(Buffer.byteLength(key, 'latin1'), 24);
  const metadata = Buffer.concat([
    Buffer.alloc(4 + chunks * 2),
    header,
    Buffer.from(`${key}\0`, 'latin1'),
    Buffer.alloc(elements, 0x61),
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
const head = (lines: Record<string, string>) =>
  Object.entries(lines).map(([name, value]) => ({ name, value }));
const at = (name: string) => `http://qa.kage.test:8091/kept/${name}`;
const keyOf = (url: string) => cacheKeys(url)[1] ?? '';

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

  it('keys an address in Firefox’s own spelling, which a URL parser here would change', () => {
    // Firefox keeps a quote in a query as it is; Node's parser writes %27.
    const quoted = "http://qa.kage.test:8091/kept/q?a='b'&c=d e";
    expect(new URL(quoted).href).not.toBe(quoted);
    expect(cacheKeys(`${quoted}#frag#more`)[1]).toBe(
      `O^partitionKey=%28http%2Ckage.test%29,:${quoted}`,
    );
  });

  it('reads the length of the entry that holds the key, and no other', async () => {
    const key = cacheKeys(page)[1] ?? '';
    await put(key, entry(key, html));
    const file = path.join(dir, entryFileName(key));
    expect(await entryLength(file, key)).toBe(html.length);
    expect(await entryLength(file, `${key}x`)).toBeNull();
    expect(await entryLength(path.join(dir, 'missing'), key)).toBeNull();
    // Not an entry of this format: too short, an offset past the end, an
    // unknown version, a key longer than its metadata, metadata past the
    // bound read.
    const odd = path.join(dir, 'odd');
    const longKey = entry(key, html);
    longKey.writeUInt32BE(1_000_000, html.length + 4 + 2 + 24);
    for (const bytes of [
      Buffer.from([1, 2]),
      Buffer.from([0, 0, 0, 0, 0, 0, 0, 9]),
      entry(key, html, 9),
      longKey,
      entry(key, html, 4, 1024 * 1024 + 1),
    ]) {
      await writeFile(odd, bytes);
      expect(await entryLength(odd, key)).toBeNull();
    }
    // Metadata up to the bound is read.
    await writeFile(odd, entry(key, html, 4, 1000));
    expect(await entryLength(odd, key)).toBe(html.length);
  });

  it('gives the kept page as the tab shows it: decoded, whole, or nothing', async () => {
    const cases: [string, Buffer, Record<string, string>][] = [
      ['plain', html, { 'Content-Type': 'text/html', 'Content-Length': String(html.length) }],
      ['gzip', gzipSync(html), { 'Content-Encoding': 'gzip' }],
      ['br', brotliCompressSync(html), { 'content-encoding': 'br' }],
      ['deflate', deflateSync(html), { 'Content-Encoding': 'deflate' }],
      // zlib's format with a small window: its first byte is no 0x78.
      ['deflate-9', deflateSync(html, { windowBits: 9 }), { 'Content-Encoding': 'deflate' }],
      ['raw-deflate', deflateRawSync(html), { 'Content-Encoding': 'deflate' }],
      ['zstd', zstdCompressSync(html), { 'Content-Encoding': 'zstd' }],
      ['both', brotliCompressSync(gzipSync(html)), { 'Content-Encoding': 'gzip, br' }],
      ['x-gzip', gzipSync(html), { 'Content-Encoding': 'x-gzip, identity' }],
    ];
    for (const [name, data, lines] of cases) {
      await put(keyOf(at(name)), entry(keyOf(at(name)), data));
      expect(await keptBody(dir, at(name), head(lines), 10_000), name).toEqual(html);
    }
    // An entry that is not the whole page, a coding it cannot undo, a body
    // that does not decode, a page the cache does not hold.
    await put(keyOf(at('short')), entry(keyOf(at('short')), html.subarray(0, 20)));
    expect(
      await keptBody(dir, at('short'), head({ 'Content-Length': String(html.length) }), 10_000),
    ).toBeNull();
    await put(keyOf(at('dict')), entry(keyOf(at('dict')), html));
    for (const coding of ['dcb', 'dcz']) {
      expect(await keptBody(dir, at('dict'), head({ 'Content-Encoding': coding }), 10_000)).toBe(
        null,
      );
    }
    expect(await keptBody(dir, at('dict'), head({ 'Content-Encoding': 'gzip' }), 10_000)).toBe(
      null,
    );
    expect(await keptBody(dir, at('gone'), [], 10_000)).toBeNull();
    expect(await keptBody(path.join(dir, 'nowhere'), at('plain'), [], 10_000)).toBeNull();
    const empty = keyOf(at('empty'));
    await put(empty, entry(empty, Buffer.alloc(0)));
    expect(await keptBody(dir, at('empty'), [], 10_000)).toEqual(Buffer.alloc(0));
  });

  it('decodes no more than the request keeps', async () => {
    // A page that inflates far past the bound: decoding stops there.
    const large = randomBytes(64 * 1024).toString('hex');
    const packed = gzipSync(Buffer.from(large.repeat(20)));
    await put(keyOf(at('large')), entry(keyOf(at('large')), packed));
    const cut = await keptBody(dir, at('large'), head({ 'Content-Encoding': 'gzip' }), 1001);
    expect(cut).toEqual(Buffer.from(large.slice(0, 1001)));
    const plain = await keptBody(dir, at('plain'), [], 10);
    expect(plain).toEqual(html.subarray(0, 10));
  });

  it('reads the codings last applied first, and none it cannot undo', () => {
    expect(codingsOf(null)).toEqual([]);
    expect(codingsOf('identity')).toEqual([]);
    expect(codingsOf('gzip, br')).toEqual(['br', 'gzip']);
    expect(codingsOf('X-Gzip')).toEqual(['gzip']);
    expect(codingsOf('gzip, dcz')).toBeNull();
  });
});
