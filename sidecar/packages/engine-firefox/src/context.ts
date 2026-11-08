import { readFile, rename, rm, stat, writeFile } from 'node:fs/promises';

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
  GatedInput,
  PageSession,
  X11Input,
  toBrowserCookieParam,
  type BrowserCookie,
  type DriverPage,
  type DriverResponse,
  type DriverTab,
  type EngineContext,
  type EngineLog,
  type InputDevice,
  type OperationEnv,
  type Point,
  type KeptHeads,
  type SessionDriver,
  type ShownHead,
} from '@kage/engine-core';
import type { BrowserContext, Page, Response } from 'playwright-core';

import { keptBody as readKeptBody } from './cache-entry.js';

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

/**
 * The header lines of a response as Firefox holds them (F190). Its driver
 * splits every value at its commas and trims each piece, so a date
 * (`Tue, 03 Nov 2026 ...`) and a list come as several headers of one name;
 * Firefox itself holds one value per name, with repeated lines joined by
 * `, `, and the lines of the names whose values hold commas of their own
 * (`Set-Cookie`, `WWW-Authenticate`, `Proxy-Authenticate`) joined by a line
 * feed; the driver splits `Set-Cookie`'s there and keeps them as lines.
 * Consecutive pieces of one name are made one value again, joined as
 * Firefox joins them: as sent wherever a comma was followed by one space,
 * as in a date and most lists; `a,b` comes back as `a, b`, which the driver
 * no longer tells apart. A value Firefox joined with a line feed is the
 * field lines it joined, one each: no field value holds a line feed
 * (RFC 9110 §5.5), and Chrome and the http path give them so (QA round
 * three, browser-01).
 */
export function joinedHeaders(
  split: readonly { name: string; value: string }[],
): { name: string; value: string }[] {
  const lines: { name: string; value: string }[] = [];
  for (const { name, value } of split) {
    const previous = lines[lines.length - 1];
    const same = previous?.name.toLowerCase() === name.toLowerCase();
    if (previous !== undefined && same && name.toLowerCase() !== 'set-cookie') {
      previous.value = `${previous.value}, ${value}`;
    } else {
      lines.push({ name, value });
    }
  }
  return lines.flatMap(({ name, value }) =>
    value.split('\n').map((line) => ({ name, value: line.trim() })),
  );
}

/**
 * The keyboard shortcut a person prints a page with, `Control` and `p`, on
 * `device`: the gated device, so that it acts only for a step or request
 * whose input grant is still active (QA round four input-01, the review of
 * #96). `Control` is let up whatever happened to the `p`.
 */
export async function pressPrint(
  device: InputDevice,
  wait: (ms: number) => Promise<void> = sleep,
): Promise<void> {
  await device.keyDown('Control');
  try {
    await wait(60);
    await device.keyDown('p');
    await wait(70);
    await device.keyUp('p');
    await wait(40);
  } finally {
    await device.keyUp('Control');
  }
  await device.flush();
}

/** What the engine hands over when a context is open. */
export interface FirefoxContextParts {
  readonly context: BrowserContext;
  readonly page: Page;
  /**
   * The heads of the pages the persona's browser keeps (F178): read from
   * the profile when the context opened, written back when it closes.
   */
  readonly keptHeads: KeptHeads;
  /** The file in the persona's profile that holds them. */
  readonly keptHeadsPath: string;
  /**
   * The entries of the browser's own HTTP cache in the persona's profile
   * (`cache2/entries`), where the bodies of the pages it keeps are: read,
   * never written (browser-05).
   */
  readonly cacheEntries: string;
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
  readonly console = new ConsoleLog();
  private readonly pageSession: PageSession;
  /** The open tab; replaced by {@link FirefoxContext.freshPage}. */
  private current: Page;
  private input: Promise<X11Input> | null = null;
  private closed = false;
  /** Whether {@link FirefoxContext.freshPage} is opening a tab: its report is no page's (F184). */
  private opening = false;

  constructor(private readonly parts: FirefoxContextParts) {
    this.browserVersion = parts.browserVersion;
    this.applied = parts.applied;
    this.current = parts.page;
    this.listen(parts.page);
    this.pageSession = new PageSession({
      driver: this,
      personaSeed: parts.personaSeed,
      selftestPages: parts.selftestPages,
      log: parts.log,
    });
  }

  /** The open tab as the session drives it: the driver's page, whose reads grant no activation in Firefox. */
  get page(): DriverPage {
    return this.current;
  }

