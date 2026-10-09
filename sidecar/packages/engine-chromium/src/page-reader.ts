import { randomBytes } from 'node:crypto';

import type { DriverPage } from '@kage/engine-core';
import type { CDPSession, Page } from 'patchright';

/**
 * Reading a page without touching it (spec §10.2).
 *
 * The driver marks every evaluation as a user gesture: it sends
 * `Runtime.callFunctionOn` with `userGesture: true`. A page nobody touched
 * then reads `navigator.userActivation` as active and is allowed to start
 * an `AudioContext`, with no input event before either. So nothing the
 * engine reads from a page goes through the driver: the title, the text,
 * the document, selector waits and the measurements of the behavior engine
 * are evaluated here, over the engine's own debugger session, with
 * `Runtime.evaluate` in an isolated world and without the gesture flag.
 * The driver keeps the launch, navigation, and the input of flow steps that
 * ask for it (`human: false`).
 *
 * The world is this reader's own. A script registered for every new
 * document puts a mark into it, and every expression first checks for the
 * mark and for being in the top frame. An execution context id is only a
 * small number that another renderer process hands out again, so after a
 * navigation an old id may name another world of the new page, its main
 * world included; with the check such an evaluation reads nothing and is
 * repeated in the right world.
 *
 * None of this needs the `Runtime` debugger domain to be enabled, which a
 * page can detect.
 */

/** How long a wait for a selector rests between two looks at the page. */
export const DEFAULT_POLL_MS = 100;
/** How often a read is repeated when the document went away under it. */
const READ_ATTEMPTS = 8;
/** The rest between two such repetitions. */
const READ_RETRY_MS = 50;

/** What the debugger says when the world an evaluation was meant for is gone. */
const WORLD_GONE =
  /Cannot find context with specified id|Execution context was destroyed|Inspected target navigated or closed|No frame for given id found|Could not create isolated world|Cannot find default execution context/;

/** The document a read was meant for went away; the read can be repeated. */
class WorldGone extends Error {
  override readonly name = 'WorldGone';
}

/**
 * A wait ended before what it waited for was there. Named as the driver
 * names its own timeouts, so that both are classified alike.
 */
export class ReadTimeoutError extends Error {
  override readonly name = 'TimeoutError';
}

/** An expression threw in the page's document, for example for a selector that is none. */
export class PageReadError extends Error {
  override readonly name = 'PageReadError';
}

/** How a reader is set up; everything has a default. */
export interface PageReaderOptions {
  /** The rest between two looks at the page while waiting for a selector. */
  readonly pollMs?: number;
  /**
   * Name of the reader's world and of its mark. Drawn at random when not
   * given: no page can read either, and a name nobody can guess cannot be
   * prepared for.
   */
  readonly key?: string;
  /** The clock and the pause; injectable for tests. */
  readonly now?: () => number;
  readonly sleep?: (ms: number) => Promise<void>;
}

const pause = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

const firstLine = (text: string) => text.split('\n')[0] ?? '';

/** The document as the driver serializes it: the doctype and the root element. */
const READ_CONTENT = `(() => {
  let html = '';
  if (document.doctype) html = new XMLSerializer().serializeToString(document.doctype);
  if (document.documentElement) html += document.documentElement.outerHTML;
  return html;
})()`;

/**
 * Reads the main frame of one page over a debugger session of the engine's
 * own. Create it with {@link PageReader.attach} before the page navigates.
 */
export class PageReader {
  /** The world of the current document, once asked for. */
  private context: Promise<number> | null = null;
  private readonly pollMs: number;
  private readonly now: () => number;
  private readonly sleep: (ms: number) => Promise<void>;

  private constructor(
    private readonly session: CDPSession,
    private readonly key: string,
    options: PageReaderOptions,
  ) {
    this.pollMs = options.pollMs ?? DEFAULT_POLL_MS;
    this.now = options.now ?? Date.now;
    this.sleep = options.sleep ?? pause;
  }

  /**
   * Prepares a session for reading: from now on every new document of the
   * page gets the reader's world with its mark, and so does the document
   * that is open already.
   *
   * The `Page` domain is enabled for it: without it the browser accepts
   * the script and never runs it. Enabling it changes nothing a page can
   * see; the driver's own session has it enabled as well.
   */
  static async attach(session: CDPSession, options: PageReaderOptions = {}): Promise<PageReader> {
    const key = options.key ?? `k${randomBytes(12).toString('hex')}`;
    await session.send('Page.enable');
    await session.send('Page.addScriptToEvaluateOnNewDocument', {
      source: `globalThis[${JSON.stringify(key)}] = true;`,
      worldName: key,
      runImmediately: true,
    });
    return new PageReader(session, key, options);
  }

  /** The expression behind the check that it runs in this reader's world of the top frame. */
  private guarded(expression: string): string {
    const key = JSON.stringify(this.key);
    return `(() => { if (globalThis[${key}] !== true || self !== top) throw new Error(${key}); return (\n${expression}\n); })()`;
  }

  /** Asks for the reader's world of the main frame's document as it is now. */
  private async world(): Promise<number> {
    try {
      const { frameTree } = await this.session.send('Page.getFrameTree');
      // The same name gives the same world for as long as the document
      // lives: the one the registered script marked.
      const { executionContextId } = await this.session.send('Page.createIsolatedWorld', {
        frameId: frameTree.frame.id,
        worldName: this.key,
      });
      return executionContextId;
    } catch (error) {
      throw WORLD_GONE.test(String(error)) ? new WorldGone(firstLine(String(error))) : error;
    }
  }

