import type { CaptureOptions, FlowAction, FlowEvent, PageResult } from '@kage/contract';
import { EXAMPLES } from '@kage/contract';
import { describe, expect, it } from 'vitest';

import type { Human } from '../behavior/index.js';
import { ConsoleLog } from '../console-log.js';

import { DEFAULT_STEP_TIMEOUT_MS, runFlow, type FlowHost } from './executor.js';

const pageResult = structuredClone(EXAMPLES['PageResult']?.[0]) as PageResult;
const capture: CaptureOptions = {
  dom: true,
  raw: false,
  xhr: null,
  console: false,
  screenshot: null,
  pdf: null,
};

/** A context that only writes down what a flow asked of it. */
function rig(
  options: {
    missing?: string[];
    text?: string;
    select?: { index: number; wanted: number } | null;
  } = {},
) {
  const calls: string[] = [];
  const note =
    (name: string) =>
    (...parameters: unknown[]) => {
      calls.push([name, ...parameters.filter((value) => typeof value !== 'object')].join(' '));
      return Promise.resolve();
    };
  const human = {
    click: note('human.click'),
    hover: note('human.hover'),
    type: note('human.type'),
    press: note('human.press'),
    chord: (modifiers: string[], key: string) => note('human.chord')(modifiers.join('+'), key),
    scrollTo: (depth: number) => note('human.scroll')(depth),
  } as unknown as Human;
  const missing = new Set(options.missing ?? []);
  const host: FlowHost = {
    human: () => Promise.resolve(human),
    typos: true,
    goto: note('goto'),
    back: note('back'),
    waitForSelector: (selector, timeout) =>
      missing.has(selector)
        ? Promise.reject(new Error('Timeout'))
        : note('waitForSelector')(selector, timeout),
    waitForUrl: (glob) =>
      glob === '**/never' ? Promise.reject(new Error('Timeout')) : note('waitForUrl')(glob),
    waitForNetworkIdle: note('waitForNetworkIdle'),
    waitForNavigation: note('waitForNavigation'),
    exists: (selector) => Promise.resolve(!missing.has(selector)),
    text: () => Promise.resolve(options.text ?? 'Listing, page 2'),
    selectState: () =>
      Promise.resolve(options.select === undefined ? { index: 0, wanted: 2 } : options.select),
    driver: {
      click: note('driver.click'),
      hover: note('driver.hover'),
      fill: note('driver.fill'),
      press: note('driver.press'),
      select: note('driver.select'),
      scroll: note('driver.scroll'),
    },
    extract: (asked) => {
      calls.push(`extract dom=${String(asked.dom)}`);
      return Promise.resolve(pageResult);
    },
    screenshot: (asked) => {
      calls.push(`screenshot ${asked.format}`);
      return Promise.resolve(pageResult);
    },
    pdf: () => {
      calls.push('pdf');
      return Promise.resolve(pageResult);
    },
  };
  const events: FlowEvent[] = [];
  const run = (actions: FlowAction[], timeout?: number, now?: () => number) =>
    runFlow(
      { actions, capture, ...(timeout === undefined ? {} : { timeout_ms: timeout }) },
      host,
      (event) => events.push(event),
      now,
    );
  return { calls, events, run, host };
}