  /**
   * Listens to a tab's console. Firefox reports console calls and uncaught
   * errors to its driver from the browser side; a page cannot tell that
   * somebody listens.
   */
  private listen(page: Page): void {
    // Every document's answer, for the page the browser will show when it
    // revalidates it (F178). Read from the driver, never from the page.
    page.on('response', (response) => {
      let document = false;
      try {
        const asked = response.request();
        document = asked.isNavigationRequest() && asked.frame() === page.mainFrame();
      } catch {
        // A request without a frame (a service worker's) is no document of the tab.
      }
      if (document) {
        response.headersArray().then(
          (headers) => {
            this.parts.keptHeads.note(
              response.url(),
              response.status(),
              joinedHeaders(headers),
              Date.now(),
            );
          },
          () => undefined,
        );
      }
    });
    page.on('console', (message) => {
      this.console.add({
        level: CONSOLE_LEVELS[message.type()] ?? 'info',
        source: 'console-api',
        text: message.text(),
        url: message.location().url || null,
      });
    });
    page.on('pageerror', (error) => {
      this.console.add({ level: 'error', source: 'javascript', text: error.message, url: null });
    });
  }

  /**
   * Replaces the open tab by a fresh one of the same window (F96). The new
   * tab is there before the old one goes: a window whose last tab closes
   * closes itself.
   */
  async freshPage(): Promise<void> {
    this.opening = true;
    const next = await this.parts.context.newPage().finally(() => {
      this.opening = false;
    });
    this.listen(next);
    const previous = this.current;
    this.current = next;
    await previous.close().catch(() => undefined);
  }

  /** The context's tabs besides the open one: tabs a page opened (F184). */
  otherTabs(): readonly DriverTab[] {
    return this.parts.context.pages().filter((page) => page !== this.current);
  }

  /** Selects the open tab in its window and raises the window, as a click on the tab does (F184). */
  async front(): Promise<void> {
    await this.current.bringToFront();
  }

  onOpenedTab(listener: () => void): void {
    this.parts.context.on('page', (page) => {
      if (!this.opening && page !== this.current) {
        listener();
      }
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
    const data = await this.current.screenshot({
      type: format,
      fullPage: options.full_page,
      ...(format === 'jpeg' ? { quality: options.quality ?? 80 } : {}),
    });
    return { data, sideEffects: [], format };
  }

  private device(): Promise<X11Input> {
    // Camoufox on the display hears a dead key and then the plain letter
    // (`´`, `e` arrives as `e`): it does not compose them here, so a letter
    // the layout lacks is lent a key instead (QA round three, browser-07).
    this.input ??= X11Input.open(this.parts.display, { deadKeys: false });
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
    // Through the gate, as every key of the persona's: a `pdf` step whose
    // time ran out presses nothing once its grant is revoked (review of #96).
    await pressPrint(new GatedInput(await this.device()));
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

  /**
   * A document's status and header lines as the tab shows them: the lines
   * as Firefox received them, not split at commas (F190); for a `304` the
   * page the browser kept, brought up to date by the 304's fields, where
   * its head is kept, and the wire's `304` where it is not (F178).
   */
  async shownHead(response: DriverResponse): Promise<ShownHead | null> {
    const status = response.status();
    const headers = joinedHeaders(await response.headersArray());
    const kept =
      status === 304 ? this.parts.keptHeads.shown(response.url(), headers, Date.now()) : null;
    return kept ?? { status, headers };
  }

  /**
   * The body of the page Firefox kept, where it revalidated it and shows it
   * (QA round three, browser-05): the driver refuses a body for the wire's
   * `304`, so the engine reads the page's entry of the browser's own cache
   * in the persona's profile, as Firefox serves it to the tab, never
   * through the page. Only where the engine keeps the page's head, whose
   * `Content-Encoding` it undoes, at most `limit` bytes; `null` where the entry is not the whole
   * page.
   */
  async keptBody(response: DriverResponse, limit: number): Promise<Buffer | null> {
    if (response.status() !== 304) {
      return null;
    }
    const kept = this.parts.keptHeads.shown(response.url(), [], Date.now());
    if (kept === null) {
      return null;
    }
    return readKeptBody(this.parts.cacheEntries, response.url(), kept.headers, limit);
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

  /** Writes the kept heads into the profile, whole or not at all (F178). */
  private async keepHeads(): Promise<void> {
    const { keptHeadsPath } = this.parts;
    const next = `${keptHeadsPath}.next`;
    try {
      await writeFile(next, this.parts.keptHeads.serialize(Date.now()), { mode: 0o600 });
      await rename(next, keptHeadsPath);
    } catch (error) {
      this.parts.log.warn({ err: error }, 'the heads of the kept pages could not be written');
    }
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
      await this.keepHeads();
      await this.parts.release();
    }
  }
}