  /** Evaluates once, in the world last asked for. */
  private async once<T>(expression: string): Promise<T> {
    const context = (this.context ??= this.world());
    try {
      const answer = await this.session
        .send('Runtime.evaluate', {
          expression: this.guarded(expression),
          contextId: await context,
          returnByValue: true,
          awaitPromise: true,
        })
        .catch((error: unknown) => {
          throw WORLD_GONE.test(String(error)) ? new WorldGone(firstLine(String(error))) : error;
        });
      const thrown = answer.exceptionDetails;
      if (thrown !== undefined) {
        const text = thrown.exception?.description ?? thrown.text;
        if (text.includes(this.key)) {
          // The check itself: the id named another world, or a document
          // the mark has not reached yet.
          throw new WorldGone('the evaluation did not reach the world of the reader');
        }
        throw new PageReadError(firstLine(text));
      }
      return answer.result.value as T;
    } catch (error) {
      // Only an expression that threw has shown that the world is the
      // right one; after anything else it is asked for again.
      if (!(error instanceof PageReadError) && this.context === context) {
        this.context = null;
      }
      throw error;
    }
  }

  /**
   * Evaluates an expression in the reader's world of the page's current
   * document and returns its value; a promise is awaited. The page cannot
   * observe the evaluation, and it grants no user activation.
   *
   * @throws {PageReadError} when the expression throws.
   */
  async evaluate<T>(expression: string): Promise<T> {
    let gone: WorldGone | null = null;
    for (let attempt = 0; attempt < READ_ATTEMPTS; attempt += 1) {
      if (attempt > 0) {
        await this.sleep(READ_RETRY_MS);
      }
      try {
        return await this.once<T>(expression);
      } catch (error) {
        if (!(error instanceof WorldGone)) {
          throw error;
        }
        gone = error;
      }
    }
    throw new PageReadError(
      `the page could not be read while it was navigating: ${gone?.message ?? ''}`,
    );
  }

  /** The title of the document. */
  title(): Promise<string> {
    return this.evaluate<string>('document.title');
  }

  /** The document as markup, doctype included. */
  content(): Promise<string> {
    return this.evaluate<string>(READ_CONTENT);
  }

  /**
   * Waits until an element matches a CSS selector, looking every
   * {@link DEFAULT_POLL_MS} and across navigations.
   *
   * @throws {ReadTimeoutError} when nothing matched in time.
   * @throws {PageReadError} when the selector is none.
   */
  async waitForSelector(selector: string, options: { timeout: number }): Promise<void> {
    const expression = `document.querySelector(${JSON.stringify(selector)}) !== null`;
    const deadline = this.now() + options.timeout;
    const timedOut = () =>
      new ReadTimeoutError(
        `Timeout ${options.timeout}ms exceeded while waiting for the selector ${selector}`,
      );
    for (;;) {
      const left = deadline - this.now();
      let timer: NodeJS.Timeout | undefined;
      // A page whose script never yields answers no evaluation: the wait
      // still ends on time.
      const late = new Promise<never>((_resolve, reject) => {
        timer = setTimeout(
          () => {
            reject(timedOut());
          },
          Math.max(0, left),
        );
      });
      const look = this.once<boolean>(expression);
      look.catch(() => undefined);
      try {
        if (await Promise.race([look, late])) {
          return;
        }
      } catch (error) {
        if (!(error instanceof WorldGone)) {
          throw error;
        }
      } finally {
        clearTimeout(timer);
      }
      const rest = deadline - this.now();
      if (rest <= 0) {
        throw timedOut();
      }
      await this.sleep(Math.min(this.pollMs, rest));
    }
  }
}

/**
 * The driver's page with its reading done by the reader: the session of
 * engine-core gets this instead of the driver's page, so that no read can
 * reach the driver. Navigation, load states, network events and the input
 * of `human: false` steps stay the driver's.
 */
export function pageWithReader(page: Page, reader: PageReader): DriverPage {
  // The driver's page offers everything the session asks for and more.
  const driver: DriverPage = page;
  return {
    url: () => driver.url(),
    title: () => reader.title(),
    content: () => reader.content(),
    evaluate: <T>(expression: string) => reader.evaluate<T>(expression),
    waitForSelector: (selector, options) => reader.waitForSelector(selector, options),
    mainFrame: () => driver.mainFrame(),
    frames: () => driver.frames(),
    goto: (url, options) => driver.goto(url, options),
    goBack: (options) => driver.goBack(options),
    waitForLoadState: (state, options) => driver.waitForLoadState(state, options),
    waitForURL: (url, options) => driver.waitForURL(url, options),
    click: (selector, options) => driver.click(selector, options),
    hover: (selector, options) => driver.hover(selector, options),
    fill: (selector, value, options) => driver.fill(selector, value, options),
    selectOption: (selector, value, options) => driver.selectOption(selector, value, options),
    keyboard: driver.keyboard,
    mouse: driver.mouse,
    on: driver.on.bind<DriverPage['on']>(driver),
  };
}
