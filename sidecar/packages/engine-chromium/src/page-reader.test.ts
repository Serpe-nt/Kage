import vm from 'node:vm';

import type { CDPSession, Page } from 'patchright';
import { describe, expect, it } from 'vitest';

import {
  DEFAULT_POLL_MS,
  PageReadError,
  PageReader,
  ReadTimeoutError,
  classifyNavigationError,
  pageWithReader,
} from './index.js';

/**
 * The page reader against a browser made of `node:vm` contexts: every
 * world is a context of its own, and the fake evaluates what the reader
 * sends the way the debugger does, so the guard the reader puts around an
 * expression really runs. What a real Chrome does with it is in the parity
 * suite and in `engine.integration.test.ts`.
 */

interface Call {
  readonly method: string;
  readonly params: Record<string, unknown> | undefined;
}

/** What a page's document offers the expressions of these tests. */
interface FakeDocument {
  title: string;
  doctype: object | null;
  documentElement: { outerHTML: string } | null;
  querySelector(selector: string): object | null;
}

const documentOf = (title: string, change: Partial<FakeDocument> = {}): FakeDocument => ({
  title,
  doctype: {},
  documentElement: { outerHTML: `<html><head><title>${title}</title></head></html>` },
  querySelector: () => null,
  ...change,
});

class XMLSerializer {
  serializeToString(): string {
    return '<!DOCTYPE html>';
  }
}

/** One renderer process as the debugger shows it: worlds by a small number. */
class FakeBrowser {
  readonly calls: Call[] = [];
  /** The script registered for new documents: its world and its source. */
  registered: { worldName: string; source: string } | null = null;
  frameId = 'F1';
  private worlds = new Map<number, vm.Context>();
  private readerWorld: number | null = null;
  private nextId = 3;
  private document: FakeDocument = documentOf('blank');
  /** What the next commands answer instead; taken once each. */
  readonly failures: { method: string; message: string }[] = [];
  /** When set, an evaluation never answers. */
  hang = false;

  readonly session = {
    send: (method: string, params?: Record<string, unknown>) => this.send(method, params),
  } as unknown as CDPSession;

  /** A world of the current document; `marked` is what the registered script does. */
  private world(marked: boolean, top: boolean, document: object = this.document): vm.Context {
    const sandbox: Record<string, unknown> = { document, XMLSerializer };
    sandbox['self'] = sandbox;
    sandbox['top'] = top ? sandbox : {};
    const context = vm.createContext(sandbox);
    if (marked && this.registered !== null) {
      vm.runInContext(this.registered.source, context);
    }
    return context;
  }

  /**
   * A new document. In a new process the numbers start again, and `squat`
   * puts another world of the new page at a number of its choice.
   */
  navigate(
    document: FakeDocument,
    options: { newProcess?: boolean; squat?: [number, vm.Context] } = {},
  ): void {
    this.document = document;
    this.worlds = new Map();
    this.readerWorld = null;
    if (options.newProcess === true) {
      this.nextId = 1;
    }
    if (options.squat !== undefined) {
      this.worlds.set(...options.squat);
    }
  }

  /** A world that is not the reader's: the page's own, or the driver's. */
  foreign(document: object, top = true): vm.Context {
    return this.world(false, top, document);
  }

  /** The reader's world of an iframe of the page: marked, and not the top frame. */
  inFrame(document: FakeDocument): vm.Context {
    return this.world(true, false, document);
  }

  private async send(method: string, params?: Record<string, unknown>): Promise<unknown> {
    this.calls.push({ method, params });
    const failure = this.failures.findIndex((entry) => entry.method === method);
    if (failure >= 0) {
      const [{ message } = { message: '' }] = this.failures.splice(failure, 1);
      throw new Error(`cdpSession.send: Protocol error (${method}): ${message}`);
    }
    switch (method) {
      case 'Page.enable':
        return {};
      case 'Page.addScriptToEvaluateOnNewDocument':
        this.registered = {
          worldName: String(params?.['worldName']),
          source: String(params?.['source']),
        };
        return { identifier: '1' };
      case 'Page.getFrameTree':
        return { frameTree: { frame: { id: this.frameId } } };
      case 'Page.createIsolatedWorld': {
        if (params?.['frameId'] !== this.frameId) {
          throw new Error('cdpSession.send: Protocol error: No frame for given id found');
        }
        if (this.readerWorld === null) {
          while (this.worlds.has(this.nextId)) {
            this.nextId += 1;
          }
          this.readerWorld = this.nextId;
          this.worlds.set(this.readerWorld, this.world(true, true));
        }
        return { executionContextId: this.readerWorld };
      }
      case 'Runtime.evaluate': {
        if (this.hang) {
          return new Promise(() => undefined);
        }
        const context = this.worlds.get(Number(params?.['contextId']));
        if (context === undefined) {
          throw new Error(
            'cdpSession.send: Protocol error (Runtime.evaluate): Cannot find context with specified id',
          );
        }
        try {
          const value: unknown = await vm.runInContext(String(params?.['expression']), context);
          return { result: { type: typeof value, value } };
        } catch (error) {
          // As the debugger describes what was thrown: its name, its
          // message, and where.
          const { name, message } = error as Error;
          return {
            exceptionDetails: {
              text: 'Uncaught',
              exception: { description: `${name}: ${message}\n    at <anonymous>:1:1` },
            },
          };
        }
      }
      default:
        throw new Error(`the reader sent ${method}, which it never should`);
    }
  }

