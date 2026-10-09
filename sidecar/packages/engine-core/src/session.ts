import { randomBytes } from 'node:crypto';

import {
  DEFAULT_CHALLENGE_WAIT_MS,
  DEFAULT_MAX_BODY_BYTES,
  DEFAULT_NAVIGATE_TIMEOUT_MS,
  type ArtifactRef,
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
  type PageResult,
  type PdfOptions,
  type RedirectHop,
  type RejectedCookie,
  type ScreenshotFormat,
  type ScreenshotOptions,
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
  Human,
  motorProfile,
  type BehaviorCounts,
  type Box,
  type InputDevice,
  type MotorProfile,
  type PageSurface,
  type Point,
  type SurfaceMetrics,
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
import { runFlow, type FlowHost } from './flow/executor.js';
import { SeededRandom } from './seeded-random.js';
import type { TableRow } from './selftest/pages.js';
import { runSelftest, type LoadedPage } from './selftest/run.js';

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
  goBack(options: { waitUntil: LoadState; timeout: number }): Promise<DriverResponse | null>;
  waitForSelector(
    selector: string,
    options: { state: 'attached'; timeout: number },
  ): Promise<unknown>;
  waitForLoadState(
    state: 'domcontentloaded' | 'load' | 'networkidle',
    options: { timeout: number },
  ): Promise<void>;
  waitForURL(url: string, options: { timeout: number }): Promise<void>;
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
/** Budget kept back for the captures when behavior fills the time. */
const CAPTURE_RESERVE_MS = 4000;
/** How long a click is given to start a navigation. */
const CLICK_NAVIGATION_MS = 15_000;

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

const boxExpression = (find: string) => `(() => {
  const element = ${find};
  if (!element) return null;
  const rect = element.getBoundingClientRect();
  if (rect.width === 0 || rect.height === 0) return null;
  return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
})()`;

/** A selector the session understands besides CSS: the link an entry clicks. */
const LINK_PREFIX = 'kage-link:';

/** Finds the anchor an entry is after. `mode` says how it is recognized. */
function linkExpression(target: string, mode: 'exact' | 'host'): string {
  return `(() => {
    const target = new URL(${JSON.stringify(target)});
    const plain = (url) => url.origin + url.pathname.replace(/\\/$/, '') + url.search;
    const visible = (anchor) => { const r = anchor.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
    const anchors = Array.from(document.querySelectorAll('a[href]')).filter(visible);
    const parsed = anchors.map((anchor) => { try { return [anchor, new URL(anchor.href)]; } catch { return null; } }).filter(Boolean);
    const exact = parsed.find(([, url]) => plain(url) === plain(target));
    if (exact || ${JSON.stringify(mode)} === 'exact') return exact ? exact[0] : null;
    const sameHost = (url) => url.hostname === target.hostname && url.hostname !== location.hostname;
    const result = parsed.find(([, url]) => sameHost(url));
    return result ? result[0] : null;
  })()`;
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

const newDocument = (): DocumentState => ({
  response: null,
  started: Date.now(),
  ttfb: null,
  domContentLoaded: null,
  load: null,
  scriptUrls: new Set(),
  xhr: [],
  bodies: [],
  proxyChallenged: false,
});

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
  /**
   * Whether the next document replaces what was collected. A plain
   * navigation keeps collecting across the documents it passes through (a
   * challenge page and the page behind it are one visit); a click that
   * leaves a page starts anew.
   */
  private freshOnNavigation: 'never' | 'once' | 'always' = 'never';

  constructor(private readonly parts: SessionParts) {
    const { page } = parts.driver;
    page.on('request', (asked) => {
      if (asked.resourceType() === 'script') {
        this.document.scriptUrls.add(asked.url());
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
      if (
        this.freshOnNavigation !== 'never' &&
        asked.redirectedFrom() === null &&
        this.document.response !== null
      ) {
        // A new document after a click: what was collected belongs to the page that was left.
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
          mode: 'exact' | 'host';
        };
        return this.read<Box | null>(boxExpression(linkExpression(target, mode)));
      }
      return this.read<Box | null>(
        boxExpression(`document.querySelector(${JSON.stringify(selector)})`),
      );
    },
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
    return {
      seed: requestSeed,
      human: new Human({
        device: this.input,
        surface: this.surface,
        profile: this.profile,
        random: new SeededRandom(this.parts.personaSeed, `behavior:${requestSeed}`),
      }),
    };
  }

  private report(
    seed: string,
    counts: BehaviorCounts,
    dwellMs: number,
    entry: EntryReport | null,
  ): BehaviorReport {
    return {
      seed,
      profile_id: (this.profile ?? motorProfile(this.parts.personaSeed)).id,
      entry,
      dwell_ms: Math.round(dwellMs),
      ...counts,
    };
  }

  /** Waits until a click has started a new document, then for the state the request names. */
  private async afterClick(previous: DriverResponse | null, until: WaitUntil, deadline: number) {
    const limit = Math.min(deadline, Date.now() + CLICK_NAVIGATION_MS);
    while (this.document.response === previous || this.document.response === null) {
      if (Date.now() > limit) {
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
   * and clicks it. The `Referer` is whatever the browser sends for that
   * click; nothing is ever forged.
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
    const selector =
      LINK_PREFIX +
      JSON.stringify({ target, mode: entry.strategy === 'search_engine' ? 'host' : 'exact' });
    if ((await this.surface.boxOf(selector)) === null) {
      return report(
        entry.strategy === 'search_engine' ? 'result_not_found' : 'link_not_found',
        null,
      );
    }
    // A person looks at a page before clicking on it.
    await human.dwell(Math.min(human.drawDwell() / 4, left() / 4), { wander: false });
    const previous = this.document.response;
    this.freshOnNavigation = 'once';
    const clicked = Date.now();
    await human.click(selector);
    if (!(await this.afterClick(previous, until, deadline))) {
      return report(
        entry.strategy === 'search_engine' ? 'result_not_found' : 'link_not_found',
        null,
      );
    }
    // Times of the result count from the click: that is when the target was asked for.
    this.document.started = clicked;
    const referer = this.document.response?.request().headers()['referer'] ?? null;
    return report('arrived', referer);
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
    const loaded = response !== null;
    let domHtml: string | null = null;
    let domTruncated = false;
    if (options.dom && loaded) {
      const cut = truncateText(await page.content().catch(() => ''), maxBody);
      domHtml = cut.text;
      domTruncated = cut.truncated;
    }
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
    const acting = behavior === null ? null : await this.human(behavior.seed);
    const entry = behavior?.entry ?? null;
    const filters = [request.capture.xhr];

    let error: NavigationError | null = null;
    let entryReport: EntryReport | null = null;
    let observation = emptyObservation;
    let wait: ChallengeWait = { marker: null, resolved: null, waitedMs: 0 };
    let dwelled = 0;
    this.begin(filters);
    try {
      let typed = true;
      if (entry !== null && acting !== null) {
        if (entry.strategy === 'link' || entry.strategy === 'search_engine') {
          entryReport = await this.enter(
            entry,
            request.url,
            request.wait.until,
            acting.human,
            deadline,
          );
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
            // Scrolling leaves the dwell its time: at most what the budget has beyond it.
            await acting.human.scrollTo(
              behavior.scroll_depth,
              performance.now() + Math.max(0, budget() - wanted),
            );
          }
          dwelled = Math.min(wanted, budget());
          await acting.human.dwell(dwelled, { wander: behavior.wander ?? true });
        } else if ((request.dwell_ms ?? 0) > 0) {
          await sleep(Math.min(request.dwell_ms ?? 0, remaining()));
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
          acting === null
            ? null
            : this.report(acting.seed, acting.human.counts, dwelled, entryReport),
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
    const snapshot = async (options: CaptureOptions, error: NavigationError | null) => {
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
        this.document = newDocument();
        try {
          await page.goto(url, { waitUntil: 'load', timeout });
        } catch (thrown) {
          const error = driver.classifyError(thrown);
          await driver.settleAfterFailure(error);
          throw new Error(`${error.code}: ${error.message}`, { cause: thrown });
        }
      },
      back: async (timeout) => {
        await page.goBack({ waitUntil: 'load', timeout });
      },
      waitForSelector: async (selector, timeout) => {
        await page.waitForSelector(selector, { state: 'attached', timeout });
      },
      waitForUrl: (glob, timeout) => page.waitForURL(glob, { timeout }),
      waitForNetworkIdle: (timeout) => page.waitForLoadState('networkidle', { timeout }),
      waitForNavigation: async (timeout) => {
        // A click or a key may leave the page: by a request for a new
        // document, or, going back, by a page the browser kept. Either
        // shows as a new response or as another address.
        const previous = this.document.response;
        const address = page.url();
        const limit = Date.now() + timeout;
        while (this.document.response === previous && page.url() === address) {
          if (Date.now() > limit) {
            return;
          }
          await sleep(40);
        }
        await page
          .waitForLoadState('load', { timeout: Math.max(1, limit + 4000 - Date.now()) })
          .catch(() => undefined);
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
    const ok = await runFlow(request, host, emit).finally(() => {
      this.freshOnNavigation = 'never';
    });
    const result = await snapshot(request.capture, null);
    const done = acting.current;
    emit({
      type: 'final',
      ok,
      result,
      behavior: done === null ? null : this.report(done.seed, done.human.counts, 0, null),
    });
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
    const response = await page
      .goto(target.url, { waitUntil: 'load', timeout: options.timeoutMs })
      .catch(async (thrown: unknown) => {
        await driver.settleAfterFailure(driver.classifyError(thrown));
        throw thrown;
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
    });
  }

  /** Releases the display's input device. */
  async close(): Promise<void> {
    await this.input?.close().catch(() => undefined);
    this.input = null;
  }
}
