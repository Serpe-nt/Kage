import { tmpdir } from 'node:os';
import path from 'node:path';

import { PageResultSchema, type NavigateRequest, type PageResult } from '@kage/contract';
import { describe, expect, it } from 'vitest';

import { ArtifactWriter } from './artifacts.js';
import { motorProfile, type InputDevice } from './behavior/index.js';
import { ConsoleLog } from './console-log.js';
import { EngineError, silentLog, type OperationEnv } from './engine.js';
import {
  PageSession,
  entriesToGoBack,
  plainAddress,
  type DriverPage,
  type DriverRequest,
  type DriverResponse,
  type SessionDriver,
  type TabHistory,
} from './session.js';

const site = (path: string) => `https://www.example.org${path}`;
const plain = (path: string) => plainAddress(site(path)) ?? '';

/** What a page says of its tab: by default one that began with the open page. */
const tab = (change: Partial<TabHistory> = {}): TabHistory => {
  const before = change.before ?? [];
  return { api: true, before, listed: before.length + 1, total: before.length + 1, ...change };
};

describe('the address a link is compared by', () => {
  it('is the origin, the path without the slash it ends on, and the query', () => {
    expect(plainAddress('https://www.example.org/a/b?x=1&y=2#part')).toBe(
      'https://www.example.org/a/b?x=1&y=2',
    );
    expect(plainAddress('https://www.example.org/a/b/')).toBe('https://www.example.org/a/b');
    expect(plainAddress('https://www.example.org/')).toBe('https://www.example.org');
    expect(plainAddress('HTTPS://WWW.Example.org:443/a')).toBe('https://www.example.org/a');
    expect(plainAddress('http://localhost:8088/site/?page=2')).toBe(
      'http://localhost:8088/site?page=2',
    );
    // Two addresses of one page compare equal, two pages do not.
    expect(plain('/site/')).toBe(plain('/site#team'));
    expect(plain('/site?page=2')).not.toBe(plain('/site'));
    expect(plainAddress('https://blog.example.org/a')).not.toBe(plain('/a'));
  });

  it('is nothing for what is no address', () => {
    expect(plainAddress('not an address')).toBeNull();
    expect(plainAddress('')).toBeNull();
    expect(plainAddress(null)).toBeNull();
    expect(plainAddress(undefined)).toBeNull();
  });
});

describe('how far back in its tab a persona goes for a page', () => {
  const wanted = plain('/listing');

  it('goes as far as the browser names the page, nearest first', () => {
    expect(entriesToGoBack(tab({ before: [site('/listing')] }), wanted, 3, true)).toBe(1);
    expect(entriesToGoBack(tab({ before: [site('/listing'), site('/a')] }), wanted, 3, true)).toBe(
      2,
    );
    expect(
      entriesToGoBack(
        tab({ before: [site('/listing'), site('/a'), site('/listing'), site('/b')] }),
        wanted,
        10,
        true,
      ),
    ).toBe(2);
    // The address as the tab has it: with a fragment, with a slash at its end.
    expect(entriesToGoBack(tab({ before: [site('/listing/#top')] }), wanted, 1, true)).toBe(1);
    // The browser's word is enough: what the session saw does not matter.
    expect(entriesToGoBack(tab({ before: [site('/listing')] }), wanted, 3, false)).toBe(1);
  });

  it('does not move when the browser names every entry within reach and none is the page', () => {
    // Not in the tab at all.
    expect(entriesToGoBack(tab({ before: [site('/a'), site('/b')] }), wanted, 3, true)).toBe(0);
    expect(entriesToGoBack(tab(), wanted, 3, true)).toBe(0);
    // In the tab, and further back than the persona may go.
    const far = tab({ before: [site('/listing'), site('/a'), site('/b')] });
    expect(entriesToGoBack(far, wanted, 2, true)).toBe(0);
    expect(entriesToGoBack(far, wanted, 3, true)).toBe(3);
    // Entries nobody names lie behind the ones within reach: out of reach too.
    expect(
      entriesToGoBack(
        tab({ before: [site('/a'), site('/b'), site('/c')], listed: 4, total: 6 }),
        wanted,
        3,
        true,
      ),
    ).toBe(0);
    // Nobody may go back at all.
    expect(entriesToGoBack(tab({ before: [site('/listing')] }), wanted, 0, true)).toBe(0);
  });

  it('goes and looks where an entry has no name, and only for a page the tab has shown', () => {
    // An entry whose address the browser keeps back.
    const kept = tab({ before: [null, site('/a')] });
    expect(entriesToGoBack(kept, wanted, 3, true)).toBe(2);
    expect(entriesToGoBack(kept, wanted, 3, false)).toBe(0);
    // Within a reach of one the browser names the entry, and it is another page.
    expect(entriesToGoBack(kept, wanted, 1, true)).toBe(0);
    expect(entriesToGoBack(tab({ before: [site('/a'), null] }), wanted, 1, true)).toBe(1);
    // A name behind one that is kept back: as far as the name, the other is looked at on the way.
    expect(entriesToGoBack(tab({ before: [site('/listing'), null] }), wanted, 3, false)).toBe(2);
    // Entries of another origin are not listed at all: the tab has more than the browser names.
    const foreign = tab({ before: [], listed: 1, total: 3 });
    expect(entriesToGoBack(foreign, wanted, 3, true)).toBe(2);
    expect(entriesToGoBack(foreign, wanted, 1, true)).toBe(1);
    expect(entriesToGoBack(foreign, wanted, 3, false)).toBe(0);
    // Past the ones it names, as far as the reach goes.
    expect(
      entriesToGoBack(tab({ before: [site('/a')], listed: 2, total: 4 }), wanted, 3, true),
    ).toBe(3);
    expect(
      entriesToGoBack(tab({ before: [site('/a')], listed: 2, total: 3 }), wanted, 3, true),
    ).toBe(2);
  });

  it('goes entry by entry in a browser that lists nothing, when there is anything to go back to', () => {
    const blind = (total: number): TabHistory => ({ api: false, before: [], listed: 0, total });
    expect(entriesToGoBack(blind(4), wanted, 3, true)).toBe(3);
    expect(entriesToGoBack(blind(2), wanted, 1, true)).toBe(1);
    // The tab began with the open page, or never showed the one wanted.
    expect(entriesToGoBack(blind(1), wanted, 3, true)).toBe(0);
    expect(entriesToGoBack(blind(4), wanted, 3, false)).toBe(0);
    expect(entriesToGoBack(blind(4), wanted, 0, true)).toBe(0);
  });
});