  methods(): string[] {
    return this.calls.map((call) => call.method);
  }

  evaluations(): Record<string, unknown>[] {
    return this.calls
      .filter((call) => call.method === 'Runtime.evaluate')
      .map((call) => call.params ?? {});
  }
}

/** A clock that only moves when the reader rests. */
const fakeTime = () => {
  const time = { now: 0, sleeps: [] as number[] };
  return {
    time,
    options: {
      now: () => time.now,
      sleep: (ms: number) => {
        time.sleeps.push(ms);
        time.now += ms;
        return Promise.resolve();
      },
    },
  };
};

const attach = async (
  browser: FakeBrowser,
  options: Parameters<typeof PageReader.attach>[1] = {},
) => PageReader.attach(browser.session, { key: 'kTESTKEY', ...options });

describe('PageReader', () => {
  it('prepares the session: the Page domain, and a marked world for every document', async () => {
    const browser = new FakeBrowser();
    await attach(browser);
    expect(browser.calls).toEqual([
      { method: 'Page.enable', params: undefined },
      {
        method: 'Page.addScriptToEvaluateOnNewDocument',
        params: {
          source: 'globalThis["kTESTKEY"] = true;',
          worldName: 'kTESTKEY',
          // The document that is open already gets the world too.
          runImmediately: true,
        },
      },
    ]);
  });

  it('draws a name nobody can guess when none is given, another for every context', async () => {
    const [first, second] = [new FakeBrowser(), new FakeBrowser()];
    await PageReader.attach(first.session);
    await PageReader.attach(second.session);
    expect(first.registered?.worldName).toMatch(/^k[0-9a-f]{24}$/);
    expect(first.registered?.source).toBe(`globalThis["${first.registered?.worldName}"] = true;`);
    expect(second.registered?.worldName).not.toBe(first.registered?.worldName);
  });

  it('evaluates in its own world, by value, and never as a user gesture', async () => {
    const browser = new FakeBrowser();
    const reader = await attach(browser);
    browser.navigate(documentOf('Landing'));
    browser.calls.length = 0;

    expect(await reader.evaluate<string>('document.title')).toBe('Landing');
    expect(await reader.evaluate<number>('Promise.resolve(6 * 7)')).toBe(42);
    expect(await reader.evaluate<{ a: number[] }>('({ a: [1, 2] })')).toEqual({ a: [1, 2] });

    // The world is asked for once per document, by the reader's own name.
    expect(browser.methods()).toEqual([
      'Page.getFrameTree',
      'Page.createIsolatedWorld',
      'Runtime.evaluate',
      'Runtime.evaluate',
      'Runtime.evaluate',
    ]);
    expect(browser.calls[1]?.params).toEqual({ frameId: 'F1', worldName: 'kTESTKEY' });
    for (const sent of browser.evaluations()) {
      expect(Object.keys(sent).sort()).toEqual([
        'awaitPromise',
        'contextId',
        'expression',
        'returnByValue',
      ]);
      expect(sent).toMatchObject({ contextId: 3, returnByValue: true, awaitPromise: true });
      // What the driver sends and a page can see the effect of.
      expect(sent).not.toHaveProperty('userGesture');
    }
  });

  it('reads the title and the document as the driver serializes them', async () => {
    const browser = new FakeBrowser();
    const reader = await attach(browser);
    browser.navigate(documentOf('Fixture home'));
    expect(await reader.title()).toBe('Fixture home');
    expect(await reader.content()).toBe(
      '<!DOCTYPE html><html><head><title>Fixture home</title></head></html>',
    );
    // A document without a doctype, and one that is being replaced.
    browser.navigate(documentOf('Quirks', { doctype: null }));
    expect(await reader.content()).toBe('<html><head><title>Quirks</title></head></html>');
    browser.navigate(documentOf('Empty', { doctype: null, documentElement: null }));
    expect(await reader.content()).toBe('');
  });

  it('asks for the world again when the document went away', async () => {
    const browser = new FakeBrowser();
    const { time, options } = fakeTime();
    const reader = await attach(browser, options);
    browser.navigate(documentOf('First'));
    expect(await reader.title()).toBe('First');

    browser.navigate(documentOf('Second'));
    browser.calls.length = 0;
    expect(await reader.title()).toBe('Second');
    // The old number is gone; one rest, the world of the new document, the answer.
    expect(browser.methods()).toEqual([
      'Runtime.evaluate',
      'Page.getFrameTree',
      'Page.createIsolatedWorld',
      'Runtime.evaluate',
    ]);
    expect(time.sleeps).toEqual([50]);
  });

  it("never reads in another world that took the old world's number", async () => {
    const browser = new FakeBrowser();
    const reader = await attach(browser, fakeTime().options);
    browser.navigate(documentOf('Old page'));
    expect(await reader.title()).toBe('Old page');
    const staleId = Number(browser.evaluations().at(-1)?.['contextId']);

    // Another process hands the number out again, to the new page's own
    // world. Whatever is read there, the page could notice.
    const touched: string[] = [];
    const pageWorld = browser.foreign(
      new Proxy(
        {},
        {
          get: (_target, name) => {
            touched.push(String(name));
            return 'the page saw this read';
          },
        },
      ),
    );
    browser.navigate(documentOf('New page'), { newProcess: true, squat: [staleId, pageWorld] });

    expect(await reader.title()).toBe('New page');
    expect(touched).toEqual([]);
    // Nothing was left behind in the page's world either.
    expect(vm.runInContext('Object.keys(globalThis)', pageWorld)).toEqual([
      'document',
      'XMLSerializer',
      'self',
      'top',
    ]);
  });

  it('never reads in a frame, even in its own world of that frame', async () => {
    const browser = new FakeBrowser();
    const reader = await attach(browser, fakeTime().options);
    browser.navigate(documentOf('Top'));
    expect(await reader.title()).toBe('Top');
    const staleId = Number(browser.evaluations().at(-1)?.['contextId']);
    browser.navigate(documentOf('Top again'), {
      newProcess: true,
      squat: [staleId, browser.inFrame(documentOf('An advertisement'))],
    });
    expect(await reader.title()).toBe('Top again');
  });

  it('reports an expression that throws, once, and keeps its world', async () => {
    const browser = new FakeBrowser();
    const { time, options } = fakeTime();
    const reader = await attach(browser, options);
    browser.navigate(
      documentOf('Strict', {
        querySelector: (selector) => {
          throw new SyntaxError(`'${selector}' is not a valid selector.`);
        },
      }),
    );
    const read = reader.evaluate('document.querySelector("][")');
    await expect(read).rejects.toBeInstanceOf(PageReadError);
    await expect(read).rejects.toThrow("SyntaxError: '][' is not a valid selector.");
    expect(time.sleeps).toEqual([]);
    browser.calls.length = 0;
    expect(await reader.title()).toBe('Strict');
    expect(browser.methods()).toEqual(['Runtime.evaluate']);
  });

  it('gives up on a page that keeps navigating, and says so', async () => {
    const browser = new FakeBrowser();
    const { time, options } = fakeTime();
    const reader = await attach(browser, options);
    for (let attempt = 0; attempt < 8; attempt += 1) {
      browser.failures.push({
        method: 'Page.createIsolatedWorld',
        message: 'No frame for given id found',
      });
    }
    const read = reader.title();
    await expect(read).rejects.toBeInstanceOf(PageReadError);
    await expect(read).rejects.toThrow(/could not be read while it was navigating: .*No frame/);
    expect(time.sleeps).toEqual([50, 50, 50, 50, 50, 50, 50]);
    // The next read starts anew.
    expect(await reader.title()).toBe('blank');
  });

  it('retries after every way the debugger says that a document is gone', async () => {
    for (const message of [
      'Cannot find context with specified id',
      'Execution context was destroyed.',
      'Inspected target navigated or closed',
      'Cannot find default execution context',
    ]) {
      const browser = new FakeBrowser();
      const reader = await attach(browser, fakeTime().options);
      browser.failures.push({ method: 'Runtime.evaluate', message });
      expect(await reader.title(), message).toBe('blank');
    }
    const browser = new FakeBrowser();
    const reader = await attach(browser, fakeTime().options);
    browser.failures.push({
      method: 'Page.createIsolatedWorld',
      message: 'Could not create isolated world',
    });
    expect(await reader.title()).toBe('blank');
  });

  it('hands on what is no navigation: a closed page stays a closed page', async () => {
    const browser = new FakeBrowser();
    const { time, options } = fakeTime();
    const reader = await attach(browser, options);
    for (const method of ['Page.getFrameTree', 'Runtime.evaluate']) {
      browser.failures.push({
        method,
        message: 'Target page, context or browser has been closed',
      });
      const read = reader.title();
      await expect(read).rejects.toThrow('has been closed');
      expect(classifyNavigationError(await read.catch((error: unknown) => error)).code).toBe(
        'aborted',
      );
    }
    expect(time.sleeps).toEqual([]);
    // Nothing of the failure is kept.
    expect(await reader.title()).toBe('blank');
  });

  describe('waitForSelector', () => {
    it('looks every tenth of a second until an element is there', async () => {
      const browser = new FakeBrowser();
      const { time, options } = fakeTime();
      const reader = await attach(browser, options);
      const asked: string[] = [];
      browser.navigate(
        documentOf('Late', {
          querySelector: (selector) => {
            asked.push(selector);
            return asked.length >= 4 ? {} : null;
          },
        }),
      );
      await reader.waitForSelector('#late [data-x="1"]', { timeout: 5000 });
      expect(asked).toEqual(Array.from({ length: 4 }, () => '#late [data-x="1"]'));
      expect(time.sleeps).toEqual([DEFAULT_POLL_MS, DEFAULT_POLL_MS, DEFAULT_POLL_MS]);
      expect(DEFAULT_POLL_MS).toBe(100);
    });

    it('rests as long as it is told, and never past the deadline', async () => {
      const browser = new FakeBrowser();
      const { time, options } = fakeTime();
      const reader = await attach(browser, { ...options, pollMs: 40 });
      const wait = reader.waitForSelector('#never', { timeout: 100 });
      await expect(wait).rejects.toBeInstanceOf(ReadTimeoutError);
      await expect(wait).rejects.toThrow(
        'Timeout 100ms exceeded while waiting for the selector #never',
      );
      expect(time.sleeps).toEqual([40, 40, 20]);
      // Named as the driver names its own timeouts: the contract's `timeout`.
      const error = await wait.catch((thrown: unknown) => thrown);
      expect((error as Error).name).toBe('TimeoutError');
      expect(classifyNavigationError(error)).toMatchObject({ code: 'timeout' });
    });

    it('keeps waiting across a navigation, and finds the element of the page behind', async () => {
      const browser = new FakeBrowser();
      const time = { now: 0 };
      let rests = 0;
      const reader = await attach(browser, {
        now: () => time.now,
        sleep: (ms: number) => {
          time.now += ms;
          rests += 1;
          if (rests === 2) {
            // The challenge page gives way to the page behind it.
            browser.navigate(documentOf('Behind', { querySelector: () => ({}) }));
          }
          return Promise.resolve();
        },
      });
      browser.navigate(documentOf('Challenge'));
      await reader.waitForSelector('#content', { timeout: 5000 });
      // Two looks at the challenge, one that found its world gone, one at the page behind.
      expect(browser.evaluations()).toHaveLength(4);
      expect(rests).toBe(3);
    });

    it('fails at once for a selector that is none', async () => {
      const browser = new FakeBrowser();
      const { time, options } = fakeTime();
      const reader = await attach(browser, options);
      browser.navigate(
        documentOf('Strict', {
          querySelector: () => {
            throw new SyntaxError("'][' is not a valid selector.");
          },
        }),
      );
      await expect(reader.waitForSelector('][', { timeout: 5000 })).rejects.toBeInstanceOf(
        PageReadError,
      );
      expect(time.sleeps).toEqual([]);
    });

    it('ends on time for a page that answers nothing', async () => {
      const browser = new FakeBrowser();
      const reader = await attach(browser);
      browser.hang = true;
      const started = Date.now();
      await expect(reader.waitForSelector('#x', { timeout: 60 })).rejects.toBeInstanceOf(
        ReadTimeoutError,
      );
      expect(Date.now() - started).toBeLessThan(1500);
    });
  });
});

