import type { CaptureOptions, FlowAction, FlowEvent, FlowPace, PageResult } from '@kage/contract';
import { EXAMPLES } from '@kage/contract';
import { describe, expect, it } from 'vitest';

import {
  HUMAN_BOUNDS,
  Human,
  motorProfile,
  tempo,
  type Clock,
  type InputDevice,
  type PageSurface,
} from '../behavior/index.js';
import { ConsoleLog } from '../console-log.js';
import { SeededRandom } from '../seeded-random.js';

import {
  CAPTURE_RESERVE_MS,
  DEFAULT_STEP_TIMEOUT_MS,
  SCROLL_RESERVE_MS,
  NAVIGATION_WATCH_MS,
  FlowStepError,
  runFlow,
  type FlowHost,
  type PageMark,
} from './executor.js';
import { urlMatcher } from './url-glob.js';

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
    /** The tab has no page to go back to. */
    nowhere?: boolean;
  } = {},
) {
  const calls: string[] = [];
  /** Until when each human scroll was told it may go on. */
  const scrollDeadlines: number[] = [];
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
    scrollTo: (depth: number, deadline?: number) => {
      scrollDeadlines.push(deadline ?? Number.POSITIVE_INFINITY);
      return note('human.scroll')(depth);
    },
    // What only a paced flow asks for: written down like everything else,
    // so that a flow without a pace shows that it asked for none of it.
    dwell: (ms: number, how: { wander: boolean }) => note('human.dwell')(ms, how.wander),
    drawPacedDwell: () => {
      calls.push('human.drawPacedDwell');
      return 5000;
    },
    drawThinking: () => {
      calls.push('human.drawThinking');
      return 500;
    },
  } as unknown as Human;
  const missing = new Set(options.missing ?? []);
  const host: FlowHost = {
    human: () => Promise.resolve(human),
    typos: true,
    goto: note('goto'),
    back: async (timeout, by) => {
      // The keys go first, as in a session; the watch for the page before is the host's.
      await by?.();
      if (options.nowhere === true) {
        throw new FlowStepError('there is no page to go back to: the history of the tab ends here');
      }
      await note('back')(timeout);
    },
    waitForSelector: (selector, timeout) =>
      missing.has(selector)
        ? Promise.reject(new Error('Timeout'))
        : note('waitForSelector')(selector, timeout),
    waitForUrl: (glob) =>
      glob === '**/never' ? Promise.reject(new Error('Timeout')) : note('waitForUrl')(glob),
    waitForNetworkIdle: note('waitForNetworkIdle'),
    mark: () => ({ response: null, address: 'https://example.org/' }),
    waitForNavigation: async (since, timeout) => {
      await note('waitForNavigation')(since, timeout);
      return false;
    },
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
  const outcome = (actions: FlowAction[], timeout?: number, now?: () => number) =>
    runFlow(
      { actions, capture, ...(timeout === undefined ? {} : { timeout_ms: timeout }) },
      host,
      (event) => events.push(event),
      now,
    );
  /** Runs a flow and says whether every step succeeded. */
  const run = async (actions: FlowAction[], timeout?: number, now?: () => number) =>
    (await outcome(actions, timeout, now)).ok;
  return { calls, events, run, outcome, host, scrollDeadlines };
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
      // The keyboard's way back is watched by the host like the driver's.
      'human.chord Alt ArrowLeft',
      `back ${DEFAULT_STEP_TIMEOUT_MS}`,
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
    // Keys that led nowhere are a failed step like the driver's (F38): a
    // way back that reports success moved nothing and says it did.
    [
      'a way back by the keyboard that leads nowhere',
      { action: 'back' },
      'there is no page to go back to: the history of the tab ends here',
      { nowhere: true },
    ],
    [
      'a way back by the driver that leads nowhere',
      { action: 'back', human: false },
      'there is no page to go back to: the history of the tab ends here',
      { nowhere: true },
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

  it('lets a human scroll go on only as long as its step has time, less a reserve for the last flick', async () => {
    const { run, scrollDeadlines } = rig();
    const before = performance.now();
    expect(
      await run([
        { action: 'scroll', depth: 0.6 },
        { action: 'scroll', depth: 0.6, timeout_ms: 9000 },
        // Less time than the reserve: no flick is begun at all.
        { action: 'scroll', depth: 0.6, timeout_ms: 500 },
      ]),
    ).toBe(true);
    const after = performance.now();
    const [byDefault, given, none] = scrollDeadlines;
    // A page too long for the step is scrolled until then, and the step
    // ends inside its budget: it is not failed by the time it was given.
    expect(byDefault).toBeGreaterThanOrEqual(before + DEFAULT_STEP_TIMEOUT_MS - SCROLL_RESERVE_MS);
    expect(byDefault).toBeLessThanOrEqual(after + DEFAULT_STEP_TIMEOUT_MS - SCROLL_RESERVE_MS);
    expect(given).toBeLessThanOrEqual(after + 9000 - SCROLL_RESERVE_MS);
    expect(given).toBeLessThan(byDefault ?? 0);
    expect(none).toBeLessThanOrEqual(after);
  });

  it('ends a step that takes longer than its budget, and a flow that takes longer than its own', async () => {
    const stuck = rig();
    const events: FlowEvent[] = [];
    const { ok } = await runFlow(
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

  it('reports a wait for an address in the words of the context when it has better ones', async () => {
    const { host } = rig();
    const events: FlowEvent[] = [];
    const error = 'the URL matched **/slow, and its page did not finish loading within 300 ms';
    await runFlow(
      { actions: [{ action: 'wait_for', url: '**/slow', timeout_ms: 300 }], capture },
      { ...host, waitForUrl: () => Promise.reject(new FlowStepError(error)) },
      (event) => events.push(event),
    );
    expect(events).toMatchObject([{ action: 'wait_for', ok: false, error, paced_ms: null }]);
  });
});

const SEED = '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac';
const seedOf = (number: number) => number.toString(16).padStart(2, '0').repeat(32);

/** One pause a paced flow made: before or after which step, how long, and whether the hand wandered. */
interface Pause {
  readonly step: number;
  readonly ms: number;
  readonly wander: boolean;
}

/**
 * A paced flow with nothing real in it: a persona as the session makes one
 * (its hand, its tempo, the stream of the flow) at a mouse and keyboard
 * that go nowhere, on a clock that only counts, in a context that says
 * which clicks and keys leave the page.
 */
function pacedRig(options: { seed?: string; persona?: string; leaves?: readonly string[] } = {}) {
  const persona = options.persona ?? SEED;
  const seed = options.seed ?? SEED;
  const leaves = new Set(options.leaves ?? []);
  let now = 0;
  const clock: Clock = {
    now: () => now,
    sleep: (ms) => {
      now += Math.max(0, ms);
      return Promise.resolve();
    },
  };
  let pointer = { x: 960, y: 540 };
  const nothing = () => Promise.resolve();
  const device: InputDevice = {
    screen: { width: 1920, height: 1080 },
    pointerPosition: () => Promise.resolve(pointer),
    pointerMove: (x, y) => {
      pointer = { x, y };
      return Promise.resolve();
    },
    buttonDown: nothing,
    buttonUp: nothing,
    wheelNotch: nothing,
    levelsOf: () => ({ shift: false, altGr: false, onLayout: true }),
    keyDown: nothing,
    keyUp: nothing,
    flush: nothing,
    close: nothing,
  };
  const surface: PageSurface = {
    metrics: () =>
      Promise.resolve({
        origin: { x: 0, y: 90 },
        viewport: { width: 1200, height: 800 },
        scrollY: 0,
        scrollHeight: 800,
      }),
    visibleWords: () => Promise.resolve(120),
    boxOf: () => Promise.resolve({ x: 500, y: 300, width: 140, height: 40 }),
    painted: () => Promise.resolve(true),
  };
  const human = new Human({
    device,
    surface,
    profile: motorProfile(persona),
    tempo: tempo(persona),
    random: new SeededRandom(persona, `behavior:${seed}`),
    clock,
  });
  const events: FlowEvent[] = [];
  const pauses: Pause[] = [];
  const calls: string[] = [];
  /** When each click of the persona began, on the clock of the rig. */
  const clicks: number[] = [];
  /** What was last done to the page. */
  let acted = '';
  const dwell = human.dwell.bind(human);
  human.dwell = (ms, how) => {
    pauses.push({ step: events.length, ms, wander: how.wander });
    return dwell(ms, how);
  };
  const click = human.click.bind(human);
  human.click = (selector) => {
    acted = selector;
    calls.push(`human.click ${selector}`);
    clicks.push(now);
    return click(selector);
  };
  const chord = human.chord.bind(human);
  human.chord = (modifiers, key) => {
    acted = key;
    calls.push(`human.chord ${[...modifiers, key].join('+')}`);
    return chord(modifiers, key);
  };
  const act = (what: string, target: string) => {
    acted = target;
    calls.push(`${what} ${target}`);
    return Promise.resolve();
  };
  const base = rig().host;
  const host: FlowHost = {
    ...base,
    human: () => Promise.resolve(human),
    goto: (url) => act('goto', url),
    back: async (_timeout, by) => {
      // By the keys when the step is the persona's, else by the driver.
      await (by === undefined ? act('back', 'history') : by());
    },
    mark: (): PageMark => ({ response: null, address: `https://example.org/${acted}` }),
    waitForNavigation: (_since, timeout) => {
      calls.push(`waitForNavigation ${timeout}`);
      const left = leaves.has(acted);
      if (!left) {
        // Nothing came: the whole watch went by.
        now += timeout;
      }
      return Promise.resolve(left);
    },
    driver: {
      ...base.driver,
      click: (selector) => act('driver.click', selector),
      press: (key) => act('driver.press', key),
    },
  };
  const run = (actions: FlowAction[], pace: FlowPace | null, timeout = 100_000_000) =>
    runFlow(
      {
        actions,
        capture,
        timeout_ms: timeout,
        behavior: { seed, ...(pace === null ? {} : { pace }) },
      },
      host,
      (event) => events.push(event),
      () => now,
    );
  const steps = () => events.filter((event) => event.type === 'step');
  return { run, events, steps, pauses, calls, clicks, elapsed: () => now };
}

const sorted = (values: readonly number[]) => [...values].sort((a, b) => a - b);
const quantile = (values: readonly number[], share: number) =>
  sorted(values)[Math.floor(values.length * share)] ?? Number.NaN;
const page = (name: string): FlowAction => ({
  action: 'goto',
  url: `https://example.org/${name}`,
});

describe('a flow that paces itself', () => {
  it('asks for no pause, no draw and no watch of the driver when it has no pace', async () => {
    const { calls, events, outcome } = rig();
    expect(
      await outcome([
        page('a'),
        { action: 'click', selector: '#a' },
        { action: 'click', selector: '#b', human: false },
        { action: 'press', key: 'Enter', human: false },
        { action: 'back' },
        { action: 'back', human: false },
        { action: 'extract', capture },
      ]),
    ).toEqual({ ok: true, dwellMs: 0 });
    expect(calls.filter((call) => /dwell|draw/i.test(call))).toEqual([]);
    // What the driver clicks and presses is not watched: nothing depends on
    // where it led. Going back is the host's to watch, whoever went.
    expect(calls.filter((call) => call.startsWith('waitForNavigation'))).toEqual([
      `waitForNavigation ${NAVIGATION_WATCH_MS}`,
    ]);
    expect(calls.filter((call) => /^back|chord/.test(call))).toEqual([
      'human.chord Alt ArrowLeft',
      `back ${DEFAULT_STEP_TIMEOUT_MS}`,
      `back ${DEFAULT_STEP_TIMEOUT_MS}`,
    ]);
    expect(events).toHaveLength(7);
    expect(events.every((event) => event.type === 'step' && event.paced_ms === null)).toBe(true);
  });

  it('dwells on every page it arrives on, thinks before acting on one it is on, and never delays a read', async () => {
    const { run, steps, pauses, calls } = pacedRig({
      leaves: ['#leave', 'Enter', 'ArrowLeft', '#driver-leave'],
    });
    const outcome = await run(
      [
        page('a'), // 0: arrives
        { action: 'extract', capture }, // 1
        { action: 'click', selector: '#stay' }, // 2: follows a read
        { action: 'click', selector: '#leave' }, // 3: follows an action, and arrives
        { action: 'hover', selector: '#menu' }, // 4: follows a dwell
        { action: 'type', selector: '#q', text: 'used cars' }, // 5
        { action: 'press', key: 'Enter' }, // 6: arrives
        { action: 'wait_for', ms: 0 }, // 7
        { action: 'assert', text_contains: 'page 2' }, // 8
        { action: 'back' }, // 9: follows reads, and arrives
        { action: 'back', human: false }, // 10: arrives
        { action: 'click', selector: '#driver-leave', human: false }, // 11: arrives
        { action: 'click', selector: '#driver-stay', human: false }, // 12
        { action: 'screenshot', options: { full_page: false, format: 'png' } }, // 13
        { action: 'pdf', options: {} }, // 14
        page('b'), // 15: follows reads, and arrives
        { action: 'press', key: 'Tab', human: false }, // 16: follows a dwell
      ],
      { dwell_ms: 4000, min_dwell_ms: 1500 },
    );
    expect(steps().filter((step) => !step.ok)).toEqual([]);
    expect(pauses.map((pause) => `${pause.step}:${pause.wander ? 'dwell' : 'think'}`)).toEqual([
      '0:dwell',
      '2:think',
      '3:think',
      '3:dwell',
      '5:think',
      '6:think',
      '6:dwell',
      '9:think',
      '9:dwell',
      '10:dwell',
      '11:dwell',
      '15:think',
      '15:dwell',
    ]);
    const [shortest, longest] = HUMAN_BOUNDS.thinkingMs;
    for (const pause of pauses) {
      if (pause.wander) {
        expect(pause.ms).toBeGreaterThanOrEqual(1500);
      } else {
        expect(pause.ms).toBeGreaterThanOrEqual(shortest);
        expect(pause.ms).toBeLessThanOrEqual(longest);
      }
    }
    // Every step says how much of its time was pacing; a step that reads says none.
    for (const step of steps()) {
      const planned = pauses
        .filter((pause) => pause.step === step.index)
        .reduce((sum, pause) => sum + pause.ms, 0);
      expect(step.paced_ms, `step ${step.index}`).toBeGreaterThanOrEqual(planned);
      expect(step.paced_ms, `step ${step.index}`).toBeLessThan(planned + 50);
      expect(step.duration_ms).toBeGreaterThanOrEqual(step.paced_ms ?? Number.NaN);
    }
    expect(
      steps()
        .filter((step) =>
          ['extract', 'wait_for', 'assert', 'screenshot', 'pdf'].includes(step.action),
        )
        .map((step) => step.paced_ms),
    ).toEqual([0, 0, 0, 0, 0]);
    const dwelled = pauses
      .filter((pause) => pause.wander)
      .reduce((sum, pause) => sum + pause.ms, 0);
    expect(outcome).toEqual({ ok: true, dwellMs: dwelled });
    // A click and a key are still watched for a navigation, the driver's
    // too now: the dwell depends on where they led.
    expect(
      calls.filter((call) => call === `waitForNavigation ${NAVIGATION_WATCH_MS}`),
    ).toHaveLength(6);
    // Going back is watched by the host, by the keys as by the driver.
    expect(calls.filter((call) => /^back |ArrowLeft/.test(call))).toEqual([
      'human.chord Alt+ArrowLeft',
      'back history',
    ]);
    expect(calls.slice(-2)).toEqual([
      'driver.press Tab',
      `waitForNavigation ${NAVIGATION_WATCH_MS}`,
    ]);
  });

  it('draws every dwell around the median it was given, twice the median at most nineteen times in twenty', async () => {
    const dwells: number[] = [];
    for (const seed of [1, 2, 3, 4].map(seedOf)) {
      const { run, pauses } = pacedRig({ seed });
      const pages = Array.from({ length: 100 }, (_unused, index) => page(String(index)));
      expect((await run(pages, { dwell_ms: 4000 })).ok).toBe(true);
      // One page after the other: nothing but dwells.
      expect(pauses.every((pause) => pause.wander)).toBe(true);
      dwells.push(...pauses.map((pause) => pause.ms));
    }
    expect(dwells).toHaveLength(400);
    expect(Math.abs(quantile(dwells, 0.5) - 4000) / 4000).toBeLessThan(0.08);
    expect(quantile(dwells, 0.95)).toBeGreaterThan(6400);
    expect(quantile(dwells, 0.95)).toBeLessThan(10_000);
    // A distribution, not a number: hardly two dwells are the same.
    expect(new Set(dwells).size).toBeGreaterThan(350);
  });

  it('never dwells below the floor, and piles nothing up at it', async () => {
    const dwellsOf = async (pace: FlowPace) => {
      const all: number[] = [];
      for (const seed of [1, 2, 3, 4].map(seedOf)) {
        const { run, pauses } = pacedRig({ seed });
        await run(
          Array.from({ length: 100 }, (_unused, index) => page(String(index))),
          pace,
        );
        all.push(...pauses.map((pause) => pause.ms));
      }
      return all;
    };
    // Without a floor one dwell in eight is below 2.5 s.
    const free = await dwellsOf({ dwell_ms: 4000 });
    expect(free.filter((ms) => ms < 2500).length).toBeGreaterThan(20);
    const floored = await dwellsOf({ dwell_ms: 4000, min_dwell_ms: 2500 });
    expect(floored).toHaveLength(400);
    expect(Math.min(...floored)).toBeGreaterThanOrEqual(2500);
    // Drawn again, not raised: next to nothing sits exactly on the floor.
    expect(floored.filter((ms) => ms === 2500).length).toBeLessThanOrEqual(1);
    expect(floored.filter((ms) => ms < 2600).length).toBeLessThan(40);
    // A floor no draw reaches is used itself, after the draws were given up.
    expect(new Set(await dwellsOf({ dwell_ms: 100, min_dwell_ms: 40_000 }))).toEqual(
      new Set([40_000]),
    );
  });

  it("dwells the persona's own human dwell when the pace names no median", async () => {
    const hand = motorProfile(SEED);
    const dwells: number[] = [];
    for (const seed of [1, 2, 3, 4].map(seedOf)) {
      const { run, pauses } = pacedRig({ seed });
      await run(
        Array.from({ length: 100 }, (_unused, index) => page(String(index))),
        {},
      );
      dwells.push(...pauses.map((pause) => pause.ms));
    }
    expect(dwells).toHaveLength(400);
    expect(Math.abs(quantile(dwells, 0.5) - hand.dwell.median) / hand.dwell.median).toBeLessThan(
      0.2,
    );
    expect(Math.max(...dwells)).toBeLessThanOrEqual(HUMAN_BOUNDS.dwellCapMs);
    expect(Math.min(...dwells)).toBeGreaterThanOrEqual(600);
  });

  it('replays for the same seeds, and differs for another seed or another persona', async () => {
    const flow: FlowAction[] = [
      page('a'),
      { action: 'click', selector: '#stay' },
      { action: 'click', selector: '#stay' },
      { action: 'click', selector: '#leave' },
      { action: 'extract', capture },
      { action: 'press', key: 'End' },
      page('b'),
    ];
    const paused = async (options: { seed?: string; persona?: string }) => {
      const { run, pauses, steps } = pacedRig({ ...options, leaves: ['#leave'] });
      await run(flow, { dwell_ms: 3000, min_dwell_ms: 800 });
      return { pauses, steps: steps().map((step) => [step.duration_ms, step.paced_ms]) };
    };
    const first = await paused({});
    expect(first.pauses.length).toBeGreaterThanOrEqual(7);
    expect(await paused({})).toEqual(first);
    const otherSeed = await paused({ seed: seedOf(7) });
    const otherPersona = await paused({ persona: seedOf(9) });
    for (const other of [otherSeed, otherPersona]) {
      // The same pauses in the same places, each of another length.
      expect(other.pauses.map((pause) => [pause.step, pause.wander])).toEqual(
        first.pauses.map((pause) => [pause.step, pause.wander]),
      );
      expect(other.pauses.map((pause) => pause.ms)).not.toEqual(
        first.pauses.map((pause) => pause.ms),
      );
    }
  });

  it('never leaves a constant time between two actions: the watch stays, a thinking time of its own follows', async () => {
    const { run, pauses, clicks, calls } = pacedRig();
    const outcome = await run(
      [
        page('a'),
        ...Array.from({ length: 60 }, () => ({ action: 'click', selector: '#stay' }) as const),
      ],
      {},
    );
    expect(outcome.ok).toBe(true);
    const thinking = pauses.filter((pause) => !pause.wander).map((pause) => pause.ms);
    // The first click follows the dwell on the page; every other follows a click.
    expect(thinking).toHaveLength(59);
    const [shortest, longest] = HUMAN_BOUNDS.thinkingMs;
    expect(Math.min(...thinking)).toBeGreaterThanOrEqual(shortest);
    expect(Math.max(...thinking)).toBeLessThanOrEqual(longest);
    expect(new Set(thinking).size).toBeGreaterThan(45);
    const own = tempo(SEED).thinking.median;
    expect(quantile(thinking, 0.5)).toBeGreaterThan(own * 0.7);
    expect(quantile(thinking, 0.5)).toBeLessThan(own * 1.4);
    // Callers rely on the watch for a navigation after a click: it is still asked for.
    expect(
      calls.filter((call) => call === `waitForNavigation ${NAVIGATION_WATCH_MS}`),
    ).toHaveLength(60);
    // From one click to the next: the click, the watch and the thinking time. Hardly two are alike.
    const gaps = clicks.slice(1).map((at, index) => Math.round(at - (clicks[index] ?? 0)));
    expect(gaps).toHaveLength(59);
    expect(Math.min(...gaps)).toBeGreaterThanOrEqual(NAVIGATION_WATCH_MS + shortest);
    expect(new Set(gaps).size).toBeGreaterThan(45);
    expect(Math.max(...gaps) - Math.min(...gaps)).toBeGreaterThan(500);
  });

  it('thinks before the first action that follows a read, and not before one that follows a dwell', async () => {
    const after = async (between: FlowAction[]) => {
      const { run, pauses } = pacedRig();
      await run([page('a'), ...between, { action: 'click', selector: '#stay' }], {});
      return pauses.filter((pause) => !pause.wander).map((pause) => pause.step);
    };
    expect(await after([])).toEqual([]);
    expect(await after([{ action: 'wait_for', selector: '#results' }])).toEqual([2]);
    expect(await after([{ action: 'assert', selector: '#results' }])).toEqual([2]);
    expect(await after([{ action: 'extract', capture }])).toEqual([2]);
    expect(await after([{ action: 'pdf', options: {} }])).toEqual([2]);
    expect(
      await after([{ action: 'screenshot', options: { full_page: false, format: 'png' } }]),
    ).toEqual([2]);
    // A flow that begins on a page somebody else loaded acts on it at once.
    const { run, pauses } = pacedRig();
    await run([{ action: 'click', selector: '#stay' }], {});
    expect(pauses).toEqual([]);
  });

  it('never dwells past the budget of the flow, whatever the median and the floor say', async () => {
    for (const pace of [{ dwell_ms: 60_000 }, { dwell_ms: 60_000, min_dwell_ms: 45_000 }]) {
      const { run, steps, pauses, elapsed } = pacedRig();
      const outcome = await run(
        [page('a'), { action: 'extract', capture }, page('b'), { action: 'back', human: false }],
        pace,
        10_000,
      );
      // The dwell ends where the reserve of the captures begins.
      const room = 10_000 - CAPTURE_RESERVE_MS;
      expect(pauses).toEqual([{ step: 0, ms: room, wander: true }]);
      expect(outcome).toEqual({ ok: true, dwellMs: room });
      expect(steps().map((step) => [step.action, step.ok, step.paced_ms])).toEqual([
        ['goto', true, room],
        ['extract', true, 0],
        // No time is left for a thinking time or a dwell; the steps still run.
        ['goto', true, 0],
        ['back', true, 0],
      ]);
      expect(elapsed()).toBe(room);
    }
  });

  it('fails the step whose pacing fails, and ends the flow there', async () => {
    const events: FlowEvent[] = [];
    const outcome = await runFlow(
      { actions: [page('a'), page('b')], capture, behavior: { pace: {} } },
      { ...rig().host, human: () => Promise.reject(new Error('the display has no input device')) },
      (event) => events.push(event),
    );
    expect(outcome).toEqual({ ok: false, dwellMs: 0 });
    expect(events).toHaveLength(1);
    expect(events).toMatchObject([
      { index: 0, action: 'goto', ok: false, error: 'the display has no input device' },
    ]);
  });
});

describe('the address patterns of wait_for', () => {
  const listing = 'http://fixture.kage.test/listing?page=2';
  it.each<[string, string, boolean]>([
    // Two stars are anything, slashes included.
    ['**/listing?page=2', listing, true],
    ['**/listing?page=2', 'https://example.org/shop/de/listing?page=2', true],
    ['**/listing?page=2', 'http://fixture.kage.test/listing?page=22', false],
    ['**/listing?page=2', 'http://fixture.kage.test/listing', false],
    ['**', listing, true],
    ['**/listing**', listing, true],
    ['http://fixture.kage.test/**', listing, true],
    ['http://fixture.kage.test/**', 'http://elsewhere.kage.test/listing', false],
    // One star stops at a slash.
    ['http://*/listing?page=*', listing, true],
    ['http://fixture.kage.test/*', listing, true],
    ['http://fixture.kage.test/*', 'http://fixture.kage.test/shop/listing', false],
    ['*://fixture.kage.test/*/item', 'https://fixture.kage.test/shop/item', true],
    ['*://fixture.kage.test/*/item', 'https://fixture.kage.test/shop/new/item', false],
    ['*', listing, false],
    // Alternatives, with wildcards and braces of their own.
    ['**/{listing,search}?page=2', listing, true],
    ['**/{search,results}?page=2', listing, false],
    ['http{,s}://fixture.kage.test/listing?page={1,2}', listing, true],
    ['{http,https}://fixture.kage.test/**', listing, true],
    ['**/{a/{b,c},d*}/end', 'https://example.org/a/c/end', true],
    ['**/{a/{b,c},d*}/end', 'https://example.org/dx/end', true],
    ['**/{a/{b,c},d*}/end', 'https://example.org/a/end', false],
    // Everything else is itself: a question mark, a dot, a plus, brackets, a brace without a partner.
    ['**/listing?page=2', 'http://fixture.kage.test/listingXpage=2', false],
    ['**/a.b', 'https://example.org/aXb', false],
    ['**/a.b', 'https://example.org/a.b', true],
    ['**/c++/(x)[1]$^|', 'https://example.org/c++/(x)[1]$^|', true],
    ['**/a{b', 'https://example.org/a{b', true],
    ['**/a}b,c', 'https://example.org/a}b,c', true],
    ['**/a,b', 'https://example.org/a,b', true],
    // The whole address, from its first character to its last.
    ['http://fixture.kage.test/listing', listing, false],
    ['fixture.kage.test/listing?page=2', listing, true],
    [listing, listing, true],
    // A path, on whatever origin.
    ['/listing?page=2', listing, true],
    ['/listing?page=2', 'https://example.org:8443/listing?page=2', true],
    ['/listing?page=2', 'https://example.org/shop/listing?page=2', false],
    ['/**/item', 'https://example.org/shop/new/item', true],
    // No scheme and no slash: from the start, or after any slash.
    ['listing?page=2', listing, true],
    ['listing?page=2', 'https://example.org/shop/listing?page=2', true],
    ['listing?page=2', 'https://example.org/shop/a-listing?page=2', false],
    ['example.org/shop/**', 'https://example.org/shop/listing?page=2', true],
    ['about:blank', 'about:blank', true],
    // An address without a path has its slash.
    ['http://fixture.kage.test', 'http://fixture.kage.test/', true],
    ['http://fixture.kage.test', 'http://fixture.kage.test/listing', false],
    ['http://fixture.kage.test**', listing, true],
    // The fragment counts only for a pattern that names one.
    ['**/listing?page=2', `${listing}#top`, true],
    ['**/listing?page=2#top', `${listing}#top`, true],
    ['**/listing?page=2#top', `${listing}#bottom`, false],
    ['**/listing?page=2#top', listing, false],
    ['**/app#/cart/*', 'https://example.org/app#/cart/7', true],
  ])('%s against %s is %s', (glob, url, expected) => {
    expect(urlMatcher(glob)(url)).toBe(expected);
  });

  it('compiles a pattern once and matches any number of addresses', () => {
    const matches = urlMatcher('**/item/*');
    expect(
      ['https://a.test/item/1', 'https://a.test/item/', 'https://a.test/item/1/2'].map(matches),
    ).toEqual([true, true, false]);
    // A line break is a character like any other: nothing ends early.
    expect(urlMatcher('**')('https://a.test/a\nb')).toBe(true);
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