describe('the flow executor', () => {
  it('does every step through the mouse and keyboard unless told otherwise', async () => {
    const { calls, events, run } = rig();
    const ok = await run([
      { action: 'goto', url: 'https://example.org/' },
      { action: 'click', selector: '#a' },
      { action: 'hover', selector: '#b' },
      { action: 'type', selector: '#q', text: 'used cars' },
      { action: 'press', key: 'Enter' },
      { action: 'scroll', depth: 0.5 },
      { action: 'select', selector: '#sort', value: 'price' },
      { action: 'back' },
    ]);
    expect(ok).toBe(true);
    expect(calls).toEqual([
      `goto https://example.org/ ${DEFAULT_STEP_TIMEOUT_MS}`,
      `waitForSelector #a ${DEFAULT_STEP_TIMEOUT_MS}`,
      'human.click #a',
      'waitForNavigation 1200',
      `waitForSelector #b ${DEFAULT_STEP_TIMEOUT_MS}`,
      'human.hover #b',
      `waitForSelector #q ${DEFAULT_STEP_TIMEOUT_MS}`,
      'human.click #q',
      'human.type used cars',
      'human.press Enter',
      'waitForNavigation 1200',
      'human.scroll 0.5',
      `waitForSelector #sort ${DEFAULT_STEP_TIMEOUT_MS}`,
      // A select is opened by a click and chosen with the keyboard: two options down.
      'human.click #sort',
      'human.press ArrowDown',
      'human.press ArrowDown',
      'human.press Enter',
      'human.chord Alt ArrowLeft',
      'waitForNavigation 5000',
    ]);
    expect(
      events.map((event) =>
        event.type === 'step' ? [event.index, event.action, event.ok] : event,
      ),
    ).toEqual(
      ['goto', 'click', 'hover', 'type', 'press', 'scroll', 'select', 'back'].map(
        (action, index) => [index, action, true],
      ),
    );
    expect(
      events.every(
        (event) => event.type === 'step' && event.capture === null && event.error === null,
      ),
    ).toBe(true);
  });

  it('uses the driver for a step that says human: false', async () => {
    const { calls, run } = rig({ select: { index: 2, wanted: 0 } });
    expect(
      await run([
        { action: 'click', selector: '#a', human: false, timeout_ms: 900 },
        { action: 'hover', selector: '#b', human: false },
        { action: 'type', selector: '#q', text: 'x', human: false },
        { action: 'press', key: 'Enter', human: false },
        { action: 'scroll', depth: 1, human: false },
        { action: 'select', selector: '#s', value: 'v', human: false },
        { action: 'back', human: false },
        // Upwards in a select: the wanted option is two above the selected one.
        { action: 'select', selector: '#s', value: 'v' },
      ]),
    ).toBe(true);
    expect(calls.filter((call) => !call.startsWith('waitForSelector'))).toEqual([
      'driver.click #a 900',
      `driver.hover #b ${DEFAULT_STEP_TIMEOUT_MS}`,
      `driver.fill #q x ${DEFAULT_STEP_TIMEOUT_MS}`,
      'driver.press Enter',
      'driver.scroll 1',
      `driver.select #s v ${DEFAULT_STEP_TIMEOUT_MS}`,
      `back ${DEFAULT_STEP_TIMEOUT_MS}`,
      'human.click #s',
      'human.press ArrowUp',
      'human.press ArrowUp',
      'human.press Enter',
    ]);
  });

  it('waits for one condition, captures where asked, and checks what it is told to check', async () => {
    const { calls, events, run } = rig();
    expect(
      await run([
        { action: 'wait_for', selector: '#results' },
        { action: 'wait_for', url: '**/listing' },
        { action: 'wait_for', network_idle: true },
        { action: 'wait_for', ms: 5 },
        { action: 'extract', capture },
        { action: 'screenshot', options: { full_page: false, format: 'png' } },
        { action: 'pdf', options: {} },
        { action: 'assert', selector: '#results' },
        { action: 'assert', text_contains: 'page 2' },
      ]),
    ).toBe(true);
    expect(calls).toEqual([
      `waitForSelector #results ${DEFAULT_STEP_TIMEOUT_MS}`,
      'waitForUrl **/listing',
      `waitForNetworkIdle ${DEFAULT_STEP_TIMEOUT_MS}`,
      'extract dom=true',
      'screenshot png',
      'pdf',
    ]);
    const captured = events.filter((event) => event.type === 'step' && event.capture !== null);
    expect(captured.map((event) => (event.type === 'step' ? event.action : ''))).toEqual([
      'extract',
      'screenshot',
      'pdf',
    ]);
  });

  it.each<[string, FlowAction, string, Parameters<typeof rig>[0]]>([
    [
      'a missing element',
      { action: 'click', selector: '#gone', timeout_ms: 700 },
      'no element matched #gone within 700 ms',
      { missing: ['#gone'] },
    ],
    [
      'an address that never comes',
      { action: 'wait_for', url: '**/never', timeout_ms: 500 },
      'the URL did not match **/never within 500 ms',
      {},
    ],
    [
      'two conditions at once',
      { action: 'wait_for', selector: '#a', ms: 5 },
      'wait_for takes exactly one of selector, url, network_idle and ms',
      {},
    ],
    [
      'no condition',
      { action: 'wait_for' },
      'wait_for takes exactly one of selector, url, network_idle and ms',
      {},
    ],
    [
      'an assertion without anything to assert',
      { action: 'assert' },
      'assert takes exactly one of selector and text_contains',
      {},
    ],
    [
      'a selector that matches nothing',
      { action: 'assert', selector: '#gone' },
      'no element matches #gone',
      { missing: ['#gone'] },
    ],
    [
      'a text that is not there',
      { action: 'assert', text_contains: 'page 3' },
      'the page text does not contain "page 3"',
      {},
    ],
    [
      'a select that is none',
      { action: 'select', selector: '#s', value: 'v' },
      '#s is not a select element',
      { select: null },
    ],
    [
      'an option that is not there',
      { action: 'select', selector: '#s', value: 'v' },
      '#s has no option with the value "v"',
      { select: { index: 0, wanted: -1 } },
    ],
  ])('fails a step for %s and runs nothing after it', async (_name, action, message, options) => {
    const { calls, events, run } = rig(options);
    expect(await run([action, { action: 'goto', url: 'https://example.org/after' }])).toBe(false);
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({
      type: 'step',
      index: 0,
      ok: false,
      error: message,
      capture: null,
    });
    expect(calls.some((call) => call.startsWith('goto'))).toBe(false);
  });

  it('ends a step that takes longer than its budget, and a flow that takes longer than its own', async () => {
    const stuck = rig();
    const events: FlowEvent[] = [];
    const ok = await runFlow(
      { actions: [{ action: 'wait_for', network_idle: true, timeout_ms: 120 }], capture },
      { ...stuck.host, waitForNetworkIdle: () => new Promise<void>(() => undefined) },
      (event) => events.push(event),
    );
    expect(ok).toBe(false);
    expect(events).toMatchObject([{ ok: false, error: 'wait_for did not finish within 120 ms' }]);

    // The clock of the flow has run out before its second step.
    const late = rig();
    let now = 0;
    const clock = () => {
      now += 600;
      return now;
    };
    expect(
      await late.run(
        [{ action: 'wait_for', ms: 1 }, { action: 'wait_for', ms: 1 }, { action: 'back' }],
        1000,
        clock,
      ),
    ).toBe(false);
    expect(late.events).toMatchObject([
      { index: 0, ok: true },
      { index: 1, ok: false, error: 'the budget of the flow ran out before this step' },
    ]);
  });

  it('reports a failure by its first line, whatever was thrown', async () => {
    const broken = rig();
    const events: FlowEvent[] = [];
    await runFlow(
      { actions: [{ action: 'goto', url: 'https://example.org/' }], capture },
      {
        ...broken.host,
        // eslint-disable-next-line @typescript-eslint/prefer-promise-reject-errors -- a driver may reject with anything
        goto: () => Promise.reject('timeout: the page did not load\nCall log: ...'),
      },
      (event) => events.push(event),
    );
    expect(events).toMatchObject([{ ok: false, error: 'timeout: the page did not load' }]);
  });
});

describe('the console log of a context', () => {
  it('gives an operation the entries added since its mark, however many were dropped before', () => {
    const log = new ConsoleLog();
    const entry = (text: string) => ({
      level: 'info' as const,
      source: 'console-api',
      text,
      url: null,
    });
    log.add(entry('before'));
    const mark = log.mark();
    expect(log.since(mark)).toEqual([]);
    log.add(entry('one'));
    log.add(entry('two'));
    expect(log.since(mark).map((each) => each.text)).toEqual(['one', 'two']);
    for (let count = 0; count < 5000; count += 1) {
      log.add(entry(`n${count}`));
    }
    // Old entries are gone; marks still count from the beginning.
    expect(log.mark()).toBe(5003);
    expect(log.since(mark).length).toBeLessThanOrEqual(4000);
    expect(log.since(log.mark() - 2).map((each) => each.text)).toEqual(['n4998', 'n4999']);
  });
});
