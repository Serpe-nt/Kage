import type {
  AppliedIdentity,
  ConsoleLevel,
  Cookie,
  ExportStateRequest,
  ExportStateResponse,
  FlowEvent,
  FlowRequest,
  ImportStateRequest,
  ImportStateResponse,
  NavigateRequest,
  NavigationError,
  PageResult,
  PdfOptions,
  ScreenshotFormat,
  ScreenshotOptions,
  SelftestPage,
  SelftestRequest,
  SelftestResponse,
  SideEffect,
} from '@kage/contract';
import {
  ConsoleLog,
  PageSession,
  X11Input,
  toBrowserCookieParam,
  type BrowserCookie,
  type DriverPage,
  type DriverResponse,
  type EngineContext,
  type EngineLog,
  type OperationEnv,
  type Point,
  type SessionDriver,
} from '@kage/engine-core';
import type { BrowserContext, CDPSession, Page } from 'patchright';

/** Paper sizes in inches, as the print command takes them. */
const PAPER = { a4: { width: 8.27, height: 11.69 }, letter: { width: 8.5, height: 11 } } as const;

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
    /download is starting|page (was )?closed|target closed|has been closed/i.test(message)
  ) {
    return { code: 'aborted', message, net_error: net };
  }
  return { code: 'navigation_failed', message, net_error: net };
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
  /** The persona's seed: its motor profile derives from it. */
  readonly personaSeed: string;
  /** Name of the context's own X display. */
  readonly display: string;
  /** Where the browser window sits on that display. */
  readonly windowOrigin: Point;
  readonly selftestPages: readonly SelftestPage[];
  readonly log: EngineLog;
  /** Stops the display and releases what the context held. */
  readonly release: () => Promise<void>;
}

/**
 * One open persona context on real Chrome.
 *
 * What a navigation, a flow or a self-test does is the shared session of
 * engine-core; this class supplies what only Chrome can: its error names,
 * its debugger commands for screenshots, PDFs and cookies, and its own log.
 */
export class ChromiumContext implements EngineContext, SessionDriver {
  readonly browserVersion: string;
  readonly applied: AppliedIdentity;
  readonly page: DriverPage;
  readonly console = new ConsoleLog();
  private readonly protocols = new Map<string, string>();
  private readonly pageSession: PageSession;
  private closed = false;

  constructor(private readonly parts: ChromiumContextParts) {
    this.browserVersion = parts.browserVersion;
    this.applied = parts.applied;
    // The driver's page offers everything the session asks for and more.
    this.page = parts.page;
    // Entries the browser itself logs (failed loads, security and
    // deprecation messages). This is not the console API: reading that
    // needs a debugger domain pages can detect.
    parts.session.on('Log.entryAdded', (event) => {
      this.console.add({
        level: LOG_LEVELS[event.entry.level] ?? 'info',
        source: event.entry.source,
        text: event.entry.text,
        url: event.entry.url ?? null,
      });
    });
    parts.session.on('Network.responseReceived', (event) => {
      if (event.type === 'Document' && event.response.protocol !== undefined) {
        if (this.protocols.size > 200) {
          this.protocols.clear();
        }
        this.protocols.set(event.response.url, event.response.protocol);
      }
    });
    this.pageSession = new PageSession({
      driver: this,
      personaSeed: parts.personaSeed,
      selftestPages: parts.selftestPages,
      log: parts.log,
    });
  }

  classifyError(thrown: unknown): NavigationError {
    return classifyNavigationError(thrown);
  }

  /**
   * After a navigation failed on the network, Chrome replaces the page by
   * its error page a moment later. A navigation started before that has
   * happened is interrupted by it, so the next caller would fail for the
   * previous caller's error. This waits until the error page is there.
   */
  async settleAfterFailure(error: NavigationError): Promise<void> {
    if (error.net_error === null || error.code === 'aborted') {
      return;
    }
    for (
      let waited = 0;
      waited < 2000 && !this.parts.page.url().startsWith(ERROR_PAGE);
      waited += 50
    ) {
      await sleep(50);
    }
  }

  async screenshot(
    options: ScreenshotOptions,
  ): Promise<{ data: Buffer; sideEffects: SideEffect[]; format: ScreenshotFormat }> {
    const image =
      options.format === 'png'
        ? { format: options.format }
        : { format: options.format, quality: options.quality ?? 80 };
    if (!options.full_page) {
      // The viewport as it is: nothing in the page changes or fires.
      const shot = await this.parts.session.send('Page.captureScreenshot', image);
      return { data: Buffer.from(shot.data, 'base64'), sideEffects: [], format: options.format };
    }
    const metrics = await this.parts.session.send('Page.getLayoutMetrics');
    const size = metrics.cssContentSize;
    const shot = await this.parts.session.send('Page.captureScreenshot', {
      ...image,
      captureBeyondViewport: true,
      clip: { x: 0, y: 0, width: Math.ceil(size.width), height: Math.ceil(size.height), scale: 1 },
    });
    // Capturing beyond the viewport fires resize in the page, once or twice.
    return {
      data: Buffer.from(shot.data, 'base64'),
      sideEffects: ['resize'],
      format: options.format,
    };
  }

  async pdf(options: PdfOptions): Promise<{ data: Buffer; sideEffects: SideEffect[] }> {
    const paper = PAPER[options.format ?? 'a4'];
    const printed = await this.parts.session.send('Page.printToPDF', {
      landscape: options.landscape ?? false,
      printBackground: options.print_background ?? true,
      paperWidth: paper.width,
      paperHeight: paper.height,
      transferMode: 'ReturnAsBase64',
    });
    return {
      data: Buffer.from(printed.data, 'base64'),
      sideEffects: ['beforeprint', 'afterprint'],
    };
  }

  protocolOf(response: DriverResponse): Promise<string | null> {
    return Promise.resolve(this.protocols.get(response.url()) ?? null);
  }

  async cookies(): Promise<BrowserCookie[]> {
    const { cookies } = await this.parts.browserSession.send('Storage.getCookies');
    return cookies;
  }

  async setCookie(cookie: Cookie): Promise<boolean> {
    const result = await this.parts.session.send('Network.setCookie', toBrowserCookieParam(cookie));
    return result.success;
  }

  windowOrigin(): Point {
    return this.parts.windowOrigin;
  }

  openInput(): Promise<X11Input> {
    return X11Input.open(this.parts.display);
  }

  navigate(request: NavigateRequest, env: OperationEnv): Promise<PageResult> {
    return this.pageSession.navigate(request, env);
  }

  flow(request: FlowRequest, env: OperationEnv, emit: (event: FlowEvent) => void): Promise<void> {
    return this.pageSession.flow(request, env, emit);
  }

  exportState(request: ExportStateRequest): Promise<ExportStateResponse> {
    return this.pageSession.exportState(request);
  }

  importState(request: ImportStateRequest): Promise<ImportStateResponse> {
    return this.pageSession.importState(request);
  }

  selftest(request: SelftestRequest, env: OperationEnv): Promise<SelftestResponse> {
    return this.pageSession.selftest(request, env);
  }

  async close(): Promise<void> {
    if (this.closed) {
      return;
    }
    this.closed = true;
    try {
      await this.pageSession.close();
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
