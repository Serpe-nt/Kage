import { describe, expect, it } from 'vitest';

import { KEPT_HEADS_AGE_MS, KeptHeads } from './kept-heads.js';

const page = 'https://www.example.org/page';
const DAY = 24 * 60 * 60 * 1000;
const full = [
  { name: 'Content-Type', value: 'text/html; charset=utf-8' },
  { name: 'ETag', value: '"v1"' },
  { name: 'Cache-Control', value: 'no-cache' },
  { name: 'Set-Cookie', value: 'visit=1' },
  { name: 'Connection', value: 'keep-alive' },
  { name: 'Content-Length', value: '120' },
  { name: 'Date', value: 'Tue, 03 Nov 2026 20:00:00 GMT' },
];
const notModified = [
  { name: 'ETag', value: '"v1"' },
  { name: 'Cache-Control', value: 'no-cache, private' },
  { name: 'Date', value: 'Wed, 04 Nov 2026 09:00:00 GMT' },
  { name: 'Connection', value: 'close' },
  { name: 'Content-Length', value: '0' },
  { name: 'X-New', value: 'yes' },
];

describe('the heads of the pages a persona’s browser keeps (F178)', () => {
  it('shows a 304 as the kept page, its head brought up to date by the 304’s fields', () => {
    const heads = new KeptHeads();
    heads.note(`${page}#top`, 200, full, 0);
    expect(heads.shown(page, notModified, DAY)).toEqual({
      status: 200,
      headers: [
        { name: 'Content-Type', value: 'text/html; charset=utf-8' },
        { name: 'ETag', value: '"v1"' },
        { name: 'Cache-Control', value: 'no-cache, private' },
        // Kept as it was: the 304 does not update the length of the body.
        { name: 'Content-Length', value: '120' },
        { name: 'Date', value: 'Wed, 04 Nov 2026 09:00:00 GMT' },
        { name: 'X-New', value: 'yes' },
      ],
    });
    // A 304 brings the kept head up to date for the next time too.
    heads.note(page, 304, notModified, DAY);
    expect(heads.shown(page, [], 2 * DAY)?.headers).toContainEqual({
      name: 'X-New',
      value: 'yes',
    });
  });

  it('keeps the kept page’s type and validator where a 304 names others (browser-03)', () => {
    const heads = new KeptHeads();
    heads.note(page, 200, full, 0);
    const odd = [
      { name: 'Content-Type', value: 'text/plain' },
      { name: 'ETag', value: '"v2"' },
      { name: 'Content-Encoding', value: 'gzip' },
      { name: 'Content-Location', value: '/elsewhere' },
      { name: 'Content-Range', value: 'bytes 0-0/1' },
      { name: 'Content-MD5', value: 'x' },
      { name: 'Trailer', value: 'Expires' },
      { name: 'Proxy-Authenticate', value: 'Basic' },
      { name: 'Cache-Control', value: 'max-age=60' },
    ];
    const shown = heads.shown(page, odd, 1);
    expect(shown?.headers).toEqual([
      { name: 'Content-Type', value: 'text/html; charset=utf-8' },
      { name: 'ETag', value: '"v1"' },
      { name: 'Cache-Control', value: 'max-age=60' },
      { name: 'Content-Length', value: '120' },
      { name: 'Date', value: 'Tue, 03 Nov 2026 20:00:00 GMT' },
    ]);
    // And the next time: the kept head brought up to date keeps them too.
    heads.note(page, 304, odd, 1);
    expect(
      heads.shown(page, [], 2)?.headers.find((line) => line.name === 'Content-Type')?.value,
    ).toBe('text/html; charset=utf-8');
  });

  it('reads a value a file of an older sidecar joined with a line feed as its lines (browser-01)', () => {
    const heads = KeptHeads.parse(
      JSON.stringify({
        version: 1,
        heads: [
          {
            url: page,
            status: 401,
            at: 0,
            headers: [
              { name: 'ETag', value: '"a"' },
              { name: 'WWW-Authenticate', value: 'Negotiate\nNTLM' },
            ],
          },
        ],
      }),
    );
    expect(heads.shown(page, [], 0)?.headers).toEqual([
      { name: 'ETag', value: '"a"' },
      { name: 'WWW-Authenticate', value: 'Negotiate' },
      { name: 'WWW-Authenticate', value: 'NTLM' },
    ]);
  });

  it('keeps no state the page set and nothing of one connection', () => {
    const heads = new KeptHeads();
    heads.note(page, 200, full, 0);
    const names = heads.shown(page, [], 0)?.headers.map((line) => line.name.toLowerCase());
    expect(names).not.toContain('set-cookie');
    expect(names).not.toContain('connection');
    expect(JSON.parse(heads.serialize(0))).toMatchObject({ version: 1 });
    expect(heads.serialize(0)).not.toContain('visit=1');
  });

  it('knows nothing of a page it never saw, a redirect, or a page without a validator', () => {
    const heads = new KeptHeads();
    expect(heads.shown(page, notModified, 0)).toBeNull();
    heads.note(page, 302, [...full, { name: 'Location', value: '/next' }], 0);
    expect(heads.shown(page, notModified, 0)).toBeNull();
    heads.note(page, 200, [{ name: 'Content-Type', value: 'text/html' }], 0);
    expect(heads.shown(page, notModified, 0)).toBeNull();
    // A 304 of a page whose head is not kept stays unknown.
    heads.note(page, 304, notModified, 0);
    expect(heads.size).toBe(0);
    // A proxy's 407 before the page's answer is no answer of the site.
    heads.note(page, 200, full, 0);
    heads.note(page, 407, [{ name: 'Proxy-Authenticate', value: 'Basic' }], 1);
    expect(heads.shown(page, notModified, 1)?.status).toBe(200);
    // A page that lost its validator is no longer kept.
    heads.note(page, 200, full, 0);
    heads.note(page, 200, [{ name: 'Content-Type', value: 'text/html' }], 1);
    expect(heads.shown(page, notModified, 1)).toBeNull();
    // Last-Modified is a validator too, and an error page can be kept.
    heads.note(page, 404, [{ name: 'Last-Modified', value: 'Tue, 03 Nov 2026 20:00:00 GMT' }], 0);
    expect(heads.shown(page, [], 0)?.status).toBe(404);
  });

  it('keeps the pages answered last, and none older than its bound', () => {
    const heads = new KeptHeads([], 3);
    for (const [index, name] of ['a', 'b', 'c', 'd'].entries()) {
      heads.note(`https://www.example.org/${name}`, 200, full, index);
    }
    expect(heads.size).toBe(3);
    expect(heads.shown('https://www.example.org/a', [], 4)).toBeNull();
    // Answered again, a page is the newest: the next one in pushes out the oldest.
    heads.note('https://www.example.org/b', 200, full, 5);
    heads.note('https://www.example.org/e', 200, full, 6);
    expect(heads.shown('https://www.example.org/b', [], 7)).not.toBeNull();
    expect(heads.shown('https://www.example.org/c', [], 7)).toBeNull();

    const aging = new KeptHeads();
    aging.note(page, 200, full, 0);
    expect(aging.shown(page, [], KEPT_HEADS_AGE_MS)).not.toBeNull();
    expect(aging.shown(page, [], KEPT_HEADS_AGE_MS + 1)).toBeNull();
  });

  it('reads back what it wrote, and nothing of a file it did not', () => {
    const heads = new KeptHeads();
    heads.note('https://www.example.org/a', 200, full, 10);
    heads.note('https://www.example.org/b', 200, full, 20);
    const again = KeptHeads.parse(heads.serialize(30));
    expect(again.size).toBe(2);
    expect(again.shown('https://www.example.org/a', [], 30)).toEqual(
      heads.shown('https://www.example.org/a', [], 30),
    );
    // What is too old is not written.
    expect(KeptHeads.parse(heads.serialize(KEPT_HEADS_AGE_MS + 15)).size).toBe(1);
    for (const text of [null, '', 'not json', '{}', '{"heads":3}', '{"heads":[{"url":1}]}']) {
      expect(KeptHeads.parse(text).size, String(text)).toBe(0);
    }
    expect(
      KeptHeads.parse(
        '{"heads":[{"url":"u","status":200,"at":1,"headers":[{"name":"ETag","value":"x"}]},{"url":"v","status":200,"at":1,"headers":[1]}]}',
      ).size,
    ).toBe(1);
  });
});
