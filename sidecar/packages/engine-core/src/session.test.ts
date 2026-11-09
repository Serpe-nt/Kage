import { mkdtemp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import {
  PageResultSchema,
  type FlowEvent,
  type FlowRequest,
  type NavigateRequest,
  type PageResult,
  type RedirectHop,
} from '@kage/contract';
import { describe, expect, it } from 'vitest';

import { ArtifactWriter } from './artifacts.js';
import { motorProfile, type InputDevice } from './behavior/index.js';
import { ConsoleLog } from './console-log.js';
import { EngineError, silentLog, type OperationEnv } from './engine.js';
import {
  PageMoved,
  PageSession,
  entriesToGoBack,
  errorPageTarget,
  plainAddress,
  type DriverPage,
  type DriverRequest,
  type DriverResponse,
  type DriverTab,
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

/** The tabs of a scripted browser besides the one it began with (F96). */
interface Tabs {
  /** How often a fresh tab replaced the open one. */
  fresh: number;
  /** The tabs a page opened (F184), closed ones too. */
  readonly opened: { readonly url: string; closed: boolean; readonly tab: DriverTab }[];
  /** How often the open tab was put in front again. */
  fronts: number;
  /** Whether the open tab has the focus: not while a tab a page opened is in front of it. */
  focused: boolean;
  /** Whether putting the open tab in front gives it the focus back; a browser that refuses keeps the other in front. */
  frontable: boolean;
  /** Has a page open a tab: the browser shows it in front, and reports it. */
  open(url: string): void;
}

/**
 * A browser that answers every navigation with one small page, at once:
 * enough of a driver for the session to load, wait, dwell and capture. A
 * navigation to an address with `fail` in it fails as the network would.
 * One with `redirect=<address>` is answered with a redirect there, which
 * the browser cannot load; one with `moves=<address>` loads, and its
 * script then sends the tab there, which the browser cannot load either:
 * both end on the browser's own error page, as Chrome does (F102). One
 * with `ffmoves=<address>` loads and moves there as Firefox does: the tab
 * keeps that address and its document is Firefox's error page (F167). One
 * with `refused=<address>` is answered with a redirect there that the
 * browser refuses to follow and tells only its debugger of, as Chrome does
 * for `file:` (F167). One with `ffrefused` in it is answered with an
 * empty `403` that Firefox replaces with its own error page, and never
 * loads (F189). One
 * with `stuck` in it loads, and then answers no read again (its script
 * never yields); one with `crash` loads, and then its renderer crashes
 * (F103). One with `refreshes=<address>` loads, and its meta refresh
 * sends the tab there while the hand reads where the page is: the read
 * fails as Firefox's does (F192). One with `readbreaks=<message>` loads,
 * and the hand's look at where the page is fails with that message. One
 * with `ffsends=<address>` is answered, and its script sends the tab there
 * before its `load`, where nothing answers: Firefox shows its own page
 * under that address and the load never ends (F200). One with
 * `later=<address>` loads, and 150 ms later moves itself there, where a
 * page with another title loads (F201). One with `revalidated` in it is
 * answered `304`, and the driver names the kept page's head (F178); one
 * with `chrome403` in it gets an empty `403` Chrome replaces with its own
 * page, which only the driver's debugger saw (F204).
 * Each tab keeps a history as Chrome keeps it: the blank page it began
 * with stays an entry, every navigation adds one, and the browser's own way
 * back moves to the entry before without asking anyone for it.
 */
function scriptedDriver(): { driver: SessionDriver; driven: Driven; tabs: Tabs } {
  const driven: Driven = { loaded: [], input: [], opened: 0 };
  let reportTab: () => void = () => undefined;
  const tabs: Tabs = {
    fresh: 0,
    opened: [],
    fronts: 0,
    focused: true,
    frontable: true,
    open: (url) => {
      const entry = {
        url,
        closed: false,
        tab: {
          url: () => url,
          close: () => {
            entry.closed = true;
            if (tabs.opened.every((other) => other.closed)) {
              tabs.focused = true;
            }
            return Promise.resolve();
          },
        },
      };
      tabs.opened.push(entry);
      tabs.focused = false;
      reportTab();
    },
  };
  // What only the browser's debugger saw of the latest navigation (F167).
  let unanswered: RedirectHop[] = [];
  const refuse = (what: string) => () => {
    driven.input.push(what);
    return Promise.reject(new Error(`no ${what} in this test`));
  };
  const makePage = (): DriverPage => {
    // Each event has listeners of its own type; the page hands each the value of its event.
    const listeners = new Map<string, ((value: never) => void)[]>();
    const emit = (event: string, value?: unknown) => {
      for (const listener of listeners.get(event) ?? []) {
        listener(value as never);
      }
    };
    let address = 'about:blank';
    // The document's own address where it is not the tab's (Firefox's error page).
    let documentUri: string | null = null;
    const history = ['about:blank'];
    // A page whose script never yields answers no read again.
    let stuck = false;
    // The page's meta refresh, and a look of the hand's that fails (F192).
    let refreshTo: string | null = null;
    let readFailure: string | null = null;
    // The title of the document the tab shows.
    let title = 'A page';
    const never = new Promise<never>(() => undefined);
    const reading = <T>(value: T): Promise<T> => (stuck ? never : Promise.resolve(value));
    const mainFrame = { url: () => address };
    const answer = (url: string, status = 200, location?: string): DriverResponse => {
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
        status: () => status,
        headers: () => ({
          'content-type': 'text/html',
          ...(location === undefined ? {} : { location }),
        }),
        headersArray: () => Promise.resolve([{ name: 'content-type', value: 'text/html' }]),
        body: () => Promise.resolve(Buffer.from('<p>A page</p>')),
        request: () => request,
      };
      return response;
    };
    const page: DriverPage = {
      url: () => address,
      title: () => reading(title),
      content: () => reading('<html><body><p>A page</p></body></html>'),
      mainFrame: () => mainFrame,
      frames: () => [mainFrame],
      goto: (url, options) => {
        driven.loaded.push(url);
        documentUri = null;
        unanswered = [];
        title = 'A page';
        if (url.includes('chrome403')) {
          // Chrome: its own page in place of the answer, which its driver never names.
          emit('request', answer(url).request());
          address = 'chrome-error://chromewebdata/';
          return Promise.reject(new Error('net::ERR_HTTP_RESPONSE_CODE_FAILURE'));
        }
        if (url.includes('fail')) {
          return Promise.reject(new Error('net::ERR_NAME_NOT_RESOLVED'));
        }
        if (url.includes('saves-file')) {
          // Chrome over plain http: the answer is a file it holds for the
          // person, the driver says a download began, and the load never
          // ends (F120).
          emit('request', answer(url).request());
          setTimeout(() => {
            emit('download', { url: () => url });
          }, 50);
          return never;
        }
        if (url.includes('ffrefused')) {
          // Firefox: the answer is seen, its own page shown, no `load`.
          const refusal = answer(url, 403);
          emit('request', refusal.request());
          address = url;
          history.push(url);
          emit('response', refusal);
          documentUri = `about:neterror?e=httpErrorPage&u=${encodeURIComponent(url)}&c=UTF-8`;
          return new Promise((_, reject) =>
            setTimeout(
              () => {
                reject(new Error('Timeout exceeded'));
              },
              Math.min(options.timeout, 3000),
            ),
          );
        }
        const sent = (key: string) => new URL(url).searchParams.get(key);
        const refused = sent('refused');
        if (refused !== null) {
          unanswered = [{ url, status: 302, location: refused }];
          return Promise.reject(new Error('net::ERR_UNSAFE_REDIRECT'));
        }
        const redirect = sent('redirect');
        if (redirect !== null) {
          const moved = answer(url, 302, redirect);
          emit('request', moved.request());
          emit('response', moved);
          if (/^https?:/.test(redirect)) {
            emit('request', answer(redirect).request());
          }
          address = 'chrome-error://chromewebdata/';
          return Promise.reject(new Error('net::ERR_TUNNEL_CONNECTION_FAILED'));
        }
        // A redirect whose target never answers, the tab still on the page
        // before (review of #101): with `hop=` the step passes on it as a
        // hand's click does once the first 302 came, with `pending=` the
        // `goto` waits out its time.
        const hop = sent('hop') ?? sent('pending');
        if (hop !== null) {
          const moved = answer(url, 302, hop);
          emit('request', moved.request());
          emit('response', moved);
          if (/^https?:/.test(hop)) {
            emit('request', answer(hop).request());
          }
          if (sent('hop') !== null) {
            return Promise.resolve(moved);
          }
          return new Promise((_, reject) =>
            setTimeout(
              () => {
                reject(new Error(`Timeout ${String(options.timeout)}ms exceeded.`));
              },
              Math.min(options.timeout, 300),
            ),
          );
        }
        const ffsends = sent('ffsends');
        if (ffsends !== null) {
          // Firefox: the page is answered, its script sends the tab on
          // before `load`, nothing answers there, and no `load` comes.
          const first = answer(url);
          emit('request', first.request());
          address = url;
          history.push(url);
          emit('response', first);
          emit('request', answer(ffsends).request());
          address = ffsends;
          documentUri = `about:neterror?e=netReset&u=${encodeURIComponent(ffsends)}&c=UTF-8`;
          return new Promise((_, reject) =>
            setTimeout(
              () => {
                reject(new Error('Timeout exceeded'));
              },
              Math.min(options.timeout, 3000),
            ),
          );
        }
        const moves = sent('moves');
        const response = answer(url, url.includes('revalidated') ? 304 : 200);
        emit('request', response.request());
        address = url;
        history.push(url);
        emit('response', response);
        emit('domcontentloaded');
        emit('load');
        if (moves !== null) {
          // The page's own script sends the tab on; nothing answers.
          emit('request', answer(moves).request());
          address = 'chrome-error://chromewebdata/';
        }
        const ffmoves = sent('ffmoves');
        if (ffmoves !== null) {
          // Firefox: the tab shows the address, its document is the error page.
          address = ffmoves;
          documentUri = `about:neterror?e=connectionFailure&u=${encodeURIComponent(ffmoves)}&c=UTF-8`;
        }
        refreshTo = sent('refreshes');
        readFailure = sent('readbreaks');
        const later = sent('later');
        if (later !== null) {
          setTimeout(() => {
            const there = answer(later);
            emit('request', there.request());
            address = later;
            history.push(later);
            title = 'Where it went';
            emit('response', there);
          }, 150);
        }
        stuck = url.includes('stuck') || url.includes('crash');
        if (url.includes('crash')) {
          setTimeout(() => {
            emit('crash');
          }, 20);
        }
        return Promise.resolve(response);
      },
      goBack: () => {
        if (history.length >= 2) {
          history.pop();
          const entry = history[history.length - 1] ?? 'about:blank';
          address = entry;
          if (entry.includes('gone')) {
            // The page before is asked for again, and nothing answers now.
            emit('request', answer(entry).request());
            address = 'chrome-error://chromewebdata/';
          }
          const onBack = entry.startsWith('http')
            ? new URL(entry).searchParams.get('onback')
            : null;
          if (onBack !== null) {
            // The page comes back out of the browser's cache, no request,
            // and its own script sends the tab on at once; nothing answers.
            emit('request', answer(onBack).request());
          }
        }
        return Promise.resolve(null);
      },
      entryBefore: () =>
        Promise.resolve(history.length < 2 ? null : (history[history.length - 2] ?? null)),
      waitForSelector: () => Promise.resolve(null),
      waitForLoadState: () => Promise.resolve(),
      // A page that has loaded and holds no text: every other read finds nothing.
      evaluate: (expression: string) => {
        if (expression.includes('innerWidth')) {
          if (refreshTo !== null) {
            // The meta refresh sends the tab on while the hand reads the page.
            const to = refreshTo;
            refreshTo = null;
            emit('request', answer(to).request());
            address = to;
            history.push(to);
            return Promise.reject(
              new Error(
                'page.evaluate: Execution context was destroyed, most likely because of a navigation',
              ),
            );
          }
          if (readFailure !== null) {
            return Promise.reject(new Error(readFailure));
          }
        }
        return reading(
          (expression === 'document.readyState'
            ? 'complete'
            : expression === 'document.documentURI'
              ? (documentUri ?? address)
              : expression === 'document.hasFocus()'
                ? tabs.focused
                : '') as never,
        );
      },
      click: (selector: string) => {
        // A link with `target=_blank`: the page opens a tab, which the
        // browser shows in front of it (F184).
        const blank = /^#blank-(.+)$/.exec(selector)?.[1];
        if (blank !== undefined) {
          driven.input.push('driver click');
          tabs.open(site(`/${blank}`));
          return Promise.resolve();
        }
        // A link to an address nothing answers: the tab is sent there, and
        // the browser shows its own error page (F102).
        const link = /^a\[href="(.+)"\]$/.exec(selector)?.[1];
        if (link === undefined) {
          return refuse('driver click')();
        }
        driven.input.push('driver click');
        emit('request', answer(link).request());
        address = 'chrome-error://chromewebdata/';
        history.push(address);
        return Promise.resolve();
      },
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
    return page;
  };
  let current = makePage();
  const device: InputDevice = {
    screen: { width: 1920, height: 1080 },
    pointerPosition: () => Promise.resolve({ x: 960, y: 540 }),
    pointerMove: refuse('pointer move'),
    buttonDown: refuse('button down'),
    buttonUp: refuse('button up'),
    wheelNotch: refuse('wheel notch'),
    levelsOf: () => ({ shift: false, altGr: false, onLayout: true, keycode: null, compose: null }),
    prepare: () => Promise.resolve({ characters: [] }),
    unprepare: () => Promise.resolve(),
    keyDown: refuse('key down'),
    keyUp: refuse('key up'),
    flush: () => Promise.resolve(),
    close: () => Promise.resolve(),
  };
  const driver: SessionDriver = {
    get page() {
      return current;
    },
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
    freshPage: () => {
      tabs.fresh += 1;
      current = makePage();
      return Promise.resolve();
    },
    unansweredRedirects: () => unanswered,
    shownHead: (response) =>
      Promise.resolve(
        response.status() === 304
          ? {
              status: 200,
              headers: [
                { name: 'Content-Type', value: 'text/html' },
                { name: 'ETag', value: '"v1"' },
              ],
            }
          : null,
      ),
    replacedStatus: (url) => (url.includes('chrome403') ? 403 : null),
    otherTabs: () => tabs.opened.filter((entry) => !entry.closed).map((entry) => entry.tab),
    front: () => {
      tabs.fronts += 1;
      tabs.focused = tabs.frontable;
      return Promise.resolve();
    },
    onOpenedTab: (listener) => {
      reportTab = listener;
    },
  };
  return { driver, driven, tabs };
}

describe('the tab a request runs in (F96)', () => {
  const env: OperationEnv = {
    artifacts: new ArtifactWriter(path.join(tmpdir(), 'kage-tab-unused')),
    defaultArtifactPrefix: 'sidecar/test',
  };
  const capture = { dom: true, raw: false, xhr: null, console: false, screenshot: null, pdf: null };
  const session = () => {
    const { driver, driven, tabs } = scriptedDriver();
    const parts = { driver, personaSeed: PERSONA_SEED, selftestPages: [], log: silentLog };
    return { session: new PageSession(parts), driven, tabs };
  };
  const navigate = (on: PageSession, url: string, change: Partial<NavigateRequest> = {}) =>
    on.navigate({ url, wait: { until: 'load' }, capture, ...change }, env);
  const flow = async (on: PageSession, actions: FlowRequest['actions']) => {
    const events: FlowEvent[] = [];
    await on.flow({ actions, capture }, env, (event) => events.push(event));
    const steps = events.filter((event) => event.type === 'step');
    const final = events.find((event) => event.type === 'final');
    if (final === undefined) {
      throw new Error('the flow sent no final event');
    }
    return { steps, final };
  };

  it('names where a redirect still under way went, for the page it left only (review of #95 and #101)', async () => {
    const unansweredAfter = async (actions: FlowRequest['actions']) => {
      const { session: on } = session();
      const run = await flow(on, actions);
      return run.final.result.unanswered_url ?? null;
    };
    const a = site('/a');
    const later = site('/silent');
    // The redirect's last hop, from the page the tab still shows.
    expect(
      await unansweredAfter([
        { action: 'goto', url: a },
        { action: 'goto', url: site(`/r?pending=${encodeURIComponent(later)}`), timeout_ms: 1000 },
      ]),
    ).toBe(later);
    // A second navigation started while a redirect was still under way: it
    // left the page the tab shows, not the first hop's own 302 address.
    expect(
      await unansweredAfter([
        { action: 'goto', url: a },
        { action: 'goto', url: site(`/r1?hop=${encodeURIComponent(site('/first'))}`) },
        { action: 'goto', url: site(`/r2?pending=${encodeURIComponent(later)}`), timeout_ms: 1000 },
      ]),
    ).toBe(later);
    // No site of any request, and the page itself: nothing named.
    for (const target of ['data:text/html,nothing', 'javascript:void(0)', a]) {
      expect(
        await unansweredAfter([
          { action: 'goto', url: a },
          {
            action: 'goto',
            url: site(`/r?pending=${encodeURIComponent(target)}`),
            timeout_ms: 1000,
          },
        ]),
        target,
      ).toBeNull();
    }
  });

  it('names a navigation the page that came back started itself, and nothing the way back answered (review of #101)', async () => {
    const unansweredAfter = async (actions: FlowRequest['actions']) => {
      const { session: on } = session();
      const run = await flow(on, actions);
      expect(run.final).toMatchObject({ ok: true });
      return run.final.result.unanswered_url ?? null;
    };
    const nowhere = site('/silent');
    // The way back answered: the page that came back names nothing.
    expect(
      await unansweredAfter([
        { action: 'goto', url: site('/a') },
        { action: 'goto', url: site('/b') },
        { action: 'back', human: false },
      ]),
    ).toBeNull();
    // Its own script sent the tab on since: that departure is named.
    expect(
      await unansweredAfter([
        { action: 'goto', url: site(`/kept?onback=${encodeURIComponent(nowhere)}`) },
        { action: 'goto', url: site('/b') },
        { action: 'back', human: false },
      ]),
    ).toBe(nowhere);
  });

  it('ends a load the browser turns into a download at once, in the driver’s words (F120)', async () => {
    const { session: on } = session();
    const began = Date.now();
    const saved = await navigate(on, site('/saves-file'), { timeout_ms: 30_000 });
    // The engines read the driver's words as `aborted`; Kage reads them as
    // a file the site answered with.
    expect(saved.error?.message).toContain('Download is starting');
    expect(saved).toMatchObject({ dom_html: null, raw_body: null });
    expect(Date.now() - began).toBeLessThan(5_000);
  });

  it('starts every flow in a fresh tab: back at its start goes nowhere, on the second flow as on the first', async () => {
    const { session: on, driven, tabs } = session();
    // Within a flow the tab holds the pages the flow loaded, and the way
    // back leads to the one before.
    const first = await flow(on, [
      { action: 'goto', url: site('/a') },
      { action: 'goto', url: site('/b') },
      { action: 'back', human: false },
    ]);
    expect(first.steps.map((step) => [step.action, step.ok])).toEqual([
      ['goto', true],
      ['goto', true],
      ['back', true],
    ]);
    expect(first.final).toMatchObject({ ok: true });
    expect(first.final.result.final_url).toBe(site('/a'));
    expect(tabs.fresh).toBe(0);

    // The next flow finds none of them: its first way back has no page to
    // go back to, nothing moves, and the flow ends there.
    const second = await flow(on, [
      { action: 'goto', url: site('/c') },
      { action: 'back' },
      { action: 'extract', capture },
    ]);
    expect(tabs.fresh).toBe(1);
    expect(second.steps.map((step) => [step.action, step.ok, step.error])).toEqual([
      ['goto', true, null],
      [
        'back',
        false,
        `there is no page to go back to: the history of the tab ends at ${site('/c')}`,
      ],
    ]);
    expect(second.final).toMatchObject({ ok: false });
    expect(second.final.result.final_url).toBe(site('/c'));
    // Said before any key is pressed: the keys never went looking.
    expect(driven.input).toEqual([]);
    expect(driven.loaded).toEqual([site('/a'), site('/b'), site('/c')]);
  });

  it('lets a flow read nothing of the navigation before it', async () => {
    const { session: on, tabs } = session();
    const before = await navigate(on, site('/private'));
    expect(before.dom_html).toContain('A page');
    const run = await flow(on, [
      { action: 'extract', capture },
      { action: 'assert', selector: 'p' },
    ]);
    expect(tabs.fresh).toBe(1);
    expect(run.steps[0]?.capture).toMatchObject({
      final_url: 'about:blank',
      status: null,
      title: null,
      dom_html: null,
    });
    expect(run.steps[1]).toMatchObject({
      action: 'assert',
      ok: false,
      error: 'no element matches p',
    });
    expect(run.final.result).toMatchObject({ final_url: 'about:blank', dom_html: null });
  });

  it('goes on in the tab only for a navigation that names the tab the last one named', async () => {
    const { session: on, tabs } = session();
    // The tab a context begins with is fresh: the first request takes it as it is.
    await navigate(on, site('/crawl/1'), { tab: 'job-1' });
    expect(tabs.fresh).toBe(0);
    // A crawl's next page: the same tab, the page before still in it.
    await navigate(on, site('/crawl/2'), { tab: 'job-1' });
    expect(tabs.fresh).toBe(0);
    // Another crawl, and a request that names no tab: a fresh one each.
    await navigate(on, site('/crawl/other'), { tab: 'job-2' });
    expect(tabs.fresh).toBe(1);
    await navigate(on, site('/plain'));
    expect(tabs.fresh).toBe(2);
    // The nameless request left the tab nobody's: the crawl does not find its pages again.
    await navigate(on, site('/crawl/3'), { tab: 'job-2' });
    expect(tabs.fresh).toBe(3);
    // A flow names no tab, and leaves it nobody's.
    await flow(on, [{ action: 'goto', url: site('/flow') }]);
    expect(tabs.fresh).toBe(4);
    await navigate(on, site('/crawl/4'), { tab: 'job-2' });
    expect(tabs.fresh).toBe(5);
  });
});

describe('a tab a flow’s page opens (F184)', () => {
  const env: OperationEnv = {
    artifacts: new ArtifactWriter(path.join(tmpdir(), 'kage-opened-unused')),
    defaultArtifactPrefix: 'sidecar/test',
  };
  const capture = { dom: true, raw: false, xhr: null, console: false, screenshot: null, pdf: null };
  const session = () => {
    const { driver, tabs } = scriptedDriver();
    const parts = { driver, personaSeed: PERSONA_SEED, selftestPages: [], log: silentLog };
    return { session: new PageSession(parts), tabs };
  };
  const flow = async (on: PageSession, actions: FlowRequest['actions']) => {
    const events: FlowEvent[] = [];
    await on.flow({ actions, capture }, env, (event) => events.push(event));
    const steps = events.filter((event) => event.type === 'step');
    const final = events.find((event) => event.type === 'final');
    if (final === undefined) {
      throw new Error('the flow sent no final event');
    }
    return { steps, final };
  };

  it('is reported by the step that opened it, kept behind the flow’s page, and closed with the flow', async () => {
    const { session: on, tabs } = session();
    // The round's flow (flows-02): a link that opens a tab, a wait, and
    // more steps on the page the flow is on.
    const run = await flow(on, [
      { action: 'goto', url: site('/a') },
      { action: 'click', selector: '#blank-terms', human: false },
      { action: 'wait_for', ms: 10 },
      { action: 'click', selector: 'a[href="https://www.example.org/b"]', human: false },
    ]);
    expect(run.steps.map((step) => [step.action, step.ok, step.opened_tabs])).toEqual([
      ['goto', true, []],
      ['click', true, [site('/terms')]],
      ['wait_for', true, []],
      ['click', true, []],
    ]);
    // The flow's page was put in front again as soon as the browser
    // reported the tab, and had the focus for the next step.
    expect(tabs.fronts).toBeGreaterThanOrEqual(1);
    // The tab ended with the flow, before the flow said it was done.
    expect(tabs.opened).toEqual([expect.objectContaining({ url: site('/terms'), closed: true })]);
    expect(run.final.ok).toBe(true);
  });

  it('fails the step whose input could not reach the flow’s page, and is closed all the same', async () => {
    const { session: on, tabs } = session();
    // A browser that keeps the tab in front, whatever is asked.
    tabs.frontable = false;
    const run = await flow(on, [
      { action: 'goto', url: site('/a') },
      { action: 'click', selector: '#blank-ad', human: false },
      { action: 'click', selector: 'a[href="https://www.example.org/b"]', human: false },
    ]);
    expect(run.steps.map((step) => [step.action, step.ok])).toEqual([
      ['goto', true],
      ['click', false],
    ]);
    expect(run.steps[1]?.error).toBe(
      `a tab the page opened (${site('/ad')}) stays in front of the flow's page`,
    );
    expect(run.steps[1]?.opened_tabs).toEqual([site('/ad')]);
    expect(run.final.ok).toBe(false);
    expect(tabs.opened.every((tab) => tab.closed)).toBe(true);
  });

  it('is closed at once when no flow runs, and before the next request when the browser reported it late', async () => {
    const { session: on, tabs } = session();
    await on.navigate({ url: site('/a'), wait: { until: 'load' }, capture }, env);
    // A tab that opens while no flow runs belongs to no request.
    tabs.open(site('/late'));
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(tabs.opened).toEqual([expect.objectContaining({ url: site('/late'), closed: true })]);
    // A request finds no tab of another's: one still open is closed first.
    tabs.opened.push({
      url: site('/unreported'),
      closed: false,
      tab: {
        url: () => site('/unreported'),
        close: () => {
          const entry = tabs.opened.at(-1);
          if (entry !== undefined) {
            entry.closed = true;
          }
          return Promise.resolve();
        },
      },
    });
    const run = await flow(on, [{ action: 'goto', url: site('/b') }]);
    expect(run.steps[0]).toMatchObject({ ok: true, opened_tabs: [] });
    expect(tabs.opened.every((tab) => tab.closed)).toBe(true);
  });
});

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

describe('a page that moves itself away under the hand (F192)', () => {
  const env: OperationEnv = {
    artifacts: new ArtifactWriter(path.join(tmpdir(), 'kage-moved-unused')),
    defaultArtifactPrefix: 'sidecar/test',
  };
  const capture = { dom: true, raw: false, xhr: null, console: false, screenshot: null, pdf: null };
  const visit = (url: string) => {
    const { driver, driven } = scriptedDriver();
    const parts = { driver, personaSeed: PERSONA_SEED, selftestPages: [], log: silentLog };
    const going = new PageSession(parts).navigate(
      {
        url,
        wait: { until: 'load' },
        capture,
        behavior: { seed: REQUEST_SEED, scroll_depth: 0.5, wander: false },
      },
      env,
    );
    return { going, driven };
  };
  const destroyed =
    'page.evaluate: Execution context was destroyed, most likely because of a navigation';

  it('ends the hand’s work there, and the capture says where the tab went', async () => {
    const elsewhere = 'http://rules.kage.test:8088/guard/secret';
    const { going, driven } = visit(site(`/m?refreshes=${encodeURIComponent(elsewhere)}`));
    const result = await going;
    expect(PageResultSchema.parse(result)).toEqual(result);
    expect(result.final_url).toBe(elsewhere);
    // The hand stopped at its first look: it sent nothing.
    expect(driven.input).toEqual([]);
    expect(result.behavior).toMatchObject({
      seed: REQUEST_SEED,
      dwell_ms: 0,
      scroll_reached: null,
      scroll_ended_by: null,
    });
  });

  it('fails as before where the read failed for another reason, or the page did not move', async () => {
    for (const failure of ['boom', destroyed]) {
      const { going } = visit(site(`/m?readbreaks=${encodeURIComponent(failure)}`));
      const thrown: unknown = await going.then(
        () => null,
        (error: unknown) => error,
      );
      expect(thrown).toBeInstanceOf(Error);
      expect(thrown).not.toBeInstanceOf(PageMoved);
      expect((thrown as Error).message).toBe(failure);
    }
  });
});

describe('a page the browser could not load (F102)', () => {
  const env: OperationEnv = {
    artifacts: new ArtifactWriter(path.join(tmpdir(), 'kage-unloaded-unused')),
    defaultArtifactPrefix: 'sidecar/test',
  };
  const capture = { dom: true, raw: true, xhr: null, console: false, screenshot: null, pdf: null };
  const session = () => {
    const { driver } = scriptedDriver();
    const parts = { driver, personaSeed: PERSONA_SEED, selftestPages: [], log: silentLog };
    return new PageSession(parts);
  };
  const navigate = (on: PageSession, url: string) =>
    on.navigate({ url, wait: { until: 'load' }, capture }, env);
  const sending = (path: string, key: string, to: string) =>
    site(`${path}?${key}=${encodeURIComponent(to)}`);

  it('is reported where the tab was sent, and the browser’s error page is no page of the site', async () => {
    const on = session();
    // A page whose script sends the tab to an address nothing answers.
    const moved = await navigate(on, sending('/p', 'moves', 'http://10.0.0.1:8091/x'));
    expect(PageResultSchema.parse(moved)).toEqual(moved);
    expect(moved).toMatchObject({
      final_url: 'http://10.0.0.1:8091/x',
      status: null,
      http_version: null,
      headers: [],
      title: null,
      dom_html: null,
      raw_body: null,
    });
    expect(moved.error).toMatchObject({ code: 'navigation_failed' });
    expect(moved.error?.message).toContain('http://10.0.0.1:8091/x');

    // A redirect the browser could not follow is a hop of the chain, and
    // the page is where it pointed.
    const start = sending('/r', 'redirect', 'http://169.254.169.254/latest/');
    const redirected = await navigate(on, start);
    expect(redirected.final_url).toBe('http://169.254.169.254/latest/');
    expect(redirected.redirects).toEqual([
      { url: start, status: 302, location: 'http://169.254.169.254/latest/' },
    ]);
    expect(redirected).toMatchObject({ status: null, dom_html: null });
    expect(redirected.error?.code).toBe('navigation_failed');

    // To an address the browser never asks for: where the redirect pointed.
    const scheme = await navigate(on, sending('/s', 'redirect', 'file:///etc/passwd'));
    expect(scheme.final_url).toBe('file:///etc/passwd');
    expect(scheme.redirects[0]?.location).toBe('file:///etc/passwd');
  });

  it('is Firefox’s error page by its document, under whatever address the tab shows (F167)', async () => {
    const on = session();
    // A meta refresh to a name the forwarder refused: Firefox keeps the
    // address in the tab and shows its error page as the document.
    const refused = 'http://m06.m1.test:8091/meta';
    const start = sending('/m', 'ffmoves', refused);
    const moved = await navigate(on, start);
    expect(PageResultSchema.parse(moved)).toEqual(moved);
    expect(moved).toMatchObject({
      final_url: refused,
      status: null,
      headers: [],
      title: null,
      dom_html: null,
      raw_body: null,
    });
    expect(moved.error).toMatchObject({ code: 'navigation_failed' });
    expect(moved.error?.message).toContain(refused);

    expect(
      errorPageTarget(`about:neterror?e=dnsNotFound&u=${encodeURIComponent(refused)}&c=UTF-8`),
    ).toBe(refused);
    expect(errorPageTarget('about:certerror?e=nssBadCert&u=')).toBeNull();
    expect(errorPageTarget('about:neterror?e=x&u=%E0%A4%A')).toBeNull();
    expect(errorPageTarget(refused)).toBeNull();
  });

  it('ends where Firefox shows its own page for an error answer, with the status it saw and nothing else (F189)', async () => {
    const on = session();
    const refused = site('/ffrefused/x');
    const started = Date.now();
    const result = await on.navigate(
      { url: refused, wait: { until: 'load' }, timeout_ms: 20_000, capture },
      env,
    );
    // At once, not at the timeout.
    expect(Date.now() - started).toBeLessThan(2_000);
    expect(PageResultSchema.parse(result)).toEqual(result);
    expect(result).toMatchObject({
      final_url: refused,
      status: 403,
      http_version: null,
      headers: [],
      title: null,
      dom_html: null,
      raw_body: null,
    });
    expect(result.error).toMatchObject({ code: 'navigation_failed' });
    expect(result.error?.message).toContain('answered with an error');

    // A flow's goto fails its step there as well.
    const events: FlowEvent[] = [];
    const flowStarted = Date.now();
    await on.flow({ actions: [{ action: 'goto', url: refused }], capture }, env, (event) =>
      events.push(event),
    );
    expect(Date.now() - flowStarted).toBeLessThan(2_000);
    const step = events.find((event) => event.type === 'step' && event.action === 'goto');
    expect(step).toMatchObject({ ok: false });
    expect(step?.type === 'step' ? step.error : null).toMatch(/^navigation_failed: /);

    // A page whose document moved on to an address that failed has its
    // own status; only the very document the error page names has one.
    const moved = await navigate(on, sending('/m', 'ffmoves', 'http://m.test:8091/x'));
    expect(moved.status).toBeNull();
  });

  it('ends at once where the page sent the tab on before its load and nothing answered there (F200)', async () => {
    const on = session();
    // A script or a form that sends the tab to an address the forwarder
    // refused: Firefox shows its own page there and fires no `load`.
    const refused = 'http://m06.m1.test:8091/secret';
    const start = sending('/s', 'ffsends', refused);
    const started = Date.now();
    const result = await on.navigate(
      { url: start, wait: { until: 'load' }, timeout_ms: 20_000, capture },
      env,
    );
    expect(Date.now() - started).toBeLessThan(2_000);
    expect(PageResultSchema.parse(result)).toEqual(result);
    expect(result).toMatchObject({
      final_url: refused,
      status: null,
      headers: [],
      title: null,
      dom_html: null,
      raw_body: null,
    });
    expect(result.error).toMatchObject({ code: 'navigation_failed' });
    expect(result.error?.message).toContain(
      `could not load ${refused}, where the page sent the tab`,
    );

    // A flow's goto fails its step there as well, and names where.
    const events: FlowEvent[] = [];
    const flowStarted = Date.now();
    await on.flow({ actions: [{ action: 'goto', url: start }], capture }, env, (event) =>
      events.push(event),
    );
    expect(Date.now() - flowStarted).toBeLessThan(2_000);
    const step = events.find((event) => event.type === 'step' && event.action === 'goto');
    expect(step).toMatchObject({ ok: false });
    expect(step?.type === 'step' ? step.error : null).toContain(
      `navigation_failed: the browser could not load ${refused}`,
    );
  });

  it('says the status of an empty error answer Chrome replaced with its own page, as its debugger heard it (F204)', async () => {
    const on = session();
    const walled = site('/chrome403/x');
    const result = await navigate(on, walled);
    expect(PageResultSchema.parse(result)).toEqual(result);
    expect(result).toMatchObject({
      final_url: walled,
      status: 403,
      headers: [],
      title: null,
      dom_html: null,
      raw_body: null,
    });
    expect(result.error).toMatchObject({ code: 'navigation_failed' });
    // Elsewhere the debugger names nothing, and no status is made up.
    const failed = await navigate(on, site('/fail/x'));
    expect(failed.status).toBeNull();
  });

  it('names a redirect the browser refused to follow and told only its debugger of (F167)', async () => {
    const on = session();
    for (const target of ['file:///etc/passwd', 'data:text/html,<b>x</b>', 'javascript:alert(1)']) {
      const start = sending('/u', 'refused', target);
      const refused = await navigate(on, start);
      expect(PageResultSchema.parse(refused)).toEqual(refused);
      expect(refused.redirects).toEqual([{ url: start, status: 302, location: target }]);
      expect(refused.final_url).toBe(new URL(target).href);
      expect(refused).toMatchObject({ status: null, dom_html: null, title: null });
      expect(refused.error?.code).toBe('navigation_failed');
    }
    // A page that arrived is what it is, whatever the debugger kept before.
    const arrived = await navigate(on, site('/fine'));
    expect(arrived).toMatchObject({ final_url: site('/fine'), status: 200, redirects: [] });
  });

  it('is where a flow ended when its page moved on to an address that did not load', async () => {
    const on = session();
    const events: FlowEvent[] = [];
    await on.flow(
      {
        actions: [
          { action: 'goto', url: sending('/start', 'moves', 'http://192.168.1.1/admin') },
          { action: 'extract', capture },
        ],
        capture,
      },
      env,
      (event) => events.push(event),
    );
    const step = events.find((event) => event.type === 'step' && event.action === 'extract');
    const final = events.find((event) => event.type === 'final');
    expect(step?.type === 'step' ? step.capture : null).toMatchObject({
      final_url: 'http://192.168.1.1/admin',
      dom_html: null,
      status: null,
    });
    expect(final?.type === 'final' ? final.result.final_url : null).toBe(
      'http://192.168.1.1/admin',
    );
  });

  const run = async (actions: FlowRequest['actions'], pace = true) => {
    const on = session();
    const events: FlowEvent[] = [];
    await on.flow(
      {
        actions,
        capture,
        // A paced flow watches a driver's click for the page it leads to.
        ...(pace ? { behavior: { pace: { dwell_ms: 0, min_dwell_ms: 0, wander: false } } } : {}),
      },
      env,
      (event) => events.push(event),
    );
    const steps = events.flatMap((event) =>
      event.type === 'step' ? [[event.action, event.ok, event.error]] : [],
    );
    const final = events.find((event) => event.type === 'final');
    return { steps, final: final?.type === 'final' ? final : null };
  };
  const refused = 'http://10.0.0.7/internal';
  const failed = (to: string) =>
    `navigation_failed: the browser could not load ${to}: it shows its own error page`;

  it('ends a flow where a click sent the tab when the browser could not load it', async () => {
    const clicked = await run([
      { action: 'goto', url: site('/start') },
      { action: 'click', selector: `a[href="${refused}"]`, human: false },
      { action: 'extract', capture },
    ]);
    expect(clicked.steps).toEqual([
      ['goto', true, null],
      ['click', false, failed(refused)],
    ]);
    expect(clicked.final).toMatchObject({ ok: false });
    expect(clicked.final?.result).toMatchObject({
      final_url: refused,
      status: null,
      dom_html: null,
      title: null,
    });
    expect(clicked.final?.result.error?.code).toBe('navigation_failed');

    // A click no pace watched: the flow ends at the next `goto`, before it
    // goes anywhere else, with the same address.
    const unwatched = await run(
      [
        { action: 'goto', url: site('/start') },
        { action: 'click', selector: `a[href="${refused}"]`, human: false },
        { action: 'goto', url: site('/elsewhere') },
      ],
      false,
    );
    expect(unwatched.steps).toEqual([
      ['goto', true, null],
      ['click', true, null],
      ['goto', false, failed(refused)],
    ]);
    expect(unwatched.final?.result.final_url).toBe(refused);
  });

  it('ends a flow where the way back sent the tab when the page before no longer loads', async () => {
    const gone = site('/gone');
    const back = await run([
      { action: 'goto', url: gone },
      { action: 'goto', url: site('/b') },
      { action: 'back', human: false },
      { action: 'extract', capture },
    ]);
    expect(back.steps).toEqual([
      ['goto', true, null],
      ['goto', true, null],
      ['back', false, failed(gone)],
    ]);
    expect(back.final?.result).toMatchObject({ final_url: gone, status: null, dom_html: null });
  });
});

describe('the page a result describes', () => {
  const env: OperationEnv = {
    artifacts: new ArtifactWriter(path.join(tmpdir(), 'kage-described-unused')),
    defaultArtifactPrefix: 'sidecar/test',
  };
  const capture = { dom: true, raw: false, xhr: null, console: false, screenshot: null, pdf: null };
  const session = () => {
    const { driver } = scriptedDriver();
    const parts = { driver, personaSeed: PERSONA_SEED, selftestPages: [], log: silentLog };
    return new PageSession(parts);
  };

  it('is the response the tab shows, where the driver says otherwise (F178, F190)', async () => {
    const on = session();
    // Answered `304`: the driver names the kept page's status and head.
    const kept = await on.navigate(
      { url: site('/revalidated'), wait: { until: 'load' }, capture },
      env,
    );
    expect(PageResultSchema.parse(kept)).toEqual(kept);
    expect(kept).toMatchObject({
      status: 200,
      headers: [
        { name: 'content-type', value: 'text/html' },
        { name: 'etag', value: '"v1"' },
      ],
    });
    // Elsewhere the driver's own.
    const plainPage = await on.navigate({ url: site('/a'), wait: { until: 'load' }, capture }, env);
    expect(plainPage).toMatchObject({
      status: 200,
      headers: [{ name: 'content-type', value: 'text/html' }],
    });
  });

  it('is named as it is at the capture, though the page moved itself on during the rest (F201)', async () => {
    const on = session();
    const there = site('/there');
    const address = site(`/here?later=${encodeURIComponent(there)}`);
    for (const rest of [
      { dwell_ms: 600 },
      { dwell_ms: 600, behavior: { seed: REQUEST_SEED, mode: 'still' as const } },
    ]) {
      const moved = await on.navigate(
        { url: address, wait: { until: 'load' }, capture, ...rest },
        env,
      );
      expect(moved).toMatchObject({ final_url: there, title: 'Where it went', error: null });
    }
    // Without a rest the page is captured as it was looked at.
    const at = await on.navigate({ url: address, wait: { until: 'load' }, capture }, env);
    expect(at).toMatchObject({ title: 'A page' });
  });
});

describe('a page that stops answering (F103)', () => {
  const env: OperationEnv = {
    artifacts: new ArtifactWriter(path.join(tmpdir(), 'kage-stuck-unused')),
    defaultArtifactPrefix: 'sidecar/test',
  };
  const capture = { dom: true, raw: true, xhr: null, console: false, screenshot: null, pdf: null };
  const session = (readLimitMs: number) => {
    const { driver, tabs } = scriptedDriver();
    const parts = {
      driver,
      personaSeed: PERSONA_SEED,
      selftestPages: [],
      log: silentLog,
      readLimitMs,
    };
    return { session: new PageSession(parts), tabs };
  };
  const navigate = (on: PageSession, url: string, change: Partial<NavigateRequest> = {}) =>
    on.navigate({ url, wait: { until: 'load' }, capture, ...change }, env);

  it('ends a navigation whose page never answers a read with a typed error within the bound, and serves the next in a fresh tab', async () => {
    const { session: on, tabs } = session(300);
    const began = Date.now();
    const stuck = await navigate(on, site('/stuck'), {
      behavior: { seed: REQUEST_SEED, mode: 'still' },
      dwell_ms: 200,
    });
    const took = Date.now() - began;
    // One read waits out the limit; every later read of the page fails at
    // once: the request is answered well within its own bound.
    expect(took).toBeLessThan(3000);
    expect(PageResultSchema.parse(stuck)).toEqual(stuck);
    expect(stuck.error).toMatchObject({ code: 'timeout' });
    expect(stuck.error?.message).toContain('did not answer a read within 300 ms');
    expect(stuck).toMatchObject({ dom_html: null, title: null, raw_body: null });

    // The page that does not answer is given up: the next request, even one
    // that names the tab, gets a fresh one and is served.
    const next = await navigate(on, site('/after'), { tab: 'job-1' });
    expect(tabs.fresh).toBe(1);
    expect(next.error).toBeNull();
    expect(next.dom_html).toContain('A page');
  });

  it('keeps a slow screenshot, and answers one the page never paints with a typed error and no artifact', async () => {
    const { driver, tabs } = scriptedDriver();
    const artifacts: OperationEnv = {
      artifacts: new ArtifactWriter(await mkdtemp(path.join(tmpdir(), 'kage-shots-'))),
      defaultArtifactPrefix: 'sidecar/test',
    };
    let paint: 'slow' | 'never' = 'slow';
    const shotIn = (ms: number) =>
      new Promise<{ data: Buffer; sideEffects: never[]; format: 'png' }>((resolve) => {
        setTimeout(() => {
          resolve({ data: Buffer.from('png'), sideEffects: [], format: 'png' });
        }, ms);
      });
    Object.assign(driver, {
      screenshot: () => (paint === 'slow' ? shotIn(300) : new Promise<never>(() => undefined)),
      pdf: () => new Promise<never>(() => undefined),
    });
    const on = new PageSession({
      driver,
      personaSeed: PERSONA_SEED,
      selftestPages: [],
      log: silentLog,
      readLimitMs: 200,
    });
    const shooting = (pdf: boolean) => ({
      ...capture,
      screenshot: { full_page: true, format: 'png' as const },
      pdf: pdf ? {} : null,
    });

    // Longer than a read, shorter than two: a long page painted, and kept.
    const slow = await on.navigate(
      { url: site('/long'), wait: { until: 'load' }, capture: shooting(false) },
      artifacts,
    );
    expect(slow.error).toBeNull();
    expect(slow.screenshot).not.toBeNull();
    expect(slow.dom_html).toContain('A page');

    // Never painted: no artifact, the page is one that does not answer, and
    // the request is answered typed, not thrown (F103).
    paint = 'never';
    const began = Date.now();
    const unpainted = await on.navigate(
      { url: site('/painted'), wait: { until: 'load' }, capture: shooting(false) },
      artifacts,
    );
    expect(Date.now() - began).toBeLessThan(3000);
    expect(PageResultSchema.parse(unpainted)).toEqual(unpainted);
    expect(unpainted.error).toMatchObject({ code: 'timeout' });
    expect(unpainted).toMatchObject({ screenshot: null, dom_html: null, raw_body: null });

    // A PDF the page never prints, the same; the next request is served.
    paint = 'slow';
    Object.assign(driver, {
      screenshot: () => shotIn(0),
    });
    const unprinted = await on.navigate(
      { url: site('/printed'), wait: { until: 'load' }, capture: shooting(true) },
      artifacts,
    );
    expect(unprinted.error).toMatchObject({ code: 'timeout' });
    expect(unprinted).toMatchObject({ pdf: null, dom_html: null });
    const next = await on.navigate({ url: site('/after'), wait: { until: 'load' }, capture }, env);
    expect(next.error).toBeNull();
    // Every request that names no tab began in a fresh one but the first.
    expect(tabs.fresh).toBe(3);
  });

  it('ends at once what waits for a renderer that crashed', async () => {
    const { session: on, tabs } = session(60_000);
    const began = Date.now();
    const crashed = await navigate(on, site('/crash'), {
      behavior: { seed: REQUEST_SEED, mode: 'still' },
      dwell_ms: 200,
    });
    // Not the read limit of a minute: the crash cut the reads.
    expect(Date.now() - began).toBeLessThan(5000);
    expect(crashed.error).toMatchObject({ code: 'navigation_failed' });
    expect(crashed.error?.message).toContain('crashed');
    expect(crashed.dom_html).toBeNull();
    const next = await navigate(on, site('/after'));
    expect(tabs.fresh).toBe(1);
    expect(next.error).toBeNull();
  });

  it('ends a flow on a page that stopped answering with the step that met it', async () => {
    const { session: on } = session(200);
    const events: FlowEvent[] = [];
    await on.flow(
      {
        actions: [
          { action: 'goto', url: site('/stuck') },
          { action: 'assert', selector: 'p' },
          { action: 'extract', capture },
        ],
        capture,
      },
      env,
      (event) => events.push(event),
    );
    const steps = events.filter((event) => event.type === 'step');
    expect(steps.map((step) => [step.action, step.ok])).toEqual([
      ['goto', true],
      ['assert', false],
    ]);
    const final = events.find((event) => event.type === 'final');
    expect(final?.type === 'final' ? final.ok : null).toBe(false);
    expect(final?.type === 'final' ? final.result.error?.code : null).toBe('timeout');
  });
});
