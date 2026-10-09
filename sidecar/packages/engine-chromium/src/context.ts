import {
  DEFAULT_CHALLENGE_WAIT_MS,
  DEFAULT_MAX_BODY_BYTES,
  DEFAULT_NAVIGATE_TIMEOUT_MS,
  type AppliedIdentity,
  type ArtifactRef,
  type ConsoleEntry,
  type ConsoleLevel,
  type ExportStateRequest,
  type ExportStateResponse,
  type ImportStateRequest,
  type ImportStateResponse,
  type NavigateRequest,
  type NavigationError,
  type PageResult,
  type PdfOptions,
  type RedirectHop,
  type RejectedCookie,
  type ScreenshotOptions,
  type SelftestPage,
  type SelftestRequest,
  type SelftestResponse,
  type SideEffect,
  type StorageEntry,
} from '@kage/contract';
import {
  XhrCollector,
  captureBody,
  challengeSignals,
  cookiesForOrigin,
  httpOrigin,
  importRejection,
  runSelftest,
  toBrowserCookieParam,
  toContractCookie,
  toHeaders,
  truncateText,
  waitForChallenge,
  type BrowserCookie,
  type EngineContext,
  type EngineLog,
  type LoadedPage,
  type OperationEnv,
  type PageObservation,
  type TableRow,
} from '@kage/engine-core';
import type { BrowserContext, CDPSession, Frame, Page, Request, Response } from 'patchright';

/** Longest page text read for interstitial detection and evidence. */
const TEXT_LIMIT = 200_000;
/** How long bodies of XHR responses are waited for once the page settled. */
const BODY_WAIT_MS = 5000;
/** Paper sizes in inches, as the print command takes them. */
const PAPER = { a4: { width: 8.27, height: 11.69 }, letter: { width: 8.5, height: 11 } } as const;

const WAIT_UNTIL = {
  commit: 'commit',
  domcontentloaded: 'domcontentloaded',
  load: 'load',
  network_idle: 'networkidle',
} as const;

const PROXY_ERRORS =
  /ERR_(PROXY_[A-Z_]+|TUNNEL_CONNECTION_FAILED|NO_SUPPORTED_PROXIES|MANDATORY_PROXY_CONFIGURATION_FAILED)/;

const LOG_LEVELS: Readonly<Record<string, ConsoleLevel>> = {
  error: 'error',
  warning: 'warning',
  info: 'info',
  verbose: 'verbose',
};

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/** The page Chrome shows in place of a document it could not load. */
const ERROR_PAGE = 'chrome-error://';

/** Reads the document in the driver's isolated world: the page cannot see it happen. */
const READ_TEXT = `(document.body ? document.body.innerText : '').slice(0, ${TEXT_LIMIT})`;
const READ_ROWS = `Array.from(document.querySelectorAll('tr')).slice(0, 2000).map((row) => {
  const cells = Array.from(row.querySelectorAll('td, th'));
  return { cells: cells.map((cell) => cell.innerText), classes: cells.map((cell) => cell.className) };
})`;
const READ_STORAGE = `({
  local: Object.keys(localStorage).map((name) => ({ name, value: localStorage.getItem(name) ?? '' })),
  session: Object.keys(sessionStorage).map((name) => ({ name, value: sessionStorage.getItem(name) ?? '' })),
})`;

/**
 * Turns what the driver threw for a navigation into the contract's error.
 * The browser's own error name (`net::ERR_...`) decides: a proxy failure is
 * not the target's failure, and Kage treats the two differently.
 */
export function classifyNavigationError(error: unknown): NavigationError {
  const message = (error instanceof Error ? error.message : String(error)).split('\n')[0] ?? '';
  const net = /net::(ERR_[A-Z_]+)/.exec(message)?.[1] ?? null;
  if (error instanceof Error && error.name === 'TimeoutError') {
    return { code: 'timeout', message, net_error: net };
  }
  if (net !== null && PROXY_ERRORS.test(net)) {
    return { code: 'proxy_failed', message, net_error: net };
  }
  if (
    net === 'ERR_ABORTED' ||
    /download is starting|page (was )?closed|target closed/i.test(message)
  ) {
    return { code: 'aborted', message, net_error: net };
  }
  return { code: 'navigation_failed', message, net_error: net };
}

/** Whether a request is the navigation of a frame. A request without a frame (a service worker's) is not. */
function isDocumentOf(request: Request, frame: Frame): boolean {
  try {
    return request.isNavigationRequest() && request.frame() === frame;
  } catch {
    return false;
  }
}

