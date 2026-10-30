import { randomBytes } from 'node:crypto';

import {
  DEFAULT_CHALLENGE_WAIT_MS,
  DEFAULT_MAX_BODY_BYTES,
  DEFAULT_NAVIGATE_TIMEOUT_MS,
  MAX_PAGE_LINKS,
  MAX_URL_LENGTH,
  stillConflicts,
  type ArtifactRef,
  type BehaviorOptions,
  type BehaviorReport,
  type CaptureOptions,
  type Cookie,
  type Entry,
  type EntryReport,
  type ExportStateRequest,
  type ExportStateResponse,
  type FlowEvent,
  type FlowRequest,
  type ImportStateRequest,
  type ImportStateResponse,
  type NavigateRequest,
  type NavigationError,
  type PageLink,
  type PageResult,
  type PdfOptions,
  type RedirectHop,
  type RejectedCookie,
  type ScreenshotFormat,
  type ScreenshotOptions,
  type ScrollEnd,
  type SelftestPage,
  type SelftestRequest,
  type SelftestResponse,
  type SideEffect,
  type StorageEntry,
  type WaitUntil,
  type XhrCapture,
  type XhrKind,
} from '@kage/contract';

import {
  BehaviorError,
  Human,
  motorProfile,
  tempo,
  type BehaviorCounts,
  type Box,
  type InputDevice,
  type MotorProfile,
  type PageSurface,
  type Point,
  type SurfaceMetrics,
  type Tempo,
} from './behavior/index.js';
import {
  XhrCollector,
  captureBody,
  httpOrigin,
  matchesContentType,
  toHeaders,
  truncateText,
} from './capture.js';
import {
  challengeSignals,
  findInterstitial,
  waitForChallenge,
  type ChallengeWait,
  type PageObservation,
} from './challenge.js';
import {
  cookiesForOrigin,
  importRejection,
  toContractCookie,
  type BrowserCookie,
} from './cookies.js';
import type { ConsoleLog } from './console-log.js';
import { EngineError, type EngineLog, type OperationEnv } from './engine.js';
import {
  CAPTURE_RESERVE_MS,
  FlowStepError,
  runFlow,
  type FlowHost,
  type PageMark,
} from './flow/executor.js';
import { urlMatcher } from './flow/url-glob.js';
import { SeededRandom } from './seeded-random.js';
import type { TableRow } from './selftest/pages.js';
import { NavigationFailed, runSelftest, type LoadedPage } from './selftest/run.js';

/**
 * The part of a browser driver's page that the shared session uses. Both
 * engines drive their browser through a Playwright-compatible library, so
 * one description fits both; each engine hands its own page over.
 */
export interface DriverRequest {
  url(): string;
  method(): string;
  resourceType(): string;
  isNavigationRequest(): boolean;
  frame(): unknown;
  headers(): Record<string, string>;
  redirectedFrom(): DriverRequest | null;
  response(): Promise<DriverResponse | null>;
}

/** A response as the driver reports it. */
export interface DriverResponse {
  url(): string;
  status(): number;
  headers(): Record<string, string>;
  headersArray(): Promise<{ name: string; value: string }[]>;
  body(): Promise<Buffer>;
  request(): DriverRequest;
}

/** A frame of a page. */
export interface DriverFrame {
  url(): string;
}

type LoadState = 'commit' | 'domcontentloaded' | 'load' | 'networkidle';

/**
 * A page as the driver exposes it.
 *
 * The four members that read the page (`title`, `content`,
 * `waitForSelector`, `evaluate`) must leave it as it was: an isolated
 * world, and nothing a browser takes for its user (spec §10.2). A driver
 * that marks an evaluation as a user gesture does not qualify, so an
 * engine with such a driver hands over a page that reads another way
 * (engine-chromium's `pageWithReader`); the parity suite checks both.
 */
export interface DriverPage {
  url(): string;
  title(): Promise<string>;
  content(): Promise<string>;
  mainFrame(): DriverFrame;
  frames(): DriverFrame[];
  goto(
    url: string,
    options: { waitUntil: LoadState; timeout: number },
  ): Promise<DriverResponse | null>;
  /**
   * Goes back one history entry. The session asks for the commit only: a
   * page that comes back out of the browser's back/forward cache fires no
   * `load`, and a wait for one never ends (follow-up F21).
   */
  goBack(options: { waitUntil: LoadState; timeout: number }): Promise<DriverResponse | null>;
  waitForSelector(
    selector: string,
    options: { state: 'attached'; timeout: number },
  ): Promise<unknown>;
  waitForLoadState(
    state: 'domcontentloaded' | 'load' | 'networkidle',
    options: { timeout: number },
  ): Promise<void>;
  /** Runs an expression where the page cannot observe it: an isolated world, no user gesture. */
  evaluate<T>(expression: string): Promise<T>;
  click(selector: string, options: { timeout: number }): Promise<void>;
  hover(selector: string, options: { timeout: number }): Promise<void>;
  fill(selector: string, value: string, options: { timeout: number }): Promise<void>;
  selectOption(selector: string, value: string, options: { timeout: number }): Promise<unknown>;
  readonly keyboard: { press(key: string): Promise<void> };
  readonly mouse: { wheel(deltaX: number, deltaY: number): Promise<void> };
  on(event: 'request', listener: (request: DriverRequest) => void): unknown;
  on(event: 'response', listener: (response: DriverResponse) => void): unknown;
  on(event: 'domcontentloaded' | 'load', listener: () => void): unknown;
}

/** What only an engine knows how to do with its browser. */
export interface SessionDriver {
  readonly page: DriverPage;
  /** Turns what the driver threw for a navigation into the contract's error. */
  classifyError(thrown: unknown): NavigationError;
  /** Waits until the browser has settled after a failed navigation. */
  settleAfterFailure(error: NavigationError): Promise<void>;
  /**
   * Takes a screenshot. `format` is the format of the data, which is the
   * one asked for wherever the browser can encode it.
   */
  screenshot(
    options: ScreenshotOptions,
  ): Promise<{ data: Buffer; sideEffects: SideEffect[]; format: ScreenshotFormat }>;
  pdf(options: PdfOptions): Promise<{ data: Buffer; sideEffects: SideEffect[] }>;
  /** The protocol a response arrived over, in the short form (`h2`, `http/1.1`); `null` when unknown. */
  protocolOf(response: DriverResponse): Promise<string | null>;
  /** The log of the context: an operation reports the entries added while it ran. */
  readonly console: ConsoleLog;
  /** Every cookie of the browser. */
  cookies(): Promise<BrowserCookie[]>;
  /** Stores a cookie; false when the browser refused it. */
  setCookie(cookie: Cookie): Promise<boolean>;
  /** Where the browser window's top left corner really is on the display. */
  windowOrigin(): Point;
  /** Opens the mouse and keyboard of the context's display. */
  openInput(): Promise<InputDevice>;
}

/** What a session is made of. */
export interface SessionParts {
  readonly driver: SessionDriver;
  /** The persona's seed: its motor profile and every behavior stream derive from it. */
  readonly personaSeed: string;
  readonly selftestPages: readonly SelftestPage[];
  readonly log: EngineLog;
}

/** Longest page text read for interstitial detection and evidence. */
const TEXT_LIMIT = 200_000;
/** How long bodies of XHR responses are waited for once the page settled. */
const BODY_WAIT_MS = 5000;
/** How long a click is given to start a navigation. */
const CLICK_NAVIGATION_MS = 15_000;
/**
 * How long an address that changed is given to show its response. A
 * response comes before its document, so an address without one is a
 * document the browser kept or one a script renamed; the moment is for a
 * driver that reports the two the other way round.
 */
const RESPONSE_GRACE_MS = 200;
/** How long the driver's way back is given to show another page once the driver has answered. */
const BACK_SETTLE_MS = 1000;
/** How long a page is given to finish loading after a click or a key led to it. */
const ARRIVAL_LOAD_MS = 4000;
/**
 * How long a click on a link of the open page is watched once the page has
 * another address and the tab has asked for no document: a site that draws
 * its own pages changes its address and loads nothing, and no document
 * will come however long the wait.
 */
const SOFT_NAVIGATION_MS = 1500;
/** How often a page is asked, 40 ms apart, whether it shows the document a click was answered with. */
const DOCUMENT_LOOKS = 25;
/** How often the hand goes to a link of the open page before it gives up on resting on it. */
const HOVER_ATTEMPTS = 2;
/** How many addresses of documents a session remembers its tab to have shown. */
const SHOWN_LIMIT = 256;
/** How often a page that still glides from the last flick is asked where it is, and the rest between two looks. */
const SCROLL_REST_LOOKS = 12;
const SCROLL_REST_MS = 60;
/** Longest text of a link in a page result, and the bounds of its `rel`. */
const LINK_TEXT_LIMIT = 200;
const LINK_REL_TOKENS = 16;
const LINK_REL_TOKEN_LIMIT = 64;

const WAIT_UNTIL: Readonly<Record<WaitUntil, LoadState>> = {
  commit: 'commit',
  domcontentloaded: 'domcontentloaded',
  load: 'load',
  network_idle: 'networkidle',
};

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, Math.max(0, ms)));

