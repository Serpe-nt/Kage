import { readFile, rm, stat } from 'node:fs/promises';

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
  ScreenshotOptions,
  SelftestPage,
  SelftestRequest,
  SelftestResponse,
  SideEffect,
} from '@kage/contract';
import {
  ConsoleLog,
  EngineError,
  PageSession,
  X11Input,
  toBrowserCookieParam,
  type BrowserCookie,
  type DriverPage,
  type DriverResponse,
  type EngineContext,
  type EngineLog,
  type InputDevice,
  type OperationEnv,
  type Point,
  type SessionDriver,
} from '@kage/engine-core';
import type { BrowserContext, Page, Response } from 'playwright-core';

const PROXY_ERRORS = /NS_ERROR_(PROXY_[A-Z_]+|UNKNOWN_PROXY_HOST)/;

const CONSOLE_LEVELS: Readonly<Record<string, ConsoleLevel>> = {
  error: 'error',
  assert: 'error',
  warning: 'warning',
  info: 'info',
  log: 'info',
  debug: 'verbose',
  trace: 'verbose',
};

/** How the browser names the protocol of a response, in the short form Chrome uses. */
const PROTOCOLS: Readonly<Record<string, string>> = {
  'HTTP/1.0': 'http/1.0',
  'HTTP/1.1': 'http/1.1',
  'HTTP/2.0': 'h2',
  'HTTP/3.0': 'h3',
};

/** How long the browser is given to write a printed page. */
const PRINT_TIMEOUT_MS = 20_000;

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/**
 * Turns what the driver threw for a navigation into the contract's error.
 * Firefox names its network errors `NS_ERROR_…`; where it shows its own
 * error page instead, the driver only says that the navigation was
 * interrupted, and no name is known.
 */
export function classifyNavigationError(error: unknown): NavigationError {
  const message = (error instanceof Error ? error.message : String(error)).split('\n')[0] ?? '';
  const net = /\b(NS_(?:ERROR|BINDING)_[A-Z_]+)\b/.exec(message)?.[1] ?? null;
  if (error instanceof Error && error.name === 'TimeoutError') {
    return { code: 'timeout', message, net_error: net };
  }
  if (net !== null && PROXY_ERRORS.test(net)) {
    return { code: 'proxy_failed', message, net_error: net };
  }
  if (
    net === 'NS_BINDING_ABORTED' ||
    /download is starting|page (was )?closed|target closed|has been closed/i.test(message)
  ) {
    return { code: 'aborted', message, net_error: net };
  }
  return { code: 'navigation_failed', message, net_error: net };
}

/** What the engine hands over when a context is open. */
export interface FirefoxContextParts {
  readonly context: BrowserContext;
  readonly page: Page;
  readonly browserVersion: string;
  readonly applied: AppliedIdentity;
  /** The persona's seed: its motor profile derives from it. */
  readonly personaSeed: string;
  /** Name of the context's own X display. */
  readonly display: string;
  /** Where the browser writes a printed page. */
  readonly pdfPath: string;
  readonly selftestPages: readonly SelftestPage[];
  readonly log: EngineLog;
  /** Stops the display and releases what the context held. */
  readonly release: () => Promise<void>;
}

/**
 * One open persona context on Camoufox.
 *
 * What a navigation, a flow or a self-test does is the shared session of
 * engine-core; this class supplies what only Firefox does its own way: its
 * error names, its screenshots, printing to a file, its cookie store and
 * its console.
 */
export class FirefoxContext implements EngineContext, SessionDriver {
  readonly browserVersion: string;
  readonly applied: AppliedIdentity;
  readonly page: DriverPage;
  readonly console = new ConsoleLog();
  private readonly pageSession: PageSession;
  private input: Promise<X11Input> | null = null;
  private closed = false;