/** What the engine hands over when a context is open. */
export interface ChromiumContextParts {
  readonly context: BrowserContext;
  readonly page: Page;
  /** A debugger session on the page: log entries, screenshots, PDFs, cookies. */
  readonly session: CDPSession;
  /** A debugger session on the browser: the whole cookie jar. */
  readonly browserSession: CDPSession;
  readonly browserVersion: string;
  readonly applied: AppliedIdentity;
  readonly selftestPages: readonly SelftestPage[];
  readonly log: EngineLog;
  /** Stops the display and releases what the context held. */
  readonly release: () => Promise<void>;
}

interface LogEntry {
  readonly entry: ConsoleEntry;
}

/** One open persona context on real Chrome. */
export class ChromiumContext implements EngineContext {
  readonly browserVersion: string;
  readonly applied: AppliedIdentity;
  private readonly logEntries: LogEntry[] = [];
  private readonly protocols = new Map<string, string>();
  private closed = false;

  constructor(private readonly parts: ChromiumContextParts) {
    this.browserVersion = parts.browserVersion;
    this.applied = parts.applied;
    // Entries the browser itself logs (failed loads, security and
    // deprecation messages). This is not the console API: reading that
    // needs a debugger domain pages can detect.
    parts.session.on('Log.entryAdded', (event) => {
      this.logEntries.push({
        entry: {
          level: LOG_LEVELS[event.entry.level] ?? 'info',
          source: event.entry.source,
          text: event.entry.text,
          url: event.entry.url ?? null,
        },
      });
      if (this.logEntries.length > 2000) {
        this.logEntries.splice(0, 1000);
      }
    });
    parts.session.on('Network.responseReceived', (event) => {
      if (event.type === 'Document' && event.response.protocol !== undefined) {
        if (this.protocols.size > 200) {
          this.protocols.clear();
        }
        this.protocols.set(event.response.url, event.response.protocol);
      }
    });
  }

  private get page(): Page {
    return this.parts.page;
  }

  /**
   * After a navigation failed on the network, Chrome replaces the page by
   * its error page a moment later. A navigation started before that has
   * happened is interrupted by it, so the next caller would fail for the
   * previous caller's error. This waits until the error page is there.
   */
  private async settleAfterFailure(error: NavigationError): Promise<void> {
    if (error.net_error === null || error.code === 'aborted') {
      return;
    }
    for (let waited = 0; waited < 2000 && !this.page.url().startsWith(ERROR_PAGE); waited += 50) {
      await sleep(50);
    }
  }

  private read<T>(expression: string): Promise<T> {
    return this.page.evaluate<T>(expression);
  }

  private async observe(scriptUrls: ReadonlySet<string>): Promise<PageObservation> {
    const main = this.page.mainFrame();
    const [title, text] = await Promise.all([
      this.page.title().catch(() => null),
      this.read<string>(READ_TEXT).catch(() => null),
    ]);
    return {
      title,
      text,
      scriptUrls: [...scriptUrls],
      frameUrls: this.page
        .frames()
        .filter((frame) => frame !== main)
        .map((frame) => frame.url())
        .filter((url) => url !== '' && url !== 'about:blank' && !url.startsWith('about:srcdoc')),
    };
  }

  private async screenshot(
    options: ScreenshotOptions,
  ): Promise<{ data: Buffer; sideEffects: SideEffect[] }> {
    const image =
      options.format === 'png'
        ? { format: options.format }
        : { format: options.format, quality: options.quality ?? 80 };
    if (!options.full_page) {
      // The viewport as it is: nothing in the page changes or fires.
      const shot = await this.parts.session.send('Page.captureScreenshot', image);
      return { data: Buffer.from(shot.data, 'base64'), sideEffects: [] };
    }
    const metrics = await this.parts.session.send('Page.getLayoutMetrics');
    const size = metrics.cssContentSize;
    const shot = await this.parts.session.send('Page.captureScreenshot', {
      ...image,
      captureBeyondViewport: true,
      clip: { x: 0, y: 0, width: Math.ceil(size.width), height: Math.ceil(size.height), scale: 1 },
    });
    // Capturing beyond the viewport fires resize in the page, once or twice.
    return { data: Buffer.from(shot.data, 'base64'), sideEffects: ['resize'] };
  }

  private async pdf(options: PdfOptions): Promise<Buffer> {
    const paper = PAPER[options.format ?? 'a4'];
    const printed = await this.parts.session.send('Page.printToPDF', {
      landscape: options.landscape ?? false,
      printBackground: options.print_background ?? true,
      paperWidth: paper.width,
      paperHeight: paper.height,
      transferMode: 'ReturnAsBase64',
    });
    return Buffer.from(printed.data, 'base64');
  }