const READ_TEXT = `(document.body ? document.body.innerText : '').slice(0, ${TEXT_LIMIT})`;
const READ_ROWS = `Array.from(document.querySelectorAll('tr')).slice(0, 2000).map((row) => {
  const cells = Array.from(row.querySelectorAll('td, th'));
  return { cells: cells.map((cell) => cell.innerText), classes: cells.map((cell) => cell.className) };
})`;
const READ_STORAGE = `({
  local: Object.keys(localStorage).map((name) => ({ name, value: localStorage.getItem(name) ?? '' })),
  session: Object.keys(sessionStorage).map((name) => ({ name, value: sessionStorage.getItem(name) ?? '' })),
})`;
const READ_METRICS = `({
  iw: innerWidth, ih: innerHeight, ow: outerWidth, oh: outerHeight,
  sy: Math.round(scrollY), sh: document.documentElement.scrollHeight,
})`;
/**
 * Which document a page shows: the address it was loaded from, which no
 * script can change afterwards (`history.pushState` changes the address of
 * a page and makes no other document of it), and the moment it began,
 * which no two documents share.
 */
const READ_SHOWN = `(() => {
  const entry = performance.getEntriesByType('navigation')[0];
  const from = entry && typeof entry.name === 'string' && entry.name !== '' ? entry.name : null;
  return { from, born: performance.timeOrigin };
})()`;
/** Words of the text nodes inside the viewport; a few thousand nodes at most are looked at. */
const READ_WORDS = `(() => {
  if (!document.body) return 0;
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  let seen = 0;
  let words = 0;
  while (walker.nextNode() && seen < 4000) {
    seen += 1;
    const text = walker.currentNode.nodeValue.trim();
    const parent = walker.currentNode.parentElement;
    if (text.length < 2 || !parent) continue;
    const rect = parent.getBoundingClientRect();
    if (rect.height === 0 || rect.width === 0) continue;
    const top = Math.max(0, rect.top);
    const bottom = Math.min(innerHeight, rect.bottom);
    if (bottom <= top) continue;
    words += (text.split(/\\s+/).length * (bottom - top)) / rect.height;
  }
  return Math.round(words);
})()`;

/**
 * Two frames of the page, or false at `limitMs`: from the reader's world,
 * whose animation frames and timers the page cannot see.
 */
const readPainted = (limitMs: number) => `new Promise((resolve) => {
  const limit = setTimeout(() => resolve(false), ${Math.max(0, Math.round(limitMs))});
  requestAnimationFrame(() => requestAnimationFrame(() => {
    clearTimeout(limit);
    resolve(true);
  }));
})`;

const boxExpression = (find: string) => `(() => {
  const element = ${find};
  if (!element) return null;
  const rect = element.getBoundingClientRect();
  if (rect.width === 0 || rect.height === 0) return null;
  return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
})()`;

/**
 * A selector the session understands besides CSS: the link an entry clicks.
 * What follows the prefix is JSON: the target, and how the link is
 * recognized ({@link LinkMode}, see {@link linkExpression}).
 */
const LINK_PREFIX = 'kage-link:';

/**
 * How an entry recognizes its link: `exact`, a link to the target itself
 * (the `link` entry); `host`, that one or else a link to another page of
 * the target's site (a result of a `search_engine` entry).
 */
type LinkMode = 'exact' | 'host';

/** An address as a link is compared by: without its fragment, and without the slash a path may end on. */
const PLAIN = `(url) => url.origin + url.pathname.replace(/\\/$/, '') + url.search`;

/**
 * Statements that define, for a target, `exactly(url)`: whether an address
 * is the target as a link is compared ({@link PLAIN}); and `leads(url)`:
 * whether a link to it is one the entry may click, by {@link LinkMode}. A
 * result leads to the target's site from a page of another site.
 */
const leadsTo = (target: string, mode: LinkMode) => `
  const plain = ${PLAIN};
  const wanted = new URL(${JSON.stringify(target)});
  const exactly = (url) => plain(url) === plain(wanted);
  const onSite = (url) => url.hostname === wanted.hostname && url.hostname !== location.hostname;
  const leads = (url) => exactly(url) || (${JSON.stringify(mode === 'host')} && onSite(url));
`;

/**
 * An address as the link of an entry is compared by: its origin, its path
 * without the slash it may end on, and its query; no fragment. `null` for
 * what is no address.
 */
export function plainAddress(address: string | null | undefined): string | null {
  try {
    const url = new URL(address ?? '');
    return url.origin + url.pathname.replace(/\/$/, '') + url.search;
  } catch {
    return null;
  }
}

/** Where an anchor leads, as the browser resolved it. An SVG anchor keeps its address in an object. */
const HREF = `(anchor) => new URL(typeof anchor.href === 'string' ? anchor.href : anchor.href.baseVal, document.baseURI)`;

/**
 * Statements that define `boxes(element)`: the boxes of an element a reader
 * can see, in pixels of the viewport, and none for an element nobody sees.
 *
 * A reader sees an element that is rendered (not `display: none`, not
 * `visibility: hidden`, not fully transparent, nor inside anything that
 * is), has a box that is not empty, and whose box lies on the canvas of
 * the document: not wholly left of or above its origin, and not beyond
 * what can be scrolled to. The browser's own `checkVisibility` answers the
 * first part where it takes the options for opacity and visibility; it is
 * asked which names it reads, because they were renamed once. Elsewhere the
 * computed styles of the element and its ancestors answer. An inline anchor
 * around a block has no box of its own and still one a reader sees, so
 * every client rectangle counts.
 *
 * A style sheet decides most of this, so only the browser knows; and all of
 * it is read, nothing is changed, in a world the page cannot look into.
 */
const SEEN = `
  const root = document.documentElement;
  const scroller = document.scrollingElement || root;
  const wide = Math.max(scroller.scrollWidth, root.clientWidth);
  const tall = Math.max(scroller.scrollHeight, root.clientHeight);
  // A document written from the right overflows to the left of its origin.
  const leftward = getComputedStyle(document.body || root).direction === 'rtl';
  const leftEdge = leftward ? root.clientWidth - wide : 0;
  const rightEdge = leftward ? root.clientWidth : wide;
  const read = [];
  const probe = {};
  for (const name of ['opacityProperty', 'visibilityProperty', 'checkOpacity', 'checkVisibilityCSS']) {
    Object.defineProperty(probe, name, { enumerable: true, get: () => { read.push(name); return true; } });
  }
  let asks = typeof root.checkVisibility === 'function';
  if (asks) {
    try { root.checkVisibility(probe); } catch (error) { asks = false; }
  }
  const checks = {};
  for (const name of read) checks[name] = true;
  const whole = asks
    && (read.includes('opacityProperty') || read.includes('checkOpacity'))
    && (read.includes('visibilityProperty') || read.includes('checkVisibilityCSS'));
  const styled = (element) => {
    if (getComputedStyle(element).visibility !== 'visible') return false;
    for (let node = element; node && node.nodeType === 1; node = node.parentElement) {
      const style = getComputedStyle(node);
      if (style.display === 'none' || Number(style.opacity) === 0) return false;
      if (node !== element && style.contentVisibility === 'hidden') return false;
    }
    return true;
  };
  const rendered = (element) => (whole ? element.checkVisibility(checks) : styled(element));
  const onCanvas = (rect) => rect.width > 0 && rect.height > 0
    && rect.right + scrollX > leftEdge && rect.left + scrollX < rightEdge
    && rect.bottom + scrollY > 0 && rect.top + scrollY < tall;
  const boxes = (element) => (rendered(element) ? Array.from(element.getClientRects()).filter(onCanvas) : []);
`;

/**
 * Reads the links of a document for a page result: every `a[href]` that
 * leads to an `http` or `https` address, in document order, with whether a
 * reader can see it ({@link SEEN}) and whether it stands in what a site
 * repeats on every page. One evaluation, so that every link is of the same
 * moment. An address longer than a navigation takes is left out: nobody
 * could ask for it.
 */
const READ_LINKS = `(() => {
  if (!document.documentElement) return { links: [], truncated: false };
  ${SEEN}
  const href = ${HREF};
  const furniture = 'nav, header, footer, aside, [role~="navigation" i], [role~="banner" i], [role~="contentinfo" i], [role~="complementary" i]';
  // Text as JSON can carry it: no half of a character pair, at the end or anywhere.
  const cut = (text, length) => {
    const sound = typeof text.toWellFormed === 'function' ? text.toWellFormed() : text;
    const short = sound.slice(0, length);
    const last = short.charCodeAt(short.length - 1);
    return sound.length > length && last >= 0xd800 && last <= 0xdbff ? short.slice(0, -1) : short;
  };
  const links = [];
  let truncated = false;
  for (const anchor of document.querySelectorAll('a[href]')) {
    let url;
    try { url = href(anchor); } catch (error) { continue; }
    if ((url.protocol !== 'http:' && url.protocol !== 'https:') || url.href.length > ${MAX_URL_LENGTH}) continue;
    if (links.length === ${MAX_PAGE_LINKS}) { truncated = true; break; }
    links.push({
      url: url.href,
      text: cut((anchor.textContent || '').replace(/\\s+/g, ' ').trim(), ${LINK_TEXT_LIMIT}).trimEnd(),
      rel: (anchor.getAttribute('rel') || '').toLowerCase().split(/\\s+/).filter(Boolean)
        .slice(0, ${LINK_REL_TOKENS}).map((token) => cut(token, ${LINK_REL_TOKEN_LIMIT})),
      visible: boxes(anchor).length > 0,
      furniture: anchor.closest(furniture) !== null,
    });
  }
  return { links, truncated };
})()`;