  constructor(private readonly parts: FirefoxContextParts) {
    this.browserVersion = parts.browserVersion;
    this.applied = parts.applied;
    this.page = parts.page;
    // Firefox reports console calls and uncaught errors to its driver from
    // the browser side; a page cannot tell that somebody listens.
    parts.page.on('console', (message) => {
      this.console.add({
        level: CONSOLE_LEVELS[message.type()] ?? 'info',
        source: 'console-api',
        text: message.text(),
        url: message.location().url || null,
      });
    });
    parts.page.on('pageerror', (error) => {
      this.console.add({ level: 'error', source: 'javascript', text: error.message, url: null });
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
   * After a failed navigation Firefox loads its own error page, and a
   * navigation started meanwhile is interrupted by it. A short rest lets
   * that finish.
   */
  async settleAfterFailure(error: NavigationError): Promise<void> {
    if (error.code !== 'aborted' && error.code !== 'timeout') {
      await sleep(400);
    }
  }

  async screenshot(
    options: ScreenshotOptions,
  ): Promise<{ data: Buffer; sideEffects: SideEffect[]; format: 'png' | 'jpeg' }> {
    // The driver draws the page from the browser side: the page neither
    // resizes nor scrolls, for the viewport and for the whole document
    // alike. Firefox encodes PNG and JPEG; a WebP request gets a PNG.
    const format = options.format === 'jpeg' ? 'jpeg' : 'png';
    const data = await this.parts.page.screenshot({
      type: format,
      fullPage: options.full_page,
      ...(format === 'jpeg' ? { quality: options.quality ?? 80 } : {}),
    });
    return { data, sideEffects: [], format };
  }

  private device(): Promise<X11Input> {
    this.input ??= X11Input.open(this.parts.display);
    return this.input;
  }

  /**
   * Prints the page to a file with the keyboard shortcut a person uses.
   * The profile is set to print without a dialog into one file; the page
   * sees `beforeprint` and `afterprint`, as for any print.
   *
   * Paper size and orientation are the profile's (A4, portrait, with
   * backgrounds): Firefox takes them from preferences, not per print.
   */
  async pdf(): Promise<{ data: Buffer; sideEffects: SideEffect[] }> {
    const { pdfPath } = this.parts;
    await rm(pdfPath, { force: true });
    const device = await this.device();
    await device.keyDown('Control');
    await sleep(60);
    await device.keyDown('p');
    await sleep(70);
    await device.keyUp('p');
    await sleep(40);
    await device.keyUp('Control');
    await device.flush();
    let size = -1;
    for (let waited = 0; waited < PRINT_TIMEOUT_MS; waited += 200) {
      await sleep(200);
      const current = await stat(pdfPath).then(
        (found) => found.size,
        () => -1,
      );
      // The file is complete when it stopped growing.
      if (current > 0 && current === size) {
        const data = await readFile(pdfPath);
        await rm(pdfPath, { force: true });
        return { data, sideEffects: ['beforeprint', 'afterprint'] };
      }
      size = current;
    }
    throw new EngineError('internal', `Firefox wrote no PDF within ${PRINT_TIMEOUT_MS} ms`);
  }

  async protocolOf(response: DriverResponse): Promise<string | null> {
    // The responses the session hands back are the driver's own.
    const version = await (response as Partial<Response>).httpVersion?.().catch(() => null);
    return version == null ? null : (PROTOCOLS[version] ?? version.toLowerCase());
  }

  async cookies(): Promise<BrowserCookie[]> {
    return this.parts.context.cookies();
  }

  async setCookie(cookie: Cookie): Promise<boolean> {
    // The driver takes a cookie by its domain: without a leading dot it is
    // kept host-only, as a response without a Domain attribute sets it.
    const param = toBrowserCookieParam(cookie);
    delete param.url;
    await this.parts.context.addCookies([
      { ...param, domain: cookie.host_only ? cookie.domain : `.${cookie.domain}` },
    ]);
    return true;
  }

  windowOrigin(): Point {
    // The window sits at the display's origin; the position a page reads
    // is the fingerprint's.
    return { x: 0, y: 0 };
  }

  openInput(): Promise<InputDevice> {
    return this.device();
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
      await (await this.input?.catch(() => null))?.close();
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