  private redirects(response: Response | null): RedirectHop[] {
    const hops: RedirectHop[] = [];
    let previous: Request | null = response?.request().redirectedFrom() ?? null;
    while (previous !== null && hops.length < 50) {
      const request = previous;
      hops.unshift({ url: request.url(), status: 0, location: null });
      previous = request.redirectedFrom();
    }
    return hops;
  }

  private async completeRedirects(response: Response | null): Promise<RedirectHop[]> {
    const hops = this.redirects(response);
    let request: Request | null = response?.request().redirectedFrom() ?? null;
    for (let index = hops.length - 1; index >= 0 && request !== null; index -= 1) {
      const answered = await request.response().catch(() => null);
      const hop = hops[index];
      if (hop !== undefined && answered !== null) {
        hops[index] = {
          url: hop.url,
          status: answered.status(),
          location: answered.headers()['location'] ?? null,
        };
      }
      request = request.redirectedFrom();
    }
    return hops;
  }

  async navigate(request: NavigateRequest, env: OperationEnv): Promise<PageResult> {
    const started = Date.now();
    const timeout = request.timeout_ms ?? DEFAULT_NAVIGATE_TIMEOUT_MS;
    const remaining = () => Math.max(1, started + timeout - Date.now());
    const maxBody = request.max_body_bytes ?? DEFAULT_MAX_BODY_BYTES;
    const prefix = request.artifact_prefix ?? env.defaultArtifactPrefix;
    const stamp = started.toString(36);
    const page = this.page;
    const mainFrame = page.mainFrame();

    const xhr = new XhrCollector(request.capture.xhr);
    const bodies: Promise<void>[] = [];
    const scriptUrls = new Set<string>();
    const logStart = this.logEntries.length;
    // Filled in by the listeners below while the page loads.
    const seen: {
      mainResponse: Response | null;
      ttfb: number | null;
      domContentLoaded: number | null;
      load: number | null;
    } = { mainResponse: null, ttfb: null, domContentLoaded: null, load: null };

    const onRequest = (asked: Request) => {
      if (asked.resourceType() === 'script') {
        scriptUrls.add(asked.url());
      }
    };
    const onResponse = (response: Response) => {
      const asked = response.request();
      if (isDocumentOf(asked, mainFrame)) {
        // The latest one counts: after a challenge resolved itself, the
        // document behind it is the result.
        seen.mainResponse = response;
        seen.ttfb ??= Date.now() - started;
        return;
      }
      const kind = asked.resourceType();
      if ((kind === 'xhr' || kind === 'fetch') && xhr.wants(response.headers()['content-type'])) {
        bodies.push(
          response
            .body()
            .then(
              (body) => body,
              () => null,
            )
            .then((body) => {
              xhr.add({
                url: response.url(),
                method: asked.method(),
                kind,
                status: response.status(),
                contentType: response.headers()['content-type'] ?? null,
                body,
              });
            }),
        );
      }
    };
    const onDomContentLoaded = () => {
      seen.domContentLoaded ??= Date.now() - started;
    };
    const onLoad = () => {
      seen.load ??= Date.now() - started;
    };
    page.on('request', onRequest);
    page.on('response', onResponse);
    page.on('domcontentloaded', onDomContentLoaded);
    page.on('load', onLoad);

    let error: NavigationError | null = null;
    const sideEffects: SideEffect[] = [];
    let screenshot: ArtifactRef | null = null;
    let pdf: ArtifactRef | null = null;
    let observation: PageObservation = { title: null, text: null, scriptUrls: [], frameUrls: [] };
    let wait = { marker: null as string | null, resolved: null as boolean | null, waitedMs: 0 };
    try {
      try {
        await page.goto(request.url, {
          waitUntil: WAIT_UNTIL[request.wait.until],
          timeout,
        });
        if (request.wait.selector != null) {
          await page.waitForSelector(request.wait.selector, {
            state: 'attached',
            timeout: remaining(),
          });
        }
        if ((request.wait.extra_ms ?? 0) > 0) {
          await sleep(Math.min(request.wait.extra_ms ?? 0, remaining()));
        }
      } catch (thrown) {
        error = classifyNavigationError(thrown);
        await this.settleAfterFailure(error);
      }

      if (error === null || seen.mainResponse !== null) {
        const challenge = await waitForChallenge(
          () => this.observe(scriptUrls),
          error === null
            ? Math.min(request.challenge_wait_ms ?? DEFAULT_CHALLENGE_WAIT_MS, remaining())
            : 0,
          sleep,
        );
        observation = challenge.observation;
        wait = challenge.wait;
        if ((request.dwell_ms ?? 0) > 0 && error === null) {
          await sleep(Math.min(request.dwell_ms ?? 0, remaining()));
        }
        await Promise.race([Promise.allSettled(bodies), sleep(BODY_WAIT_MS)]);
      }

      const response = seen.mainResponse;
      const loaded = response !== null;
      let domHtml: string | null = null;
      let domTruncated = false;
      if (request.capture.dom && loaded) {
        const cut = truncateText(await page.content().catch(() => ''), maxBody);
        domHtml = cut.text;
        domTruncated = cut.truncated;
      }
      const rawBody =
        request.capture.raw && response !== null
          ? await response.body().then(
              (body) => captureBody(body, maxBody),
              () => null,
            )
          : null;
      const headers =
        response === null ? [] : toHeaders(await response.headersArray().catch(() => []));
      const redirects = await this.completeRedirects(response);

      // Captures a page can notice come last, and only when asked for.
      if (loaded && request.capture.screenshot !== null) {
        const options = request.capture.screenshot;
        const shot = await this.screenshot(options);
        sideEffects.push(...shot.sideEffects);
        const extension = options.format === 'jpeg' ? 'jpg' : options.format;
        screenshot = await env.artifacts.write(
          `${prefix}/${stamp}-screenshot.${extension}`,
          shot.data,
          `image/${options.format}`,
        );
      }
      if (loaded && request.capture.pdf !== null) {
        const data = await this.pdf(request.capture.pdf);
        sideEffects.push('beforeprint', 'afterprint');
        pdf = await env.artifacts.write(`${prefix}/${stamp}-page.pdf`, data, 'application/pdf');
      }

      return {
        url: request.url,
        final_url: loaded ? page.url() : request.url,
        status: response?.status() ?? null,
        http_version: response === null ? null : (this.protocols.get(response.url()) ?? null),
        headers,
        redirects,
        title: loaded ? observation.title : null,
        dom_html: domHtml,
        dom_truncated: domTruncated,
        raw_body: rawBody,
        xhr: xhr.responses(),
        xhr_dropped: xhr.dropped,
        console: request.capture.console
          ? this.logEntries.slice(logStart).map((item) => item.entry)
          : [],
        console_capture: request.capture.console ? 'browser_log' : 'not_requested',
        timing: {
          ttfb_ms: seen.ttfb,
          dom_content_loaded_ms: seen.domContentLoaded,
          load_ms: seen.load,
          total_ms: Date.now() - started,
        },
        challenge: challengeSignals(observation, wait),
        screenshot,
        pdf,
        side_effects: sideEffects,
        error,
      };
    } finally {
      page.off('request', onRequest);
      page.off('response', onResponse);
      page.off('domcontentloaded', onDomContentLoaded);
      page.off('load', onLoad);
    }
  }