const PERSONA_SEED = '0b6d1f4e8a2c5397b1d3f5a7c9e0b2d4f6a8c0e2b4d6f8a1c3e5a7c9e1b3d5f7';
const REQUEST_SEED = '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac';

/** What a scripted browser was asked to do, for assertions. */
interface Driven {
  /** The addresses the driver was asked to load, in order. */
  readonly loaded: string[];
  /** Every input of any kind: through the display's devices or through the driver. */
  readonly input: string[];
  /** How often the display's input device was opened. */
  opened: number;
}

/**
 * A browser that answers every navigation with one small page, at once:
 * enough of a driver for the session to load, wait, dwell and capture. A
 * navigation to an address with `fail` in it fails as the network would.
 */
function scriptedDriver(): { driver: SessionDriver; driven: Driven } {
  const driven: Driven = { loaded: [], input: [], opened: 0 };
  // Each event has listeners of its own type; the page hands each the value of its event.
  const listeners = new Map<string, ((value: never) => void)[]>();
  const emit = (event: string, value?: unknown) => {
    for (const listener of listeners.get(event) ?? []) {
      listener(value as never);
    }
  };
  let address = 'about:blank';
  const mainFrame = { url: () => address };
  const answer = (url: string): DriverResponse => {
    const request: DriverRequest = {
      url: () => url,
      method: () => 'GET',
      resourceType: () => 'document',
      isNavigationRequest: () => true,
      frame: () => mainFrame,
      headers: () => ({}),
      redirectedFrom: () => null,
      response: () => Promise.resolve(response),
    };
    const response: DriverResponse = {
      url: () => url,
      status: () => 200,
      headers: () => ({ 'content-type': 'text/html' }),
      headersArray: () => Promise.resolve([{ name: 'content-type', value: 'text/html' }]),
      body: () => Promise.resolve(Buffer.from('<p>A page</p>')),
      request: () => request,
    };
    return response;
  };
  const refuse = (what: string) => () => {
    driven.input.push(what);
    return Promise.reject(new Error(`no ${what} in this test`));
  };
  const page: DriverPage = {
    url: () => address,
    title: () => Promise.resolve('A page'),
    content: () => Promise.resolve('<html><body><p>A page</p></body></html>'),
    mainFrame: () => mainFrame,
    frames: () => [mainFrame],
    goto: (url) => {
      driven.loaded.push(url);
      if (url.includes('fail')) {
        return Promise.reject(new Error('net::ERR_NAME_NOT_RESOLVED'));
      }
      const response = answer(url);
      emit('request', response.request());
      address = url;
      emit('response', response);
      emit('domcontentloaded');
      emit('load');
      return Promise.resolve(response);
    },
    goBack: () => Promise.resolve(null),
    waitForSelector: () => Promise.resolve(null),
    waitForLoadState: () => Promise.resolve(),
    // A page that has loaded and holds no text: every other read finds nothing.
    evaluate: (expression: string) =>
      Promise.resolve((expression === 'document.readyState' ? 'complete' : '') as never),
    click: refuse('driver click'),
    hover: refuse('driver hover'),
    fill: refuse('driver fill'),
    selectOption: refuse('driver select'),
    keyboard: { press: refuse('driver key') },
    mouse: { wheel: refuse('driver wheel') },
    on: (event: string, listener: (value: never) => void) => {
      listeners.set(event, [...(listeners.get(event) ?? []), listener]);
      return page;
    },
  };
  const device: InputDevice = {
    screen: { width: 1920, height: 1080 },
    pointerPosition: () => Promise.resolve({ x: 960, y: 540 }),
    pointerMove: refuse('pointer move'),
    buttonDown: refuse('button down'),
    buttonUp: refuse('button up'),
    wheelNotch: refuse('wheel notch'),
    levelsOf: () => ({ shift: false, altGr: false, onLayout: true }),
    keyDown: refuse('key down'),
    keyUp: refuse('key up'),
    flush: () => Promise.resolve(),
    close: () => Promise.resolve(),
  };
  const driver: SessionDriver = {
    page,
    classifyError: (thrown) => ({
      code: 'navigation_failed',
      message: String(thrown),
      net_error: 'ERR_NAME_NOT_RESOLVED',
    }),
    settleAfterFailure: () => Promise.resolve(),
    screenshot: () => Promise.reject(new Error('no screenshot in this test')),
    pdf: () => Promise.reject(new Error('no PDF in this test')),
    protocolOf: () => Promise.resolve('http/1.1'),
    console: new ConsoleLog(),
    cookies: () => Promise.resolve([]),
    setCookie: () => Promise.resolve(true),
    windowOrigin: () => ({ x: 0, y: 0 }),
    openInput: () => {
      driven.opened += 1;
      return Promise.resolve(device);
    },
  };
  return { driver, driven };
}