/**
 * Finds the link a reader would click to get to the target, and gives the
 * box to aim at: of the anchors a reader can see ({@link SEEN}) that lead
 * on in this tab (not a download, not another window), the first to
 * exactly that address, or with `host` when there is none the first that
 * leads to the target's site ({@link leadsTo}); and of its boxes the
 * largest. A link that runs over two lines has two boxes, and the
 * rectangle around both holds text that is no part of it.
 */
function linkExpression(target: string, mode: LinkMode): string {
  return `(() => {
    if (!document.documentElement) return null;
    ${SEEN}
    ${leadsTo(target, mode)}
    const href = ${HREF};
    const base = document.querySelector('base[target]');
    const here = (anchor) => {
      const name = (anchor.getAttribute('target') || (base && base.getAttribute('target')) || '').toLowerCase();
      return !anchor.hasAttribute('download') && ['', '_self', '_top', '_parent'].includes(name);
    };
    const first = (wanted) => {
      for (const anchor of document.querySelectorAll('a[href]')) {
        let url;
        try { url = href(anchor); } catch (error) { continue; }
        if (!wanted(url) || !here(anchor)) continue;
        const seen = boxes(anchor);
        if (seen.length === 0) continue;
        const box = seen.reduce((best, rect) => (rect.width * rect.height > best.width * best.height ? rect : best));
        return { x: box.x, y: box.y, width: box.width, height: box.height };
      }
      return null;
    };
    return first(exactly) || first(leads);
  })()`;
}

/** Whether what lies at a point of the viewport is a link the entry may click ({@link leadsTo}), or a part of one. */
function linkAtExpression(target: string, mode: LinkMode, at: Point): string {
  return `(() => {
    ${leadsTo(target, mode)}
    const href = ${HREF};
    const hit = document.elementFromPoint(${Math.round(at.x)}, ${Math.round(at.y)});
    const anchor = hit && typeof hit.closest === 'function' ? hit.closest('a[href]') : null;
    if (!anchor) return false;
    try { return leads(href(anchor)); } catch (error) { return false; }
  })()`;
}

/**
 * What the tab holds before the page that is open, as far as the page can
 * say without moving. The Navigation API lists the entries of the page's
 * own origin around the open one, with their addresses: `before` holds
 * those before it, the oldest first. An entry of another origin is not
 * listed, and the address of one whose document hides its referrer is
 * `null`. `listed` is how many entries the API lists and `total` how many
 * the tab has in all. `api` is false in a browser without the Navigation
 * API, which then knows the number only.
 */
const READ_TAB = `(() => {
  const total = history.length;
  try {
    const api = globalThis.navigation;
    if (api && typeof api.entries === 'function' && api.currentEntry && api.currentEntry.index >= 0) {
      const entries = Array.from(api.entries());
      const before = entries.slice(0, api.currentEntry.index)
        .map((entry) => (typeof entry.url === 'string' ? entry.url : null));
      return { api: true, total, listed: entries.length, before };
    }
  } catch (error) {}
  return { api: false, total, listed: 0, before: [] };
})()`;

/** What a page can say of the history of its tab without moving in it. */
export interface TabHistory {
  /** Whether the browser has the Navigation API. */
  readonly api: boolean;
  /** How many entries the tab has in all, before and after the open one. */
  readonly total: number;
  /** How many of them the Navigation API lists: those of the open page's origin around it. */
  readonly listed: number;
  /** The addresses of the listed entries before the open one, the oldest first; `null` for one that is kept back. */
  readonly before: readonly (string | null)[];
}

/**
 * How many entries a persona goes back at most to find a page in its tab,
 * found out without moving wherever the browser can say (follow-up F28).
 *
 * The Navigation API names the entries of the open page's origin. An entry
 * it does not name (one of another origin, or one whose document hides its
 * address) can only be looked at by going there, and so can every entry in
 * a browser without that API. Nobody goes looking for a page the tab never
 * showed, so an entry without a name is only tried when a document at that
 * address was loaded in this tab. (Chrome keeps the blank page a tab began
 * with as an entry, which is where a search through unnamed entries ends.)
 *
 * @param wanted the page, as {@link plainAddress} writes it.
 * @param reach how many entries back the request allows.
 * @param shown whether the tab was ever answered with a document at that address.
 * @returns 0 when the tab does not hold the page within reach, and then
 *   nothing moves. Otherwise the number of entries to go back, looking at
 *   the address after each one and staying at the first that is the page.
 */
export function entriesToGoBack(
  tab: TabHistory,
  wanted: string,
  reach: number,
  shown: boolean,
): number {
  if (reach <= 0) {
    return 0;
  }
  if (!tab.api) {
    return shown && tab.total > 1 ? reach : 0;
  }
  const named = Math.min(reach, tab.before.length);
  let hidden = false;
  for (let back = 1; back <= named; back += 1) {
    const address = tab.before[tab.before.length - back] ?? null;
    if (address === null) {
      hidden = true;
    } else if (plainAddress(address) === wanted) {
      // The nearest entry with that address; a hidden one before it is
      // looked at on the way.
      return back;
    }
  }
  const unnamed = Math.max(0, tab.total - tab.listed);
  if (!shown || (!hidden && (named === reach || unnamed === 0))) {
    // Every entry within reach has a name and none is the page, or the tab
    // never showed it.
    return 0;
  }
  return Math.min(reach, tab.before.length + unnamed);
}

/** A request's XHR filters, and whether a response is wanted by any of them. */
function wantedByAny(filters: readonly XhrCapture[], contentType: string | undefined): boolean {
  return filters.some((filter) => matchesContentType(contentType, filter.content_types));
}

interface RecordedXhr {
  readonly url: string;
  readonly method: string;
  readonly kind: XhrKind;
  readonly status: number;
  readonly contentType: string | null;
  body: Uint8Array | null;
}

/** What is known about the document a page shows, filled in by listeners while it loads. */
interface DocumentState {
  response: DriverResponse | null;
  /**
   * The address of a document that is shown although no response of this
   * flow came with it: the browser kept it from before the flow and
   * brought it back. `null` for every other document.
   */
  restored: string | null;
  started: number;
  ttfb: number | null;
  domContentLoaded: number | null;
  load: number | null;
  readonly scriptUrls: Set<string>;
  readonly xhr: RecordedXhr[];
  readonly bodies: Promise<void>[];
  /** Whether the proxy asked for credentials on the way to this document. */
  proxyChallenged: boolean;
}

const newDocument = (restored: string | null = null): DocumentState => ({
  response: null,
  restored,
  started: Date.now(),
  ttfb: null,
  domContentLoaded: null,
  load: null,
  scriptUrls: new Set(),
  xhr: [],
  bodies: [],
  proxyChallenged: false,
});

const withoutFragment = (url: string) => url.split('#')[0] ?? '';

/** The address a state's document was loaded from, without its fragment; `null` for a state without a document. */
const loadedFrom = (state: DocumentState): string | null =>
  state.response !== null ? withoutFragment(state.response.url()) : state.restored;

const emptyObservation: PageObservation = {
  title: null,
  text: null,
  scriptUrls: [],
  frameUrls: [],
};

/** How a capture's files are named. */
interface CaptureNames {
  readonly prefix: string;
  readonly stamp: string;
}

/** What came of a way back in the tab. */
type WentBack =
  /** The page before is there and has loaded. */
  | 'arrived'
  /** The tab has no entry before the open one. */
  | 'nowhere'
  /** The browser asked for the page before and got no answer in time. */
  | 'unanswered'
  /** The page before came and did not finish loading in time. */
  | 'unfinished';

/** How the scrolling of a navigation ended, for its report. */
interface ScrollReport {
  readonly reached: number | null;
  readonly endedBy: ScrollEnd;
}

/**
 * A page that was asked for by a click or a key did not come in time.
 * Named as the drivers name their own timeouts, so that every engine
 * classifies it like one of those.
 */
class ArrivalTimeout extends Error {
  override readonly name = 'TimeoutError';
}

/**
 * A click on a link was answered, and the answer became no page. Worded as
 * the drivers word a navigation that turns into a download, so that every
 * engine classifies it like one of those: `aborted`.
 */
class NoPage extends Error {
  override readonly name = 'NoPage';

  constructor() {
    super('the link led to no page: a download is starting, or the answer held no document');
  }
}

/** Counts the times of a document from another moment than the one it was first noted at. */
function countFrom(state: DocumentState, moment: number): void {
  const later = moment - state.started;
  state.started = moment;
  for (const mark of ['ttfb', 'domContentLoaded', 'load'] as const) {
    const time = state[mark];
    if (time !== null) {
      state[mark] = Math.max(0, time - later);
    }
  }
}

/** What a still visit (contract 1.5.0) needs to be reported: its seed. */
interface StillVisit {
  readonly seed: string;
}

/**
 * Takes a still visit: the request's seed, or one drawn as for every other
 * behavior. The contract refuses a still visit that also asks for input;
 * the engine refuses it again, so that no caller that skipped the contract
 * gets a hand it did not ask for, nor a still visit that ignored part of
 * what it asked.
 *
 * @throws {EngineError} `invalid_request`, naming every field a still visit cannot have.
 */
function stillVisit(behavior: BehaviorOptions): StillVisit {
  const conflicts = stillConflicts(behavior);
  if (conflicts.length > 0) {
    throw new EngineError(
      'invalid_request',
      conflicts
        .map((conflict) => `${['behavior', ...conflict.path].join('.')}: ${conflict.message}`)
        .join('; '),
    );
  }
  return { seed: behavior.seed ?? randomBytes(32).toString('hex') };
}