describe('pageWithReader', () => {
  /** A driver's page that records what is asked of it and refuses every read. */
  const driverPage = () => {
    const asked: [string, ...unknown[]][] = [];
    const note =
      (name: string, answer?: unknown) =>
      (...args: unknown[]) => {
        asked.push([name, ...args]);
        return answer;
      };
    const refuse = (name: string) => () => {
      throw new Error(`${name} went to the driver, which marks it as a user gesture`);
    };
    const mainFrame = { url: () => 'http://fixture.kage.test/' };
    const keyboard = { press: note('keyboard.press', Promise.resolve()) };
    const mouse = { wheel: note('mouse.wheel', Promise.resolve()) };
    const listeners = new Map<string, unknown>();
    const page = {
      url: () => 'http://fixture.kage.test/',
      mainFrame: () => mainFrame,
      frames: () => [mainFrame],
      goto: note('goto', Promise.resolve(null)),
      goBack: note('goBack', Promise.resolve(null)),
      waitForLoadState: note('waitForLoadState', Promise.resolve()),
      click: note('click', Promise.resolve()),
      hover: note('hover', Promise.resolve()),
      fill: note('fill', Promise.resolve()),
      selectOption: note('selectOption', Promise.resolve([])),
      keyboard,
      mouse,
      on(this: unknown, event: string, listener: unknown) {
        // The driver's own `on` needs its page as `this`.
        if (this !== page) {
          throw new Error('on was called without its page');
        }
        listeners.set(event, listener);
        return page;
      },
      title: refuse('title'),
      content: refuse('content'),
      evaluate: refuse('evaluate'),
      waitForSelector: refuse('waitForSelector'),
    };
    return { page: page as unknown as Page, asked, mainFrame, keyboard, mouse, listeners };
  };

  it('sends every read to the reader and none to the driver', async () => {
    const browser = new FakeBrowser();
    const reader = await attach(browser);
    browser.navigate(documentOf('Read here', { querySelector: () => ({}) }));
    const driver = driverPage();
    const page = pageWithReader(driver.page, reader);

    expect(await page.title()).toBe('Read here');
    expect(await page.content()).toContain('<title>Read here</title>');
    expect(await page.evaluate<number>('1 + 1')).toBe(2);
    await page.waitForSelector('#x', { state: 'attached', timeout: 1000 });
    expect(driver.asked).toEqual([]);
  });

  it('leaves navigation, waits and the input of unhuman steps to the driver', async () => {
    const browser = new FakeBrowser();
    const driver = driverPage();
    const page = pageWithReader(driver.page, await attach(browser));
    browser.calls.length = 0;

    expect(page.url()).toBe('http://fixture.kage.test/');
    // The session compares frames by identity: they must be the driver's own.
    expect(page.mainFrame()).toBe(driver.mainFrame);
    expect(page.frames()).toEqual([driver.mainFrame]);
    await page.goto('http://fixture.kage.test/a', { waitUntil: 'load', timeout: 1000 });
    await page.goBack({ waitUntil: 'commit', timeout: 2000 });
    await page.waitForLoadState('networkidle', { timeout: 3000 });
    await page.click('#a', { timeout: 5 });
    await page.hover('#b', { timeout: 6 });
    await page.fill('#c', 'text', { timeout: 7 });
    await page.selectOption('#d', 'two', { timeout: 8 });
    await page.keyboard.press('End');
    await page.mouse.wheel(0, 240);
    expect(driver.asked).toEqual([
      ['goto', 'http://fixture.kage.test/a', { waitUntil: 'load', timeout: 1000 }],
      ['goBack', { waitUntil: 'commit', timeout: 2000 }],
      ['waitForLoadState', 'networkidle', { timeout: 3000 }],
      ['click', '#a', { timeout: 5 }],
      ['hover', '#b', { timeout: 6 }],
      ['fill', '#c', 'text', { timeout: 7 }],
      ['selectOption', '#d', 'two', { timeout: 8 }],
      ['keyboard.press', 'End'],
      ['mouse.wheel', 0, 240],
    ]);
    expect(page.keyboard).toBe(driver.keyboard);
    expect(page.mouse).toBe(driver.mouse);
    const onLoad = () => undefined;
    page.on('load', onLoad);
    expect(driver.listeners.get('load')).toBe(onLoad);
    // None of it touched the reader's session.
    expect(browser.calls).toEqual([]);
  });
});