describe('a still visit (contract 1.5.0)', () => {
  const env: OperationEnv = {
    artifacts: new ArtifactWriter(path.join(tmpdir(), 'kage-still-unused')),
    defaultArtifactPrefix: 'sidecar/test',
  };
  const target = 'https://www.example.org/listing?page=2';
  const session = () => {
    const { driver, driven } = scriptedDriver();
    const parts = { driver, personaSeed: PERSONA_SEED, selftestPages: [], log: silentLog };
    return { session: new PageSession(parts), driven };
  };
  const navigate = async (
    on: PageSession,
    change: Partial<NavigateRequest>,
  ): Promise<{ result: PageResult; took: number }> => {
    const began = performance.now();
    const result = await on.navigate(
      {
        url: target,
        wait: { until: 'load' },
        capture: { dom: true, raw: false, xhr: null, console: false, screenshot: null, pdf: null },
        ...change,
      },
      env,
    );
    // What the engine returns is what the contract says, every time.
    expect(PageResultSchema.parse(result)).toEqual(result);
    return { result, took: performance.now() - began };
  };
  const nothing = {
    moves: 0,
    positions: 0,
    overshoots: 0,
    clicks: 0,
    notches: 0,
    keys: 0,
    typos: 0,
    wanders: 0,
    patience_ms: null,
    scroll_reached: null,
    scroll_ended_by: null,
  };

  it('loads the page typed, rests for the dwell, and never opens the input', async () => {
    const { session: on, driven } = session();
    const { result, took } = await navigate(on, {
      dwell_ms: 300,
      behavior: { seed: REQUEST_SEED, mode: 'still', entry: { strategy: 'direct' } },
    });
    expect(driven).toEqual({ loaded: [target], input: [], opened: 0 });
    expect(result).toMatchObject({ status: 200, final_url: target, error: null });
    expect(result.dom_html).toContain('A page');
    expect(result.behavior).toEqual({
      seed: REQUEST_SEED,
      // The persona's own hand, though it never moved: the same for every request of it.
      profile_id: motorProfile(PERSONA_SEED).id,
      entry: { strategy: 'direct', outcome: 'arrived', from_url: null, referer: null, backs: null },
      dwell_ms: 300,
      ...nothing,
    });
    // A timer may fire a moment early; the dwell is never noticeably short.
    expect(took).toBeGreaterThanOrEqual(295);
  });

  it('draws a seed when the request names none, and reports no entry when it named none', async () => {
    const { session: on, driven } = session();
    const { result } = await navigate(on, { behavior: { mode: 'still' } });
    expect(driven.opened).toBe(0);
    expect(driven.input).toEqual([]);
    expect(result.behavior?.seed).toMatch(/^[0-9a-f]{64}$/);
    expect(result.behavior).toMatchObject({ entry: null, dwell_ms: 0, ...nothing });
    const again = await navigate(on, { behavior: { mode: 'still', entry: { strategy: 'none' } } });
    expect(again.result.behavior?.seed).not.toBe(result.behavior?.seed);
    expect(again.result.behavior?.entry).toMatchObject({ strategy: 'none', outcome: 'arrived' });
    expect(again.result.behavior?.profile_id).toBe(result.behavior?.profile_id);
    expect(driven.opened).toBe(0);
  });

  it('spends no more of the dwell than the budget has left', async () => {
    const { session: on } = session();
    const { result, took } = await navigate(on, {
      timeout_ms: 1000,
      dwell_ms: 5000,
      behavior: { seed: REQUEST_SEED, mode: 'still' },
    });
    expect(result.error).toBeNull();
    expect(result.behavior?.dwell_ms).toBeLessThanOrEqual(1000);
    expect(result.behavior?.dwell_ms).toBeGreaterThan(800);
    expect(took).toBeLessThan(2500);
  });

  it('reports a still visit that did not arrive, with nothing dwelled and nothing sent', async () => {
    const { session: on, driven } = session();
    const { result } = await navigate(on, {
      url: 'https://fail.example.org/',
      dwell_ms: 5000,
      behavior: { seed: REQUEST_SEED, mode: 'still' },
    });
    expect(result.error).toMatchObject({ code: 'navigation_failed' });
    expect(result.behavior).toMatchObject({ seed: REQUEST_SEED, dwell_ms: 0, ...nothing });
    expect(driven).toMatchObject({ input: [], opened: 0 });
  });

  it.each<[string, NonNullable<NavigateRequest['behavior']>, string]>([
    ['a scroll', { scroll_depth: 0.5 }, 'behavior.scroll_depth: a still visit does not scroll'],
    [
      'a patience',
      { patience_ms: 30_000 },
      'behavior.patience_ms: a still visit does not scroll, so it has no patience to spend',
    ],
    [
      'a wandering hand',
      { wander: true },
      'behavior.wander: a still visit does not move the mouse',
    ],
    [
      "the persona's own dwell",
      { dwell: 'human' },
      'behavior.dwell: a still visit dwells for the fixed dwell_ms',
    ],
    [
      'a click on a link',
      { entry: { strategy: 'link', from_url: 'https://www.example.org/' } },
      'behavior.entry.strategy: a still visit arrives by a typed navigation: direct or none',
    ],
    [
      'a click on a search result',
      { entry: { strategy: 'search_engine', from_url: 'https://search.example.org/?q=x' } },
      'behavior.entry.strategy: a still visit arrives by a typed navigation: direct or none',
    ],
    [
      'a click on the open page',
      { entry: { strategy: 'direct', from_open_page: true } },
      'behavior.entry.from_open_page: a still visit clicks nothing on the open page',
    ],
  ])('refuses %s before it loads anything', async (_label, behavior, message) => {
    const { session: on, driven } = session();
    const refused = navigate(on, { behavior: { mode: 'still', ...behavior } });
    await expect(refused).rejects.toBeInstanceOf(EngineError);
    await expect(refused).rejects.toMatchObject({ code: 'invalid_request', message });
    expect(driven).toEqual({ loaded: [], input: [], opened: 0 });
  });

  it('leaves a human visit as it was: the hand is there, absent a mode or named', async () => {
    const { session: on, driven } = session();
    for (const mode of [undefined, 'human'] as const) {
      const { result } = await navigate(on, {
        behavior: { seed: REQUEST_SEED, wander: false, ...(mode === undefined ? {} : { mode }) },
      });
      expect(result.behavior).toMatchObject({ seed: REQUEST_SEED, entry: null, ...nothing });
    }
    // The device was opened for the first and kept for the second.
    expect(driven.opened).toBe(1);
    // And a bare load opens nothing and reports nothing.
    const { session: bare, driven: untouched } = session();
    const { result } = await navigate(bare, { dwell_ms: 50 });
    expect(result.behavior).toBeNull();
    expect(untouched).toEqual({ loaded: [target], input: [], opened: 0 });
  });
});