/** The counts of a visit that sent no input at all. */
const noInput = (): BehaviorCounts => ({
  moves: 0,
  positions: 0,
  overshoots: 0,
  clicks: 0,
  notches: 0,
  keys: 0,
  typos: 0,
  wanders: 0,
});

/**
 * One open persona context as both engines run it: navigation with its
 * captures, the behavior engine, flows, state export and import, and the
 * self-test (spec §10.2 to §10.5).
 *
 * Everything that differs between Chrome and Firefox is behind
 * {@link SessionDriver}; everything here is the same code for both, which
 * is what keeps the engines' results comparable.
 */
export class PageSession {
  private document: DocumentState = newDocument();
  /** Filters whose XHR bodies are read while a page loads. */
  private filters: readonly XhrCapture[] = [];
  private input: InputDevice | null = null;
  private profile: MotorProfile | null = null;
  private tempo: Tempo | null = null;
  /**
   * The documents a flow has left, by the address each was loaded from
   * (without its fragment); `null` outside a flow. A page the browser
   * brings back out of its back/forward cache comes without a response:
   * its state is found here, so that a capture reports its own status,
   * headers and requests and not those of the page that was left
   * (follow-up F21).
   */
  private remembered: Map<string, DocumentState> | null = null;
  /**
   * Whether the next document replaces what was collected. A plain
   * navigation keeps collecting across the documents it passes through (a
   * challenge page and the page behind it are one visit); a click that
   * leaves a page starts anew.
   */
  private freshOnNavigation: 'never' | 'once' | 'always' = 'never';
  /**
   * How many documents the tab has asked for. A way back that is not the
   * driver's returns before the browser has an answer; a request that went
   * out since says that one is on its way (follow-up F38).
   */
  private documentsAsked = 0;
  /**
   * The addresses of the documents this tab was answered with, as
   * {@link plainAddress} writes them, the latest last: what the tab can
   * hold at all. Kept short; a page from long ago is out of reach anyway.
   */
  private readonly shown = new Set<string>();

  constructor(private readonly parts: SessionParts) {
    const { page } = parts.driver;
    page.on('request', (asked) => {
      if (asked.resourceType() === 'script') {
        this.document.scriptUrls.add(asked.url());
      }
      try {
        if (asked.isNavigationRequest() && asked.frame() === this.page.mainFrame()) {
          this.documentsAsked += 1;
        }
      } catch {
        // A request without a frame (a service worker's) is no navigation of the page.
      }
    });
    page.on('response', (response) => {
      this.onResponse(response);
    });
    page.on('domcontentloaded', () => {
      this.document.domContentLoaded ??= Date.now() - this.document.started;
    });
    page.on('load', () => {
      this.document.load ??= Date.now() - this.document.started;
    });
  }

  private get page(): DriverPage {
    return this.parts.driver.page;
  }

  private onResponse(response: DriverResponse): void {
    const asked = response.request();
    const navigation = (() => {
      try {
        // A request without a frame (a service worker's) is no navigation of the page.
        return asked.isNavigationRequest() && asked.frame() === this.page.mainFrame();
      } catch {
        return false;
      }
    })();
    if (navigation && response.status() === 407) {
      // The proxy asking for credentials is no document: the browser
      // answers it and asks again, or the navigation fails.
      this.document.proxyChallenged = true;
      return;
    }
    if (navigation) {
      const address = plainAddress(response.url());
      if (address !== null) {
        // Said again, an address moves to the end: the latest stay.
        this.shown.delete(address);
        this.shown.add(address);
        if (this.shown.size > SHOWN_LIMIT) {
          this.shown.delete(this.shown.values().next().value ?? address);
        }
      }
      if (
        this.freshOnNavigation !== 'never' &&
        asked.redirectedFrom() === null &&
        loadedFrom(this.document) !== null
      ) {
        // A new document after a click: what was collected belongs to the page that was left.
        this.leave(this.document);
        this.document = newDocument();
        if (this.freshOnNavigation === 'once') {
          this.freshOnNavigation = 'never';
        }
      }
      // The latest one counts: after a challenge resolved itself, the
      // document behind it is the result.
      this.document.response = response;
      this.document.ttfb ??= Date.now() - this.document.started;
      return;
    }
    const kind = asked.resourceType();
    const contentType = response.headers()['content-type'];
    if ((kind === 'xhr' || kind === 'fetch') && wantedByAny(this.filters, contentType)) {
      const record: RecordedXhr = {
        url: response.url(),
        method: asked.method(),
        kind,
        status: response.status(),
        contentType: contentType ?? null,
        body: null,
      };
      const state = this.document;
      state.xhr.push(record);
      // The body is read now: after the next navigation the browser no longer holds it.
      state.bodies.push(
        response.body().then(
          (body) => {
            record.body = body;
          },
          () => undefined,
        ),
      );
    }
  }

  /** Starts collecting for a new document, for the request's XHR filters. */
  private begin(filters: readonly (XhrCapture | null)[]): void {
    this.filters = filters.filter((filter): filter is XhrCapture => filter !== null);
    this.document = newDocument();
    this.freshOnNavigation = 'never';
  }

  private read<T>(expression: string): Promise<T> {
    return this.page.evaluate<T>(expression);
  }

  // --- the document a flow is on (follow-up F21) --------------------------

  /** Keeps the state of a document a flow leaves, for the day the browser brings the page back. */
  private leave(state: DocumentState): void {
    const from = loadedFrom(state);
    if (from !== null) {
      this.remembered?.set(from, state);
    }
  }

  /**
   * Waits until the page shows another document than at the mark.
   *
   * @returns `answered` when a response came with it, `unanswered` when
   * only the address changed (a page the browser kept, or one a script
   * gave another address), `null` when the page was still the same at the
   * limit.
   */
  private async departed(
    since: PageMark,
    limit: number,
  ): Promise<'answered' | 'unanswered' | null> {
    const answered = () => this.document.response !== since.response;
    while (!answered() && this.page.url() === since.address) {
      if (Date.now() > limit) {
        return null;
      }
      await sleep(40);
    }
    const grace = Date.now() + RESPONSE_GRACE_MS;
    while (!answered() && Date.now() < grace) {
      await sleep(20);
    }
    return answered() ? 'answered' : 'unanswered';
  }