  private async allCookies(): Promise<BrowserCookie[]> {
    const { cookies } = await this.parts.browserSession.send('Storage.getCookies');
    return cookies;
  }

  async exportState(request: ExportStateRequest): Promise<ExportStateResponse> {
    const cookies = cookiesForOrigin(
      (await this.allCookies()).map(toContractCookie),
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
      const accepted = await this.parts.session
        .send('Network.setCookie', toBrowserCookieParam(cookie))
        .then(
          (result) => result.success,
          () => false,
        );
      if (accepted) {
        imported += 1;
      } else {
        rejected.push({ name: cookie.name, domain: cookie.domain, reason: 'refused_by_browser' });
      }
    }
    return { imported, rejected };
  }

  private async loadForSelftest(
    target: SelftestPage,
    options: { settleMs: number; timeoutMs: number; publishedSelector: string | null },
  ): Promise<LoadedPage> {
    const page = this.page;
    const started = Date.now();
    const response = await page
      .goto(target.url, { waitUntil: 'load', timeout: options.timeoutMs })
      .catch(async (thrown: unknown) => {
        await this.settleAfterFailure(classifyNavigationError(thrown));
        throw thrown;
      });
    let published: string | null = null;
    if (options.publishedSelector !== null) {
      const left = Math.max(1, started + options.timeoutMs - Date.now());
      published = await page
        .waitForSelector(options.publishedSelector, { state: 'attached', timeout: left })
        .then((element) => element.textContent())
        .catch(() => null);
    }
    if (options.settleMs > 0) {
      await sleep(options.settleMs);
    }
    const [text, rows, shot] = await Promise.all([
      this.read<string>(READ_TEXT).catch(() => ''),
      this.read<TableRow[]>(READ_ROWS).catch(() => []),
      this.parts.session.send('Page.captureScreenshot', { format: 'png' }).then(
        (result) => Buffer.from(result.data, 'base64'),
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

  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    try {
      // Closing the context ends the browser the way a person quits it:
      // cookies and storage are flushed to the profile.
      await this.parts.context.close();
    } catch (error) {
      this.parts.log.warn({ err: error }, 'closing the browser failed');
    } finally {
      await this.parts.release();
    }
  }
}
