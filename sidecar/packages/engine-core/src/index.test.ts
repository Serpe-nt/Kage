import { mkdtemp, readFile, readdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { describe, expect, it } from 'vitest';

import {
  ArtifactPathError,
  ArtifactWriter,
  EngineError,
  SeededRandom,
  XhrCollector,
  artifactSegments,
  browserMajorOfUserAgent,
  captureBody,
  createStubEngine,
  headerValue,
  hintPlatformOf,
  hostOf,
  httpOrigin,
  majorOfVersion,
  matchesContentType,
  mediaType,
  osOfHintPlatform,
  osOfPlatform,
  osOfUserAgent,
  parseFingerprint,
  silentLog,
  summarize,
  toHeaders,
  truncateText,
  withSeededMathRandom,
  type FingerprintDocument,
} from './index.js';

const SEED = '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac';
const OTHER_SEED = SEED.replace(/^5/, '6');

describe('createStubEngine', () => {
  it.each(['chromium', 'firefox'] as const)(
    'reports an idle %s engine without a browser',
    (name) => {
      const engine = createStubEngine(name);
      expect(engine.name).toBe(name);
      expect(engine.status()).toEqual({ browserVersion: null });
    },
  );

  it('answers not_implemented to everything that needs a browser', async () => {
    const engine = createStubEngine('firefox');
    for (const call of [
      () => engine.generatePersona({ seed: SEED, claimed_os: 'linux', languages: ['en'] }),
      () => engine.openContext({} as never),
    ]) {
      const error: unknown = await call().catch((thrown: unknown) => thrown);
      expect(error).toBeInstanceOf(EngineError);
      expect((error as EngineError).code).toBe('not_implemented');
      expect((error as EngineError).message).toContain('firefox');
    }
  });

  it('closes more than once without error', async () => {
    const engine = createStubEngine('chromium');
    await expect(engine.close()).resolves.toBeUndefined();
    await expect(engine.close()).resolves.toBeUndefined();
  });

  it('has a log that drops everything', () => {
    expect(() => {
      silentLog.info({}, 'a');
      silentLog.warn({}, 'a');
      silentLog.error({}, 'a');
    }).not.toThrow();
  });
});

describe('SeededRandom', () => {
  it('gives the same stream for the same seed and label', () => {
    const [a, b] = [new SeededRandom(SEED, 'x'), new SeededRandom(SEED, 'x')];
    const draws = Array.from({ length: 50 }, () => a.next());
    expect(Array.from({ length: 50 }, () => b.next())).toEqual(draws);
    expect(new Set(draws).size).toBe(50);
    for (const draw of draws) {
      expect(draw).toBeGreaterThanOrEqual(0);
      expect(draw).toBeLessThan(1);
    }
  });

  it('gives unrelated streams for another seed or another label', () => {
    const first = (seed: string, label: string) => new SeededRandom(seed, label).nextUint64();
    expect(first(SEED, 'x')).not.toBe(first(OTHER_SEED, 'x'));
    expect(first(SEED, 'x')).not.toBe(first(SEED, 'y'));
  });

  it('pins the algorithm: a recorded seed replays the same numbers after any upgrade', () => {
    const random = new SeededRandom(SEED, 'pinned');
    expect([random.nextUint64(), random.nextUint64()].map(String)).toMatchInlineSnapshot(`
      [
        "3802886577532542181",
        "3452404577468067120",
      ]
    `);
  });

  it('draws integers inside a range, both ends included', () => {
    const random = new SeededRandom(SEED, 'range');
    const draws = Array.from({ length: 400 }, () => random.integer(2, 5));
    expect(new Set(draws)).toEqual(new Set([2, 3, 4, 5]));
    expect(random.integer(7, 7)).toBe(7);
    expect(() => random.integer(5, 2)).toThrow(RangeError);
    expect(() => random.integer(0.5, 2)).toThrow(RangeError);
  });

  it.each(['', 'abc', SEED.toUpperCase(), `${SEED}0`])('refuses the seed %j', (seed) => {
    expect(() => new SeededRandom(seed, 'x')).toThrow(/64 lower-case hexadecimal/);
  });
});

describe('withSeededMathRandom', () => {
  it('makes Math.random reproducible inside and restores it afterwards', () => {
    const original = Math.random;
    const draw = () => withSeededMathRandom(SEED, 'gen', () => [Math.random(), Math.random()]);
    expect(draw()).toEqual(draw());
    expect(withSeededMathRandom(OTHER_SEED, 'gen', () => Math.random())).not.toBe(draw()[0]);
    expect(Math.random).toBe(original);
  });

  it('restores Math.random when the function throws', () => {
    const original = Math.random;
    expect(() =>
      withSeededMathRandom(SEED, 'gen', () => {
        throw new Error('boom');
      }),
    ).toThrow('boom');
    expect(Math.random).toBe(original);
  });

  it('refuses to nest and refuses an asynchronous function', () => {
    const original = Math.random;
    expect(() =>
      withSeededMathRandom(SEED, 'a', () => withSeededMathRandom(SEED, 'b', () => 1)),
    ).toThrow(/must not nest/);
    expect(() => withSeededMathRandom(SEED, 'a', () => Promise.resolve(1))).toThrow(/synchronous/);
    expect(Math.random).toBe(original);
    expect(withSeededMathRandom(SEED, 'a', () => 1)).toBe(1);
  });
});

describe('content types', () => {
  it('reads the media type without parameters and case', () => {
    expect(mediaType('Application/JSON; charset=utf-8')).toBe('application/json');
    expect(mediaType('  ')).toBeNull();
    expect(mediaType(null)).toBeNull();
    expect(mediaType(undefined)).toBeNull();
  });

  it.each([
    ['application/json; charset=utf-8', ['application/json'], true],
    ['application/ld+json', ['+json'], true],
    ['application/json', ['+json'], false],
    ['text/html', ['text/*'], true],
    ['text/html', ['TEXT/*'], true],
    ['image/png', ['*/*'], true],
    ['image/png', ['text/*', 'application/json'], false],
    ['application/jsonp', ['application/json'], false],
    [null, ['*/*'], false],
    ['text/plain', [], false],
  ])('%j against %j is %j', (contentType, patterns, expected) => {
    expect(matchesContentType(contentType, patterns)).toBe(expected);
  });
});

describe('captureBody', () => {
  it('keeps a text body as text', () => {
    expect(captureBody(Buffer.from('{"a":"ä"}'), 100)).toEqual({
      bytes: 10,
      truncated: false,
      encoding: 'utf8',
      data: '{"a":"ä"}',
    });
    expect(captureBody(new Uint8Array(), 10)).toEqual({
      bytes: 0,
      truncated: false,
      encoding: 'utf8',
      data: '',
    });
  });

  it('cuts at the limit and never inside a character', () => {
    const body = Buffer.from('aä€😀z');
    // a=1 byte, ä=2, €=3, 😀=4: every cut inside a character steps back.
    expect(captureBody(body, 2)).toMatchObject({
      bytes: 11,
      truncated: true,
      encoding: 'utf8',
      data: 'a',
    });
    expect(captureBody(body, 5)).toMatchObject({ truncated: true, data: 'aä' });
    expect(captureBody(body, 9)).toMatchObject({ truncated: true, data: 'aä€' });
    expect(captureBody(body, 10)).toMatchObject({ truncated: true, data: 'aä€😀' });
    expect(captureBody(body, 11)).toMatchObject({ truncated: false, data: 'aä€😀z' });
  });

  it('writes a body that is not UTF-8 as base64', () => {
    const png = Uint8Array.from([0x89, 0x50, 0x4e, 0x47, 0xff, 0xfe]);
    expect(captureBody(png, 100)).toEqual({
      bytes: 6,
      truncated: false,
      encoding: 'base64',
      data: Buffer.from(png).toString('base64'),
    });
    expect(captureBody(png, 4)).toEqual({
      bytes: 6,
      truncated: true,
      encoding: 'base64',
      data: Buffer.from(png.subarray(0, 4)).toString('base64'),
    });
  });
});

describe('text and header helpers', () => {
  it('cuts a text at a byte limit on a character boundary', () => {
    expect(truncateText('abc', 3)).toEqual({ text: 'abc', truncated: false });
    expect(truncateText('aä€', 2)).toEqual({ text: 'a', truncated: true });
    expect(truncateText('aä€', 3)).toEqual({ text: 'aä', truncated: true });
    expect(truncateText('€', 0)).toEqual({ text: '', truncated: true });
  });

  it('lower-cases header names, keeps order and repeats, and finds a value', () => {
    const headers = toHeaders([
      { name: 'Content-Type', value: 'text/html' },
      { name: 'Set-Cookie', value: 'a=1' },
      { name: 'set-cookie', value: 'b=2' },
    ]);
    expect(headers.map((header) => header.name)).toEqual([
      'content-type',
      'set-cookie',
      'set-cookie',
    ]);
    expect(headerValue(headers, 'SET-COOKIE')).toBe('a=1');
    expect(headerValue(headers, 'location')).toBeNull();
  });

  it('reads hosts and origins and gives null for what has none', () => {
    expect(hostOf('https://CDN.Example.org:8443/a.js')).toBe('cdn.example.org');
    expect(hostOf('data:text/plain,x')).toBeNull();
    expect(hostOf('not a url')).toBeNull();
    expect(httpOrigin('https://example.org:8443/a?b#c')).toBe('https://example.org:8443');
    expect(httpOrigin('about:blank')).toBeNull();
    expect(httpOrigin('nope')).toBeNull();
  });
});

describe('XhrCollector', () => {
  const observed = (body: string | null, contentType = 'application/json; charset=utf-8') => ({
    url: 'https://example.org/api',
    method: 'GET',
    kind: 'fetch' as const,
    status: 200,
    contentType,
    body: body === null ? null : Buffer.from(body),
  });

  it('keeps nothing without a filter', () => {
    const collector = new XhrCollector(null);
    expect(collector.wants('application/json')).toBe(false);
    collector.add(observed('{}'));
    expect(collector.responses()).toEqual([]);
    expect(collector.dropped).toBe(1);
  });

  it('keeps matching responses, cuts their bodies and reports a missing body as null', () => {
    const collector = new XhrCollector({ content_types: ['application/json'], max_bytes: 4 });
    expect(collector.wants('text/html')).toBe(false);
    expect(collector.wants('application/json')).toBe(true);
    collector.add(observed('{"a":1}'));
    collector.add(observed(null));
    expect(collector.responses()).toEqual([
      {
        url: 'https://example.org/api',
        method: 'GET',
        kind: 'fetch',
        status: 200,
        content_type: 'application/json',
        body: { bytes: 7, truncated: true, encoding: 'utf8', data: '{"a"' },
      },
      {
        url: 'https://example.org/api',
        method: 'GET',
        kind: 'fetch',
        status: 200,
        content_type: 'application/json',
        body: null,
      },
    ]);
    expect(collector.dropped).toBe(0);
  });

  it('stops at max_responses and counts what it dropped', () => {
    const collector = new XhrCollector({ content_types: ['*/*'], max_bytes: 10, max_responses: 2 });
    for (let index = 0; index < 5; index += 1) {
      if (collector.wants('text/plain')) {
        collector.add(observed('x', 'text/plain'));
      }
    }
    expect(collector.responses()).toHaveLength(2);
    expect(collector.dropped).toBe(3);
    // A response that slipped past `wants` is still not kept.
    collector.add(observed('x'));
    expect(collector.responses()).toHaveLength(2);
    expect(collector.dropped).toBe(4);
  });
});

describe('ArtifactWriter', () => {
  it('writes a file below the root and describes it', async () => {
    const root = await mkdtemp(path.join(tmpdir(), 'kage-artifacts-'));
    const writer = new ArtifactWriter(root);
    const reference = await writer.write(
      'sidecar/ctx-1/shot.png',
      Buffer.from('hello'),
      'image/png',
    );
    expect(reference).toEqual({
      path: 'sidecar/ctx-1/shot.png',
      bytes: 5,
      sha256: '2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824',
      content_type: 'image/png',
    });
    expect(await readFile(path.join(root, 'sidecar', 'ctx-1', 'shot.png'), 'utf8')).toBe('hello');
    // No temporary file is left behind.
    expect(await readdir(path.join(root, 'sidecar', 'ctx-1'))).toEqual(['shot.png']);
  });

  it.each([
    '../x',
    'a/../b',
    '/abs',
    'a//b',
    'a/.hidden',
    '',
    'a\\b',
    'a/b c',
    `a/${'b'.repeat(400)}`,
  ])('refuses the path %j', async (relative) => {
    expect(() => artifactSegments(relative)).toThrow(ArtifactPathError);
    await expect(
      new ArtifactWriter('/nonexistent').write(relative, new Uint8Array(), 'x/y'),
    ).rejects.toThrow(ArtifactPathError);
  });
});

describe('identity marks', () => {
  const windows =
    'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36';
  const mac =
    'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36';
  const linux =
    'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36';

  it('reads the operating system from each place a page can read it', () => {
    expect([windows, mac, linux].map(osOfUserAgent)).toEqual(['windows', 'macos', 'linux']);
    expect(osOfUserAgent('curl/8')).toBeNull();
    expect(['Win32', 'MacIntel', 'Linux x86_64', 'iPhone'].map(osOfPlatform)).toEqual([
      'windows',
      'macos',
      'linux',
      null,
    ]);
    expect(['"Windows"', 'macOS', '"Linux"', 'Android'].map(osOfHintPlatform)).toEqual([
      'windows',
      'macos',
      'linux',
      null,
    ]);
    expect(hintPlatformOf('macos')).toBe('macOS');
  });

  it('reads browser majors', () => {
    expect(browserMajorOfUserAgent(linux)).toBe(155);
    expect(
      browserMajorOfUserAgent(
        'Mozilla/5.0 (X11; Linux x86_64; rv:152.0) Gecko/20100101 Firefox/152.0',
      ),
    ).toBe(152);
    expect(browserMajorOfUserAgent('curl/8')).toBeNull();
    expect(majorOfVersion('155.0.8059.39')).toBe(155);
    expect(majorOfVersion('')).toBeNull();
    expect(majorOfVersion('x.1')).toBeNull();
  });
});

describe('fingerprint document', () => {
  const document: FingerprintDocument = {
    schema: 1,
    engine: 'chromium',
    generator: 'fingerprint-generator@2.1.88',
    seed: SEED,
    claimed_os: 'linux',
    browser_version: '155.0.8059.39',
    user_agent:
      'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36',
    platform: 'Linux x86_64',
    languages: ['de-DE', 'de', 'en-US', 'en'],
    native: {
      screen: { width: 1920, height: 1080, dpr: 1, color_depth: 24 },
      hardware_concurrency: 8,
    },
    host_bound: ['device_memory', 'webgl'],
    inject: 'none',
    generated: { fingerprint: {}, headers: {} },
  };

  it('is read back unchanged and summarized', () => {
    expect(parseFingerprint(structuredClone(document))).toEqual(document);
    expect(summarize(document)).toEqual({
      engine: 'chromium',
      claimed_os: 'linux',
      browser_version: '155.0.8059.39',
      browser_major: 155,
      user_agent: document.user_agent,
      platform: 'Linux x86_64',
      screen: { width: 1920, height: 1080, dpr: 1, color_depth: 24 },
      hardware_concurrency: 8,
      languages: ['de-DE', 'de', 'en-US', 'en'],
      generator: 'fingerprint-generator@2.1.88',
      seed: SEED,
      inject: 'none',
    });
  });

  it.each([
    ['another schema version', { schema: 2 }, /schema/],
    ['a missing part', { native: undefined }, /native/],
    ['an unknown field', { extra: 1 }, /fingerprint|extra/],
    ['a version that is none', { browser_version: 'latest' }, /browser_version/],
  ])('refuses %s as an invalid request', (_label, change, message) => {
    const error = (() => {
      try {
        parseFingerprint({ ...document, ...change });
        return null;
      } catch (thrown) {
        return thrown as EngineError;
      }
    })();
    expect(error).toBeInstanceOf(EngineError);
    expect(error?.code).toBe('invalid_request');
    expect(error?.message).toMatch(message);
  });

  it('refuses something that is no document at all', () => {
    expect(() => parseFingerprint(null)).toThrow(/not a fingerprint document/);
    expect(() => parseFingerprint('x')).toThrow(EngineError);
  });
});