  /**
   * Makes the state of the document the page shows the current one, when
   * the page shows another document than the current state describes and
   * no response announced it: the remembered state of a page the flow has
   * been on, or a state without a response for one it has not.
   *
   * The page says which document it is by the address it was loaded from.
   * The address of the page does not: a script can change it without
   * making another document (`history.pushState`).
   */
  private async adoptShown(): Promise<void> {
    const state = this.document;
    const memory = this.remembered;
    if (memory === null) {
      return;
    }
    const shown = await this.read<{ from: string | null }>(READ_SHOWN).catch(() => null);
    if (this.document !== state) {
      // A response arrived meanwhile: the page is a new document after all.
      return;
    }
    const from = withoutFragment(shown?.from ?? this.page.url());
    if (from === loadedFrom(state)) {
      return;
    }
    this.leave(state);
    // Only a web page counts as a document that came back: the browser's
    // own pages (a blank one, an error) are nothing a capture describes.
    this.document = memory.get(from) ?? newDocument(/^https?:\/\//i.test(from) ? from : null);
  }

  /**
   * Waits until the document the page shows has loaded, by asking the page
   * itself. A driver's wait for `load` does not do: a page that comes back
   * out of the back/forward cache fires none.
   *
   * @returns whether it had loaded by the deadline.
   */
  private async loaded(deadline: number): Promise<boolean> {
    for (;;) {
      let timer: NodeJS.Timeout | undefined;
      // A page whose script never yields answers no read: the wait still ends.
      const late = new Promise<null>((resolve) => {
        timer = setTimeout(
          () => {
            resolve(null);
          },
          Math.max(250, deadline - Date.now()),
        );
      });
      const state = await Promise.race([
        this.read<string>('document.readyState').catch(() => null),
        late,
      ]).finally(() => {
        clearTimeout(timer);
      });
      if (state === 'complete') {
        return true;
      }
      if (Date.now() >= deadline) {
        return false;
      }
      await sleep(40);
    }
  }

  /** Notes which document the page shows, for {@link PageSession.departed}. */
  private mark(): PageMark {
    return { response: this.document.response, address: this.page.url() };
  }

  /**
   * Goes back one entry of the tab and waits for the page before: by the
   * driver, or by `by` when it is given (the persona's keys). One watch for
   * both, for a flow's `back` and for an entry that goes back to click
   * (follow-ups F21, F38, F28): nothing answers for keys, so what the page
   * shows afterwards is all that is known about where they led.
   *
   * @param by what asks the browser to go back in place of the driver.
   */
  private async goBack(timeout: number, by?: () => Promise<void>): Promise<WentBack> {
    const deadline = Date.now() + timeout;
    const since = this.mark();
    const before = await this.read<{ born: number }>(READ_SHOWN).catch(() => null);
    const asked = this.documentsAsked;
    if (by === undefined) {
      // The driver is asked for the commit only, and the page itself for
      // the rest: a page that comes back out of the back/forward cache
      // fires no `load`, and the driver's wait for one never ends.
      await this.page.goBack({ waitUntil: 'commit', timeout });
    } else {
      await by();
    }
    let how = await this.departed(since, Math.min(deadline, Date.now() + BACK_SETTLE_MS));
    if (how === null && this.documentsAsked !== asked) {
      // The browser has asked for the page before and holds no answer
      // yet: keys return at once, where the driver waits for the commit.
      how = await this.departed(since, deadline);
      if (how === null) {
        return 'unanswered';
      }
    }
    if (how === null) {
      // Neither a response nor another address. The entry before may
      // still be another document at this very address; the page knows.
      const after = await this.read<{ born: number }>(READ_SHOWN).catch(() => null);
      if (before === null || after === null || after.born === before.born) {
        return 'nowhere';
      }
      how = 'unanswered';
    }
    if (how === 'unanswered') {
      await this.adoptShown();
    }
    return (await this.loaded(deadline)) ? 'arrived' : 'unfinished';
  }

  private async observe(): Promise<PageObservation> {
    const main = this.page.mainFrame();
    const [title, text] = await Promise.all([
      this.page.title().catch(() => null),
      this.read<string>(READ_TEXT).catch(() => null),
    ]);
    return {
      title,
      text,
      scriptUrls: [...this.document.scriptUrls],
      frameUrls: this.page
        .frames()
        .filter((frame) => frame !== main)
        .map((frame) => frame.url())
        .filter((url) => url !== '' && url !== 'about:blank' && !url.startsWith('about:srcdoc')),
    };
  }

  private async redirects(response: DriverResponse | null): Promise<RedirectHop[]> {
    const hops: RedirectHop[] = [];
    let request: DriverRequest | null = response?.request().redirectedFrom() ?? null;
    while (request !== null && hops.length < 50) {
      const answered = await request.response().catch(() => null);
      hops.unshift({
        url: request.url(),
        status: answered?.status() ?? 0,
        location: answered?.headers()['location'] ?? null,
      });
      request = request.redirectedFrom();
    }
    return hops;
  }

  // --- behavior ---------------------------------------------------------

  private readonly surface: PageSurface = {
    metrics: async (): Promise<SurfaceMetrics> => {
      const m =
        await this.read<Record<'iw' | 'ih' | 'ow' | 'oh' | 'sy' | 'sh', number>>(READ_METRICS);
      const window = this.parts.driver.windowOrigin();
      // A window has its side borders left and right and everything else
      // (title, tabs, toolbar) above the page.
      const border = Math.max(0, Math.round((m.ow - m.iw) / 2));
      return {
        origin: { x: window.x + border, y: window.y + Math.max(0, m.oh - m.ih - border) },
        viewport: { width: m.iw, height: m.ih },
        scrollY: m.sy,
        scrollHeight: m.sh,
      };
    },
    visibleWords: () => this.read<number>(READ_WORDS),
    boxOf: (selector: string) => {
      if (selector.startsWith(LINK_PREFIX)) {
        const { target, mode } = JSON.parse(selector.slice(LINK_PREFIX.length)) as {
          target: string;
          mode: LinkMode;
        };
        return this.read<Box | null>(linkExpression(target, mode));
      }
      return this.read<Box | null>(
        boxExpression(`document.querySelector(${JSON.stringify(selector)})`),
      );
    },
    painted: (limitMs: number) => this.read<boolean>(readPainted(limitMs)),
  };

  /**
   * The persona at its mouse and keyboard for one request.
   *
   * @param seed the request's seed; drawn here when the request has none.
   */
  private async human(seed: string | undefined): Promise<{ human: Human; seed: string }> {
    const requestSeed = seed ?? randomBytes(32).toString('hex');
    this.input ??= await this.parts.driver.openInput();
    this.profile ??= motorProfile(this.parts.personaSeed);
    this.tempo ??= tempo(this.parts.personaSeed);
    return {
      seed: requestSeed,
      human: new Human({
        device: this.input,
        surface: this.surface,
        profile: this.profile,
        tempo: this.tempo,
        random: new SeededRandom(this.parts.personaSeed, `behavior:${requestSeed}`),
      }),
    };
  }

  /**
   * @param patience the patience the request gave the scrolling; `null` without one, and for a flow.
   * @param scroll how the scrolling of a navigation ended; `null` when none was asked for, and for a flow.
   */
  private report(
    seed: string,
    counts: BehaviorCounts,
    dwellMs: number,
    entry: EntryReport | null,
    patience: number | null = null,
    scroll: ScrollReport | null = null,
  ): BehaviorReport {
    return {
      seed,
      profile_id: (this.profile ?? motorProfile(this.parts.personaSeed)).id,
      entry,
      dwell_ms: Math.round(dwellMs),
      ...counts,
      patience_ms: patience,
      scroll_reached: scroll?.reached ?? null,
      scroll_ended_by: scroll?.endedBy ?? null,
    };
  }

  /**
   * How far down its document the page is, as a share of what can be
   * scrolled: 0 at the top, 1 at the end, and 1 for a document that ends
   * inside its window, of which there is nothing more to see.
   *
   * @param gliding whether the wheel was turned a moment ago. The browser
   *   glides to where a flick sent it, so the page is asked until two looks
   *   in a row find it in the same place.
   * @returns `null` when the page could not be read.
   */
  private async scrolled(gliding: boolean): Promise<number | null> {
    let metrics = await this.surface.metrics().catch(() => null);
    for (let look = 0; gliding && metrics !== null && look < SCROLL_REST_LOOKS; look += 1) {
      await sleep(SCROLL_REST_MS);
      const next = await this.surface.metrics().catch(() => null);
      const rests = next?.scrollY === metrics.scrollY;
      metrics = next ?? metrics;
      if (rests || next === null) {
        break;
      }
    }
    if (metrics === null) {
      return null;
    }
    const scrollable = metrics.scrollHeight - metrics.viewport.height;
    if (scrollable <= 0) {
      return 1;
    }
    return Math.round(Math.min(1, Math.max(0, metrics.scrollY / scrollable)) * 10_000) / 10_000;
  }

  /**
   * Waits until a click has started a new document, then for the state the request names.
   *
   * @param idle asked while no document has come: `true` ends the wait, for
   *   a caller that knows that none will.
   */
  private async afterClick(
    previous: DriverResponse | null,
    until: WaitUntil,
    deadline: number,
    idle: () => boolean = () => false,
  ) {
    const limit = Math.min(deadline, Date.now() + CLICK_NAVIGATION_MS);
    while (this.document.response === previous || this.document.response === null) {
      if (Date.now() > limit || idle()) {
        return false;
      }
      await sleep(40);
    }
    const state = WAIT_UNTIL[until];
    if (state !== 'commit') {
      await this.page.waitForLoadState(state, { timeout: Math.max(1, deadline - Date.now()) });
    }
    return true;
  }

  /**
   * Arrives at the target the way the entry says (spec §10.4): loads the
   * page to start from as a typed navigation, finds the link to the target
   * and clicks it as on a page the persona has been reading
   * ({@link Session.clickThrough}). The `Referer` is whatever the browser
   * sends for that click; nothing is ever forged.
   *
   * @throws {ArrivalTimeout} when the page the link leads to was asked for and did not come in time.
   * @throws {NoPage} when the click was answered with no page.
   */
  private async enter(
    entry: Entry,
    target: string,
    until: WaitUntil,
    human: Human,
    deadline: number,
  ): Promise<EntryReport> {
    const from = entry.from_url ?? null;
    const report = (outcome: EntryReport['outcome'], referer: string | null): EntryReport => ({
      strategy: entry.strategy,
      outcome,
      from_url: from,
      referer,
      backs: null,
    });
    if (from === null) {
      throw new EngineError('invalid_request', `the entry ${entry.strategy} needs from_url`);
    }
    const left = () => Math.max(1, deadline - Date.now());
    const first = await this.page
      .goto(from, { waitUntil: 'load', timeout: left() })
      .catch(() => null);
    const observation = await this.observe().catch(() => emptyObservation);
    if (first === null || first.status() >= 400 || findInterstitial(observation) !== null) {
      // The page to start from did not load, or shows a wall. Nothing is
      // typed or clicked on it.
      return report('blocked', null);
    }
    const mode: LinkMode = entry.strategy === 'search_engine' ? 'host' : 'exact';
    const notFound = mode === 'host' ? 'result_not_found' : 'link_not_found';
    if ((await this.surface.boxOf(LINK_PREFIX + JSON.stringify({ target, mode }))) === null) {
      return report(notFound, null);
    }
    // A person looks at a page before clicking on it.
    await human.dwell(Math.min(human.drawDwell() / 4, left() / 4), { wander: false });
    if (Date.now() >= deadline) {
      throw this.notCome('the time of the navigation ran out before the link was clicked');
    }
    const arrived = await this.clickThrough(target, mode, until, human, deadline);
    return arrived === null ? report(notFound, null) : report('arrived', arrived.referer);
  }

  /**
   * Clicks the link to the target on the page that is open, as its reader
   * would, and waits for what the click brings: the page the link leads to,
   * whose times count from the click; nothing, when the page answers the
   * click itself; or no page, when the browser saves the answer as a file.
   *
   * @returns the `Referer` the browser sent for the page that came, or
   *   `null` when there was nothing to click or the click asked for no
   *   document.
   * @throws {ArrivalTimeout} when the page the link leads to was asked for and did not come in time.
   * @throws {NoPage} when the click was answered with no page.
   */
  private async clickThrough(
    target: string,
    mode: LinkMode,
    until: WaitUntil,
    human: Human,
    deadline: number,
  ): Promise<{ referer: string | null } | null> {
    const selector = LINK_PREFIX + JSON.stringify({ target, mode });
    const previous = this.document.response;
    const asked = this.documentsAsked;
    const before = this.page.url();
    const shown = await this.read<{ born: number }>(READ_SHOWN).catch(() => null);
    this.freshOnNavigation = 'once';
    let onLink = false;
    let missed = 'not_on_link';
    try {
      // The pointer goes to the link once the page rests from the wheel,
      // and the page is asked what it rests on. A page that moved by itself
      // meanwhile (an image that came, a banner) has moved the link a
      // little: the hand goes again, as a hand does. What it rests on after
      // that lies over the link, and a click there would be a click on
      // that, so the hand does not click.
      for (let attempt = 0; attempt < HOVER_ATTEMPTS && !onLink; attempt += 1) {
        await human.hover(selector);
        onLink = await this.pointerOnLink(target, mode);
      }
    } catch (thrown) {
      if (!(thrown instanceof BehaviorError)) {
        throw thrown;
      }
      // The link went away, or cannot be brought into view.
      missed = thrown.reason;
    }
    if (!onLink) {
      // Rare, and the result says only `link_not_found`: the log says why
      // (follow-up F66), so that a crawl that typed what it meant to click
      // can be told from one whose page had no such link.
      this.parts.log.warn(
        { attempts: HOVER_ATTEMPTS, missed },
        'the hand did not come to rest on the link, so it did not click',
      );
      this.freshOnNavigation = 'never';
      return null;
    }
    await human.pressButton();
    const clicked = Date.now();
    // A click is watched for a moment whatever the budget says: whether it
    // asked for a document decides between a link that leads nowhere and a
    // page that is late.
    const limit = Math.max(deadline, clicked + SOFT_NAVIGATION_MS);
    const idle = () =>
      Date.now() > clicked + SOFT_NAVIGATION_MS &&
      this.documentsAsked === asked &&
      this.page.url() !== before;
    const arrived = await this.afterClick(previous, until, limit, idle).finally(() => {
      if (this.document.response !== null && this.document.response !== previous) {
        // Times of the result count from the click: that is when the
        // target was asked for. Also for a page that came and did not
        // finish loading in time.
        countFrom(this.document, clicked);
      }
    });
    if (!arrived) {
      if (this.documentsAsked !== asked) {
        throw this.notCome('the page the link leads to did not answer in time');
      }
      // The click asked for no document: the link leads nowhere in this
      // tab, or the page drew something itself.
      this.freshOnNavigation = 'never';
      return null;
    }
    const referer = this.document.response?.request().headers()['referer'] ?? null;
    if (shown !== null && !(await this.replaced(shown.born))) {
      // An answer came and the page is the document it was: the browser
      // saves the answer as a file, or it held no document. Measured in
      // Chrome 155 and Camoufox 152: the driver reports the response of a
      // download like one of a page, and the page that was open stays.
      // Reported as arrived, the result would be that page under the
      // target's address.
      this.document = newDocument();
      throw new NoPage();
    }
    return { referer };
  }

  /**
   * How many entries the persona goes back at most to find a page in the
   * tab ({@link entriesToGoBack}), from what the page says of its tab and
   * what this session has seen the tab show.
   */
  private async entriesBack(wanted: string, reach: number): Promise<number> {
    if (reach <= 0) {
      return 0;
    }
    const tab = await this.read<TabHistory>(READ_TAB).catch(() => null);
    return tab === null ? 0 : entriesToGoBack(tab, wanted, reach, this.shown.has(wanted));
  }

  /** Whether the pointer rests on a link to the target: on the link, and not on something that lies over it. */
  private async pointerOnLink(target: string, mode: LinkMode): Promise<boolean> {
    const device = this.input;
    if (device === null) {
      return false;
    }
    try {
      const [pointer, metrics] = await Promise.all([
        device.pointerPosition(),
        this.surface.metrics(),
      ]);
      return await this.read<boolean>(
        linkAtExpression(target, mode, {
          x: pointer.x - metrics.origin.x,
          y: pointer.y - metrics.origin.y,
        }),
      );
    } catch {
      return false;
    }
  }

  /** Ends an entry whose page did not come in time: nothing of the pages it passed is the result. */
  private notCome(message: string): ArrivalTimeout {
    this.document = newDocument();
    this.freshOnNavigation = 'never';
    return new ArrivalTimeout(message);
  }

  /**
   * Arrives at the target by a click on a link of a page the persona has
   * been reading (spec §10.4, follow-up F28). Nothing is loaded to start
   * from. When the page that is open is that page, its link is clicked
   * there; when the tab holds it a few entries back, the persona goes back
   * to it with the keyboard, as a flow's `back` does, looks at what came
   * back, and clicks there. A page the browser kept costs the site no
   * request. When the tab does not hold the page, or the page no link a
   * reader could click, the outcome says so and the caller's fallback
   * decides; nothing moves when the browser can say beforehand that the
   * page is not there.
   *
   * @throws {ArrivalTimeout} when a page that was asked for, going back or by the click, did not come in time.
   */
  private async enterFromOpenPage(
    entry: Entry,
    target: string,
    until: WaitUntil,
    human: Human,
    deadline: number,
  ): Promise<EntryReport> {
    const from = entry.from_url ?? null;
    if (from === null) {
      throw new EngineError('invalid_request', `the entry ${entry.strategy} needs from_url`);
    }
    const report = (
      outcome: EntryReport['outcome'],
      referer: string | null = null,
      backs: number | null = null,
    ): EntryReport => ({ strategy: entry.strategy, outcome, from_url: from, referer, backs });
    let backs = 0;
    // Why the link was not clicked, for the log: the result says only
    // `link_not_found` (F66). The hand's own misses log themselves.
    const notFound = (missed: 'not_in_tab' | 'no_such_link' | null) => {
      if (missed !== null) {
        this.parts.log.info({ missed, backs }, 'the link of the open page was not clicked');
      }
      this.freshOnNavigation = 'never';
      return report('link_not_found');
    };
    const wanted = plainAddress(from);
    const left = () => Math.max(1, deadline - Date.now());
    // The page says where it is: a script may have given it another address than it was loaded from.
    const address = () => this.read<string>('location.href').catch(() => this.page.url());

    const open = await address();
    // The tab shows a document from before this request. Marked as one, the
    // document the click asks for begins the result, and nothing the page
    // that is left still loads is counted with it.
    this.document = newDocument(/^https?:\/\//i.test(open) ? withoutFragment(open) : null);
    if (wanted === null || plainAddress(open) !== wanted) {
      const reach = wanted === null ? 0 : await this.entriesBack(wanted, entry.max_backs ?? 0);
      let found = false;
      while (!found && backs < reach) {
        const went = await this.goBack(left(), () => human.chord(['Alt'], 'ArrowLeft'));
        if (went === 'nowhere') {
          break;
        }
        if (went !== 'arrived') {
          throw this.notCome(
            `the page ${backs + 1} back in the tab did not come within the time of the navigation`,
          );
        }
        backs += 1;
        // A person looks at the page that came back before doing anything on it.
        await human.dwell(Math.min(human.drawThinking(), left()), { wander: false });
        found = plainAddress(await address()) === wanted;
      }
      if (!found) {
        return notFound('not_in_tab');
      }
    }
    const observation = await this.observe().catch(() => emptyObservation);
    if ((this.document.response?.status() ?? 0) >= 400 || findInterstitial(observation) !== null) {
      // The page came back as an error or shows a wall. Nothing is clicked
      // on it, and nothing is typed after it.
      return report('blocked');
    }
    if (
      (await this.surface.boxOf(LINK_PREFIX + JSON.stringify({ target, mode: 'exact' }))) === null
    ) {
      return notFound('no_such_link');
    }
    if (Date.now() >= deadline) {
      throw this.notCome('the time of the navigation ran out before the link was clicked');
    }
    const arrived = await this.clickThrough(target, 'exact', until, human, deadline);
    return arrived === null ? notFound(null) : report('arrived', arrived.referer, backs);
  }

  /**
   * Whether the page shows another document than the one that began at the
   * given moment. Asked for a second: a response comes a moment before its
   * document. A page that cannot be read is taken to be another one.
   */
  private async replaced(born: number): Promise<boolean> {
    let same = false;
    for (let look = 0; look < DOCUMENT_LOOKS; look += 1) {
      const now = await this.read<{ born: number }>(READ_SHOWN).catch(() => null);
      if (now !== null && now.born !== born) {
        return true;
      }
      same = now !== null;
      await sleep(40);
    }
    return !same;
  }

  // --- capture ----------------------------------------------------------

  /** Reads the page as it is into a page result. Captures a page can notice come last. */
  private async capture(
    url: string,
    options: CaptureOptions,
    maxBody: number,
    env: OperationEnv,
    names: CaptureNames,
    extra: {
      readonly error: NavigationError | null;
      readonly observation: PageObservation;
      readonly wait: ChallengeWait;
      readonly consoleStart: number;
      readonly behavior: BehaviorReport | null;
      readonly started: number;
    },
  ): Promise<PageResult> {
    const { driver } = this.parts;
    const page = this.page;
    const state = this.document;
    const response = state.response;
    // A page the browser brought back from before the flow is a document
    // too, though nothing is known of the response it once came with.
    const loaded = response !== null || state.restored !== null;
    let domHtml: string | null = null;
    let domTruncated = false;
    if (options.dom && loaded) {
      const cut = truncateText(await page.content().catch(() => ''), maxBody);
      domHtml = cut.text;
      domTruncated = cut.truncated;
    }
    // The links are read as the document is: in the isolated world, all in
    // one look. A page that could not be read has none to report.
    const links =
      options.links === true && loaded
        ? await this.read<{ links: PageLink[]; truncated: boolean }>(READ_LINKS).catch(() => null)
        : null;
    const rawBody =
      options.raw && response !== null
        ? await response.body().then(
            (body) => captureBody(body, maxBody),
            () => null,
          )
        : null;
    const headers =
      response === null ? [] : toHeaders(await response.headersArray().catch(() => []));
    const redirects = await this.redirects(response);
    const xhr = new XhrCollector(options.xhr);
    for (const record of state.xhr) {
      if (xhr.wants(record.contentType)) {
        xhr.add(record);
      }
    }

    const sideEffects: SideEffect[] = [];
    let screenshot: ArtifactRef | null = null;
    let pdf: ArtifactRef | null = null;
    if (loaded && options.screenshot !== null) {
      const shot = await driver.screenshot(options.screenshot);
      sideEffects.push(...shot.sideEffects);
      const extension = shot.format === 'jpeg' ? 'jpg' : shot.format;
      screenshot = await env.artifacts.write(
        `${names.prefix}/${names.stamp}-screenshot.${extension}`,
        shot.data,
        `image/${shot.format}`,
      );
    }
    if (loaded && options.pdf !== null) {
      const printed = await driver.pdf(options.pdf);
      sideEffects.push(...printed.sideEffects);
      pdf = await env.artifacts.write(
        `${names.prefix}/${names.stamp}-page.pdf`,
        printed.data,
        'application/pdf',
      );
    }
    return {
      url,
      final_url: loaded ? page.url() : url,
      status: response?.status() ?? null,
      http_version: response === null ? null : await driver.protocolOf(response),
      headers,
      redirects,
      title: loaded ? extra.observation.title : null,
      dom_html: domHtml,
      dom_truncated: domTruncated,
      raw_body: rawBody,
      xhr: xhr.responses(),
      xhr_dropped: xhr.dropped,
      console: options.console ? driver.console.since(extra.consoleStart) : [],
      console_capture: options.console ? 'browser_log' : 'not_requested',
      timing: {
        ttfb_ms: state.ttfb,
        dom_content_loaded_ms: state.domContentLoaded,
        load_ms: state.load,
        total_ms: Date.now() - extra.started,
      },
      challenge: challengeSignals(extra.observation, extra.wait),
      screenshot,
      pdf,
      side_effects: sideEffects,
      error: extra.error,
      behavior: extra.behavior,
      links: links?.links ?? null,
      links_truncated: links?.truncated ?? false,
    };
  }

  /** Loads a page, lets the persona behave on it when asked, and captures it. */
  async navigate(request: NavigateRequest, env: OperationEnv): Promise<PageResult> {
    const { driver } = this.parts;
    const page = this.page;
    const started = Date.now();
    const timeout = request.timeout_ms ?? DEFAULT_NAVIGATE_TIMEOUT_MS;
    const deadline = started + timeout;
    const remaining = () => Math.max(1, deadline - Date.now());
    const consoleStart = driver.console.mark();
    const behavior = request.behavior ?? null;
    // A still visit (contract 1.5.0) is the persona's browser loading the
    // page and nothing else: the input device is never opened, so nothing
    // can be sent through it, and the visit is reported all the same.
    const still = behavior?.mode === 'still' ? stillVisit(behavior) : null;
    const acting = behavior === null || still !== null ? null : await this.human(behavior.seed);
    const entry = behavior?.entry ?? null;
    const filters = [request.capture.xhr];

    let error: NavigationError | null = null;
    let entryReport: EntryReport | null = null;
    let observation = emptyObservation;
    let wait: ChallengeWait = { marker: null, resolved: null, waitedMs: 0 };
    let dwelled = 0;
    const patience = behavior?.patience_ms ?? null;
    let scroll: ScrollReport | null = null;
    this.begin(filters);
    try {
      let typed = true;
      if (entry !== null) {
        // A still visit never clicks its way in: `stillVisit` refused that.
        if (acting !== null && (entry.strategy === 'link' || entry.strategy === 'search_engine')) {
          const arrive =
            entry.strategy === 'link' && entry.from_open_page === true
              ? this.enterFromOpenPage(
                  entry,
                  request.url,
                  request.wait.until,
                  acting.human,
                  deadline,
                )
              : this.enter(entry, request.url, request.wait.until, acting.human, deadline);
          entryReport = await arrive;
          typed = false;
          if (entryReport.outcome !== 'arrived') {
            if ((entry.fallback ?? 'stop') === 'none' && entryReport.outcome !== 'blocked') {
              entryReport = { ...entryReport, outcome: 'fell_back' };
              typed = true;
            } else {
              // Nothing of the page to start from is the result of this request.
              this.begin(filters);
              error = {
                code: 'aborted',
                message: `the entry did not reach the target: ${entryReport.outcome}`,
                net_error: null,
              };
            }
          }
        } else {
          entryReport = {
            strategy: entry.strategy,
            outcome: 'arrived',
            from_url: null,
            referer: null,
            backs: null,
          };
        }
      }
      if (typed) {
        this.begin(filters);
        const arrived = await page.goto(request.url, {
          waitUntil: WAIT_UNTIL[request.wait.until],
          timeout: remaining(),
        });
        if (arrived?.status() === 407) {
          error = {
            code: 'proxy_failed',
            message: 'the proxy refused the credentials of the persona (407)',
            net_error: null,
          };
        }
      }
      if (error === null) {
        if (request.wait.selector != null) {
          await page.waitForSelector(request.wait.selector, {
            state: 'attached',
            timeout: remaining(),
          });
        }
        if ((request.wait.extra_ms ?? 0) > 0) {
          await sleep(Math.min(request.wait.extra_ms ?? 0, remaining()));
        }
      }
    } catch (thrown) {
      if (thrown instanceof EngineError) {
        throw thrown;
      }
      error = driver.classifyError(thrown);
      if (
        this.document.response === null &&
        this.document.proxyChallenged &&
        error.code === 'navigation_failed'
      ) {
        // The proxy asked for credentials and no document followed: it
        // refused the persona's, whatever name the browser gives the failure.
        error = { ...error, code: 'proxy_failed' };
      }
      await driver.settleAfterFailure(error);
    }

    if (error === null || this.document.response !== null) {
      const challenge = await waitForChallenge(
        () => this.observe(),
        error === null
          ? Math.min(request.challenge_wait_ms ?? DEFAULT_CHALLENGE_WAIT_MS, remaining())
          : 0,
        sleep,
      );
      observation = challenge.observation;
      wait = challenge.wait;
      if (error === null) {
        const budget = () => Math.max(0, remaining() - CAPTURE_RESERVE_MS);
        if (acting !== null && behavior !== null) {
          const wanted =
            behavior.dwell === 'human' ? acting.human.drawDwell() : (request.dwell_ms ?? 0);
          if (behavior.scroll_depth !== undefined && behavior.scroll_depth > 0) {
            // With a patience the page is here, and what `timeout_ms` had
            // to bound is over: the scrolling ends at its depth or when the
            // patience is used up (follow-up F44). Without one it leaves
            // the dwell its time: at most what the budget has beyond it,
            // which ends every page too long for that at the same moment.
            const ended = await acting.human.scrollTo(
              behavior.scroll_depth,
              performance.now() + (patience ?? Math.max(0, budget() - wanted)),
            );
            scroll = {
              endedBy: ended === 'deadline' ? (patience === null ? 'budget' : 'patience') : ended,
              reached: await this.scrolled(ended === 'deadline'),
            };
          }
          // The dwell follows in full where the scrolling had a time of its own.
          dwelled = patience === null ? Math.min(wanted, budget()) : wanted;
          await acting.human.dwell(dwelled, { wander: behavior.wander ?? true });
        } else if ((request.dwell_ms ?? 0) > 0) {
          // A bare load and a still visit rest alike: a plain wait, no hand.
          dwelled = Math.min(request.dwell_ms ?? 0, remaining());
          await sleep(dwelled);
        }
      }
      await Promise.race([Promise.allSettled(this.document.bodies), sleep(BODY_WAIT_MS)]);
    }

    return this.capture(
      request.url,
      request.capture,
      request.max_body_bytes ?? DEFAULT_MAX_BODY_BYTES,
      env,
      {
        prefix: request.artifact_prefix ?? env.defaultArtifactPrefix,
        stamp: started.toString(36),
      },
      {
        error,
        observation,
        wait,
        consoleStart,
        behavior:
          acting !== null
            ? this.report(acting.seed, acting.human.counts, dwelled, entryReport, patience, scroll)
            : still !== null
              ? this.report(still.seed, noInput(), dwelled, entryReport)
              : null,
        started,
      },
    );
  }

  /** Runs a flow, sending one event per finished step and a final one (spec §10.5). */
  async flow(
    request: FlowRequest,
    env: OperationEnv,
    emit: (event: FlowEvent) => void,
  ): Promise<void> {
    const { driver } = this.parts;
    const page = this.page;
    const started = Date.now();
    const consoleStart = driver.console.mark();
    const prefix = env.defaultArtifactPrefix;
    const acting: { current: { human: Human; seed: string } | null } = { current: null };
    let captures = 0;
    // Every filter the flow can ask with is known before it starts, so the
    // bodies they want are read as the pages load.
    const filters = [
      request.capture.xhr,
      ...request.actions.map((action) => (action.action === 'extract' ? action.capture.xhr : null)),
    ];
    this.filters = filters.filter((filter): filter is XhrCapture => filter !== null);
    this.freshOnNavigation = 'always';
    this.remembered = new Map();
    const snapshot = async (options: CaptureOptions, error: NavigationError | null) => {
      if (
        loadedFrom(this.document) !== null &&
        withoutFragment(page.url()) !== loadedFrom(this.document)
      ) {
        // The address is not the one this document was loaded from: a
        // script gave the page another, or the browser brought back a page
        // it had kept without a step of the flow asking for it.
        await this.adoptShown();
      }
      await Promise.race([Promise.allSettled(this.document.bodies), sleep(BODY_WAIT_MS)]);
      captures += 1;
      return this.capture(
        page.url(),
        options,
        DEFAULT_MAX_BODY_BYTES,
        env,
        { prefix, stamp: `${started.toString(36)}-${captures}` },
        {
          error,
          observation: await this.observe().catch(() => emptyObservation),
          wait: { marker: null, resolved: null, waitedMs: 0 },
          consoleStart,
          behavior: null,
          started,
        },
      );
    };
    const nothing: CaptureOptions = {
      dom: false,
      raw: false,
      xhr: null,
      console: false,
      screenshot: null,
      pdf: null,
    };
    const host: FlowHost = {
      human: async () => {
        acting.current ??= await this.human(request.behavior?.seed);
        return acting.current.human;
      },
      typos: request.behavior?.typos ?? false,
      goto: async (url, timeout) => {
        this.leave(this.document);
        this.document = newDocument();
        try {
          await page.goto(url, { waitUntil: 'load', timeout });
        } catch (thrown) {
          const error = driver.classifyError(thrown);
          await driver.settleAfterFailure(error);
          throw new Error(`${error.code}: ${error.message}`, { cause: thrown });
        }
      },
      back: async (timeout, by) => {
        // The same watch whoever goes back, and for an entry that goes
        // back to click as well (follow-ups F38, F28).
        const went = await this.goBack(timeout, by);
        if (went === 'unanswered') {
          throw new FlowStepError(`the page before did not answer within ${timeout} ms`);
        }
        if (went === 'nowhere') {
          throw new FlowStepError(
            `there is no page to go back to: the history of the tab ends at ${page.url()}`,
          );
        }
        if (went === 'unfinished') {
          throw new FlowStepError(`the page before did not finish loading within ${timeout} ms`);
        }
      },
      waitForSelector: async (selector, timeout) => {
        await page.waitForSelector(selector, { state: 'attached', timeout });
      },
      waitForUrl: async (glob, timeout) => {
        // The address is compared here, and the page asked whether it has
        // loaded: the driver's own wait for an address waits for a `load`
        // event as well, which a page out of the back/forward cache never fires.
        const matches = urlMatcher(glob);
        const deadline = Date.now() + timeout;
        while (!matches(page.url())) {
          if (Date.now() >= deadline) {
            throw new Error(`the address ${page.url()} does not match ${glob}`);
          }
          await sleep(40);
        }
        if (!(await this.loaded(deadline))) {
          throw new FlowStepError(
            `the URL matched ${glob}, and its page did not finish loading within ${timeout} ms`,
          );
        }
      },
      waitForNetworkIdle: (timeout) => page.waitForLoadState('networkidle', { timeout }),
      mark: () => this.mark(),
      waitForNavigation: async (since, timeout) => {
        // A click or a key may leave the page: by a request for a new
        // document, or, going back, by a page the browser kept. Either
        // shows as a new response or as another address.
        const limit = Date.now() + timeout;
        const how = await this.departed(since, limit);
        if (how === null) {
          return false;
        }
        if (how === 'answered') {
          await page
            .waitForLoadState('load', {
              timeout: Math.max(1, limit + ARRIVAL_LOAD_MS - Date.now()),
            })
            .catch(() => undefined);
        } else {
          // No response came with it, so no `load` will either (measured on
          // Chrome 155: the driver's wait for one runs into its timeout
          // after a page came back out of the back/forward cache). The page
          // says itself when it is ready.
          await this.adoptShown();
          await this.loaded(limit + ARRIVAL_LOAD_MS);
        }
        return true;
      },
      exists: (selector) =>
        this.read<boolean>(`document.querySelector(${JSON.stringify(selector)}) !== null`),
      text: () => this.read<string>(READ_TEXT),
      selectState: (selector, value) =>
        this.read<{ index: number; wanted: number } | null>(`(() => {
          const select = document.querySelector(${JSON.stringify(selector)});
          if (!select || !select.options) return null;
          const wanted = Array.from(select.options).findIndex((option) => option.value === ${JSON.stringify(value)});
          return { index: select.selectedIndex, wanted };
        })()`),
      driver: {
        click: (selector, timeout) => page.click(selector, { timeout }),
        hover: (selector, timeout) => page.hover(selector, { timeout }),
        fill: (selector, text, timeout) => page.fill(selector, text, { timeout }),
        press: (key) => page.keyboard.press(key),
        select: async (selector, value, timeout) => {
          await page.selectOption(selector, value, { timeout });
        },
        scroll: async (depth) => {
          const metrics = await this.surface.metrics();
          const target = Math.max(0, metrics.scrollHeight - metrics.viewport.height) * depth;
          await page.mouse.wheel(0, target - metrics.scrollY);
        },
      },
      extract: (options) => snapshot(options, null),
      screenshot: (options) => snapshot({ ...nothing, screenshot: options }, null),
      pdf: (options) => snapshot({ ...nothing, pdf: options }, null),
    };
    try {
      const outcome = await runFlow(request, host, emit).finally(() => {
        this.freshOnNavigation = 'never';
      });
      const result = await snapshot(request.capture, null);
      const done = acting.current;
      emit({
        type: 'final',
        ok: outcome.ok,
        result,
        behavior:
          done === null ? null : this.report(done.seed, done.human.counts, outcome.dwellMs, null),
      });
    } finally {
      this.remembered = null;
    }
  }

  // --- state ------------------------------------------------------------

  async exportState(request: ExportStateRequest): Promise<ExportStateResponse> {
    const cookies = cookiesForOrigin(
      (await this.parts.driver.cookies()).map(toContractCookie),
      request.origin,
      new Date(),
    );
    let local: StorageEntry[] | null = null;
    let session: StorageEntry[] | null = null;
    // Storage is read only from a page that is on the origin already:
    // loading something to get at it would be a visit nobody asked for.
    if (
      request.include_storage === true &&
      httpOrigin(this.page.url()) === new URL(request.origin).origin
    ) {
      const storage = await this.read<{ local: StorageEntry[]; session: StorageEntry[] }>(
        READ_STORAGE,
      ).catch(() => null);
      local = storage?.local ?? null;
      session = storage?.session ?? null;
    }
    return { origin: request.origin, cookies, local_storage: local, session_storage: session };
  }

  async importState(request: ImportStateRequest): Promise<ImportStateResponse> {
    const now = new Date();
    const rejected: RejectedCookie[] = [];
    let imported = 0;
    for (const cookie of request.cookies) {
      const reason = importRejection(cookie, request.origin, now);
      if (reason !== null) {
        rejected.push({ name: cookie.name, domain: cookie.domain, reason });
        continue;
      }
      // The browser applies its own cookie rules (prefixes, SameSite=None
      // needs Secure, size limits) and says whether it took the cookie.
      if (await this.parts.driver.setCookie(cookie).catch(() => false)) {
        imported += 1;
      } else {
        rejected.push({ name: cookie.name, domain: cookie.domain, reason: 'refused_by_browser' });
      }
    }
    return { imported, rejected };
  }

  // --- self-test --------------------------------------------------------

  private async loadForSelftest(
    target: SelftestPage,
    options: { settleMs: number; timeoutMs: number; publishedSelector: string | null },
  ): Promise<LoadedPage> {
    const { driver } = this.parts;
    const page = this.page;
    const started = Date.now();
    // A failure of the navigation itself is typed, so that the run knows
    // to load the page once more (F95); a failure in reading a page that
    // arrived stays what it is.
    const response = await page
      .goto(target.url, { waitUntil: 'load', timeout: options.timeoutMs })
      .catch(async (thrown: unknown) => {
        const error = driver.classifyError(thrown);
        await driver.settleAfterFailure(error);
        throw new NavigationFailed(error, thrown);
      });
    let published: string | null = null;
    if (options.publishedSelector !== null) {
      const left = Math.max(1, started + options.timeoutMs - Date.now());
      published = await page
        .waitForSelector(options.publishedSelector, { state: 'attached', timeout: left })
        .then(() =>
          this.read<string | null>(
            `(document.querySelector(${JSON.stringify(options.publishedSelector)}) || {}).textContent ?? null`,
          ),
        )
        .catch(() => null);
    }
    if (options.settleMs > 0) {
      await sleep(options.settleMs);
    }
    const [text, rows, shot] = await Promise.all([
      this.read<string>(READ_TEXT).catch(() => ''),
      this.read<TableRow[]>(READ_ROWS).catch(() => []),
      driver.screenshot({ full_page: false, format: 'png' }).then(
        (result) => result.data,
        () => null,
      ),
    ]);
    return { httpStatus: response?.status() ?? null, text, rows, published, screenshot: shot };
  }

  selftest(request: SelftestRequest, env: OperationEnv): Promise<SelftestResponse> {
    return runSelftest({ load: (page, options) => this.loadForSelftest(page, options) }, request, {
      artifacts: env.artifacts,
      artifactPrefix: request.artifact_prefix ?? `${env.defaultArtifactPrefix}/selftest`,
      defaultPages: this.parts.selftestPages,
      now: () => new Date(),
      sleep,
    });
  }

  /** Releases the display's input device. */
  async close(): Promise<void> {
    await this.input?.close().catch(() => undefined);
    this.input = null;
  }
}
