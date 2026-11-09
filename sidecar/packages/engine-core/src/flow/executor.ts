import type {
  CaptureOptions,
  FlowAction,
  FlowEvent,
  FlowPace,
  FlowRequest,
  PageResult,
  PdfOptions,
  ScreenshotOptions,
} from '@kage/contract';

import { childGrant, withInputGrant, type Human } from '../behavior/index.js';
import type { EngineLog } from '../engine.js';

/** Default budget of one step (spec §10.5). */
export const DEFAULT_STEP_TIMEOUT_MS = 30_000;
/** Default budget of a whole flow. */
export const DEFAULT_FLOW_TIMEOUT_MS = 180_000;
/** Budget kept back for the captures when behavior fills the time: of a navigation, and of a flow. */
export const CAPTURE_RESERVE_MS = 4000;
/**
 * What a human scroll keeps back of its step's time. A page too long for
 * the step is scrolled as far as the time allows; the flick that began just
 * before the scrolling's deadline is finished after it, and the step must
 * still end inside its own budget instead of being failed by it.
 */
export const SCROLL_RESERVE_MS = 2000;
/** How long a click or a key is watched for a navigation it may have started. */
export const NAVIGATION_WATCH_MS = 1200;

/**
 * Which document a page showed at one moment. Taken before an action, it
 * tells afterwards whether the action left the page, however quickly the
 * next one arrived: a look that is only taken after the action has missed
 * a page that was already there.
 */
export interface PageMark {
  /** The response the document came with: the host's own object, compared by identity. */
  readonly response: unknown;
  /** The address of the page. */
  readonly address: string;
}

/**
 * What the flow executor needs of an open context. The session of
 * engine-core implements it once for both engines.
 */
export interface FlowHost {
  /** The persona at its mouse and keyboard; created at the first human step. */
  human(): Promise<Human>;
  /** Where a step that would not stop in time is written down. */
  readonly log: EngineLog;
  /** Whether typed text gets the occasional typo. */
  readonly typos: boolean;
  goto(url: string, timeoutMs: number): Promise<void>;
  /**
   * Goes back one history entry and waits for the page before: by the
   * driver, or by `by` when it is given (the persona's keys). Either way
   * reports the same thing, because the session looks at the page itself.
   *
   * @param by what asks the browser to go back in place of the driver.
   * @throws when there is no entry to go back to, or the page before did not come in time.
   */
  back(timeoutMs: number, by?: () => Promise<void>): Promise<void>;
  waitForSelector(selector: string, timeoutMs: number): Promise<void>;
  /**
   * Waits until the address matches a pattern (see `url-glob.ts`) and its document has loaded.
   *
   * @throws when it did not within the time; a {@link FlowStepError} is reported as it is.
   */
  waitForUrl(glob: string, timeoutMs: number): Promise<void>;
  waitForNetworkIdle(timeoutMs: number): Promise<void>;
  /** Notes which document the page shows, for {@link FlowHost.waitForNavigation}. */
  mark(): PageMark;
  /**
   * Waits for a navigation an action may have started since the mark was
   * taken, and for the page it led to; returns when none started within the
   * time.
   *
   * @returns whether the page was left: a new document arrived, or the address changed.
   */
  waitForNavigation(since: PageMark, timeoutMs: number): Promise<boolean>;
  exists(selector: string): Promise<boolean>;
  /** The visible text of the page. */
  text(): Promise<string>;
  /** The selected and the wanted option of a select element; `null` when the selector matches none. */
  selectState(selector: string, value: string): Promise<{ index: number; wanted: number } | null>;
  /** The same actions done by the driver: fast, and visible to a page as not a person's. */
  readonly driver: {
    click(selector: string, timeoutMs: number): Promise<void>;
    hover(selector: string, timeoutMs: number): Promise<void>;
    fill(selector: string, text: string, timeoutMs: number): Promise<void>;
    press(key: string): Promise<void>;
    select(selector: string, value: string, timeoutMs: number): Promise<void>;
    scroll(depth: number): Promise<void>;
  };
  extract(capture: CaptureOptions): Promise<PageResult>;
  screenshot(options: ScreenshotOptions): Promise<PageResult>;
  pdf(options: PdfOptions): Promise<PageResult>;
  /**
   * Keeps the flow's page the one the persona's hand and keys reach (F184):
   * a tab the page opened, which a browser shows in front of it, is put
   * behind it again.
   *
   * @throws FlowStepError when a tab the page opened stays in front of it.
   */
  keepFront(): Promise<void>;
  /** The addresses of the tabs the flow's page opened since the last time this was asked (F184). */
  openedTabs(): string[];
}

/** A step failed; the message is what the step event reports. */
export class FlowStepError extends Error {
  override readonly name = 'FlowStepError';
}

/** How a flow ended. */
export interface FlowOutcome {
  /** Whether every step succeeded. */
  readonly ok: boolean;
  /** Whether it was stopped before a step because its caller went away (F175). */
  readonly stopped?: boolean;
  /** Milliseconds the persona dwelled on the pages a paced flow arrived on, together; 0 for a flow that is not paced. */
  readonly dwellMs: number;
}

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, Math.max(0, ms)));

/**
 * How long a step that outlived its budget is given to stop once told
 * to: typing stops at its next key and lets every key up (QA round
 * three, the timed-out `type`).
 */
const STOP_GRACE_MS = 1000;

/**
 * Ends work that outlives its budget. The grace lets a step report its
 * own, better worded, timeout first. Once the budget is over the work's
 * signal is aborted, and the work is waited for until it stopped (for
 * `STOP_GRACE_MS` at most).
 *
 * The work runs with an input grant of its own (QA round four, input-01),
 * revoked when the step ends: at once when it finished, after the grace
 * when it ran out of time. From then on nothing it does reaches the
 * display, whatever it is doing: a press, a move, a button or a wheel
 * notch of it throws before it acts, and what it still held down is let
 * up. So a step whose budget ran out does nothing more once the flow has
 * moved on, in the flow's page or in the next request's tab (a `select`
 * pressed its arrows on into another key's page and submitted its form).
 */
async function withTimeout<T>(
  work: (signal: AbortSignal) => Promise<T>,
  ms: number,
  graceMs: number,
  what: string,
  log: EngineLog,
): Promise<T> {
  const stop = new AbortController();
  const grant = childGrant();
  const running = withInputGrant(grant, () => work(stop.signal));
  let timer: NodeJS.Timeout | undefined;
  const late = new Promise<'late'>((resolve) => {
    timer = setTimeout(() => {
      resolve('late');
    }, ms + graceMs);
  });
  try {
    try {
      const first = await Promise.race([running.then((value) => ({ value })), late]);
      if (first !== 'late') {
        return first.value;
      }
    } finally {
      clearTimeout(timer);
    }
    stop.abort();
    const stopped = await Promise.race([
      running.then(
        () => true,
        () => true,
      ),
      sleep(STOP_GRACE_MS).then(() => false),
    ]);
    if (!stopped) {
      // Work that does not watch the signal at every act (a hand still
      // reaching its element). Its grant is revoked below: whatever it
      // does from now on reaches no page.
      log.warn(
        { step: what, budget_ms: ms, grace_ms: STOP_GRACE_MS },
        'a step whose budget ran out had not stopped after the grace; its input is revoked and the flow goes on without it',
      );
    }
    throw new FlowStepError(`${what} did not finish within ${ms} ms`);
  } finally {
    await grant.revoke();
  }
}

/** Waits until a selector matches, looking again every little while. */
async function waitUntilExists(host: FlowHost, selector: string, timeoutMs: number): Promise<void> {
  try {
    await host.waitForSelector(selector, timeoutMs);
  } catch {
    throw new FlowStepError(`no element matched ${selector} within ${timeoutMs} ms`);
  }
}

/**
 * Chooses an option of a select element the way a person without a mouse
 * on the list does: a click to open it, the arrow keys to the option,
 * `Enter`. The browser fires its own `input` and `change` events. It stops
 * before the next key once `signal` is aborted, the `Enter` included (QA
 * round four, input-01): a long list is many keys.
 */
async function selectByKeyboard(
  host: FlowHost,
  human: Human,
  selector: string,
  value: string,
  signal: AbortSignal,
): Promise<void> {
  const stopIfOver = () => {
    if (signal.aborted) {
      throw new FlowStepError('the budget of the step ran out before the option was reached');
    }
  };
  const state = await host.selectState(selector, value);
  if (state === null) {
    throw new FlowStepError(`${selector} is not a select element`);
  }
  if (state.wanted < 0) {
    throw new FlowStepError(`${selector} has no option with the value ${JSON.stringify(value)}`);
  }
  stopIfOver();
  await human.click(selector);
  // The click may have opened a tab: the keys are for the flow's page (F184).
  await host.keepFront();
  const steps = state.wanted - state.index;
  for (let step = 0; step < Math.abs(steps); step += 1) {
    stopIfOver();
    await human.press(steps > 0 ? 'ArrowDown' : 'ArrowUp');
  }
  stopIfOver();
  await human.press('Enter');
}

/** What a step did besides succeeding. */
interface Performed {
  readonly capture: PageResult | null;
  /** Whether the step arrived on a page: it loaded one, or a navigation it started was seen. */
  readonly arrived: boolean;
}

const stayed: Performed = { capture: null, arrived: false };
const arrived: Performed = { capture: null, arrived: true };

/**
 * Does something that may leave the page, and waits for the page it led to.
 * The look at the page comes before the action: the next page may be there
 * before the action has returned.
 */
async function leaving(
  host: FlowHost,
  watchMs: number,
  act: () => Promise<void>,
): Promise<Performed> {
  const before = host.mark();
  await act();
  return { capture: null, arrived: await host.waitForNavigation(before, watchMs) };
}

/**
 * Does one step.
 *
 * @param paced whether the flow paces itself: then a click or a key of the driver is watched for a navigation like a person's, because the dwell on the next page depends on it.
 * @param signal aborted once the step's budget is over: typing stops at its next key.
 */
async function perform(
  action: FlowAction,
  host: FlowHost,
  timeout: number,
  paced: boolean,
  signal: AbortSignal,
): Promise<Performed> {
  const humanly = action.human ?? true;
  // What is left of the step's time: a driver action after the wait for its
  // element gets no more than that, so it ends with the step's budget and
  // never acts on the flow's page after the flow returned (review of #96).
  const until = Date.now() + timeout;
  const left = () => Math.max(1, until - Date.now());
  switch (action.action) {
    case 'goto':
      await host.goto(action.url, timeout);
      return arrived;
    case 'click': {
      await waitUntilExists(host, action.selector, timeout);
      if (humanly) {
        const human = await host.human();
        // A click may leave the page; the next step must not run into the old one.
        return leaving(host, NAVIGATION_WATCH_MS, () => human.click(action.selector));
      }
      if (paced) {
        return leaving(host, NAVIGATION_WATCH_MS, () => host.driver.click(action.selector, left()));
      }
      await host.driver.click(action.selector, left());
      return stayed;
    }
    case 'hover':
      await waitUntilExists(host, action.selector, timeout);
      if (humanly) {
        await (await host.human()).hover(action.selector);
      } else {
        await host.driver.hover(action.selector, left());
      }
      return stayed;
    case 'type':
      await waitUntilExists(host, action.selector, timeout);
      if (humanly) {
        const human = await host.human();
        await human.click(action.selector);
        // The click may have opened a tab, which the browser shows in
        // front: the keys are for the flow's page (F184).
        await host.keepFront();
        // The budget may have run out while the hand reached the field:
        // then nothing is typed, and the keymap is left alone.
        if (signal.aborted) {
          throw new FlowStepError('the budget of the step ran out before typing began');
        }
        await human.type(action.text, { typos: host.typos, signal });
      } else {
        await host.driver.fill(action.selector, action.text, left());
      }
      return stayed;
    case 'press': {
      if (humanly) {
        const human = await host.human();
        return leaving(host, NAVIGATION_WATCH_MS, () => human.press(action.key));
      }
      if (paced) {
        return leaving(host, NAVIGATION_WATCH_MS, () => host.driver.press(action.key));
      }
      await host.driver.press(action.key);
      return stayed;
    }
    case 'scroll':
      if (humanly) {
        await (
          await host.human()
        ).scrollTo(action.depth, performance.now() + Math.max(0, timeout - SCROLL_RESERVE_MS));
      } else {
        await host.driver.scroll(action.depth);
      }
      return stayed;
    case 'select':
      await waitUntilExists(host, action.selector, timeout);
      if (humanly) {
        await selectByKeyboard(host, await host.human(), action.selector, action.value, signal);
      } else {
        await host.driver.select(action.selector, action.value, left());
      }
      return stayed;
    case 'back': {
      if (humanly) {
        // The keyboard's way back, as the browser itself offers it. The
        // host watches what came of it as it does for the driver's (F38):
        // keys that led nowhere are a step that failed, not one that
        // stayed where it was.
        const human = await host.human();
        await host.back(timeout, () => human.chord(['Alt'], 'ArrowLeft'));
      } else {
        await host.back(timeout);
      }
      return arrived;
    }
    case 'wait_for': {
      const conditions = [
        action.selector != null,
        action.url != null,
        action.network_idle === true,
        action.ms != null,
      ].filter(Boolean).length;
      if (conditions !== 1) {
        throw new FlowStepError('wait_for takes exactly one of selector, url, network_idle and ms');
      }
      if (action.selector != null) {
        await waitUntilExists(host, action.selector, timeout);
      } else if (action.url != null) {
        await host.waitForUrl(action.url, timeout).catch((thrown: unknown) => {
          throw thrown instanceof FlowStepError
            ? thrown
            : new FlowStepError(`the URL did not match ${action.url ?? ''} within ${timeout} ms`);
        });
      } else if (action.network_idle === true) {
        await host.waitForNetworkIdle(timeout).catch(() => {
          throw new FlowStepError(`the network was not idle within ${timeout} ms`);
        });
      } else {
        // A wait for a given time is done only when all of it was waited
        // (jobs-11): one the step's time, or what is left of the flow's
        // budget, cannot hold fails at once and says so, as a wait for a
        // selector or an address that does not come in time fails.
        const wanted = action.ms ?? 0;
        if (wanted > timeout) {
          throw new FlowStepError(
            `a wait of ${wanted} ms does not fit in the ${timeout} ms the step has`,
          );
        }
        await sleep(wanted);
      }
      return stayed;
    }
    case 'extract':
      return { capture: await host.extract(action.capture), arrived: false };
    case 'screenshot':
      return { capture: await host.screenshot(action.options), arrived: false };
    case 'pdf':
      return { capture: await host.pdf(action.options), arrived: false };
    case 'assert': {
      const conditions = [action.selector != null, action.text_contains != null].filter(
        Boolean,
      ).length;
      if (conditions !== 1) {
        throw new FlowStepError('assert takes exactly one of selector and text_contains');
      }
      if (action.selector != null) {
        if (!(await host.exists(action.selector))) {
          throw new FlowStepError(`no element matches ${action.selector}`);
        }
      } else if (!(await host.text()).includes(action.text_contains ?? '')) {
        throw new FlowStepError(
          `the page text does not contain ${JSON.stringify(action.text_contains)}`,
        );
      }
      return stayed;
    }
  }
}

/** The steps that only read a page or wait for it: a paced flow never delays them. */
const READING: ReadonlySet<FlowAction['action']> = new Set([
  'wait_for',
  'assert',
  'extract',
  'screenshot',
  'pdf',
]);

/**
 * The pace of a flow that asked for one (spec §10.5): a dwell on every page
 * it arrives on, a thinking time before an action on a page the persona is
 * already on. Every time comes from the stream of the flow's own persona
 * and seed, so the same flow replays.
 */
class Pacer {
  /** What the dwells came to. */
  dwelled = 0;
  /**
   * Whether the persona has just rested: at the start of a flow, and right
   * after a dwell. Whatever acts next then needs no thinking time.
   */
  private rested = true;

  /**
   * @param deadline when the budget of the flow ends, on the clock of `now`.
   */
  constructor(
    private readonly pace: FlowPace,
    private readonly host: FlowHost,
    private readonly now: () => number,
    private readonly deadline: number,
  ) {}

  /** Milliseconds a pause may still take: what is left of the flow's budget, less the reserve of the captures. */
  private room(): number {
    return Math.max(0, this.deadline - this.now() - CAPTURE_RESERVE_MS);
  }

  /** Rests, and says how long that took. */
  private async rest(human: Human, ms: number, wander: boolean): Promise<number> {
    const began = this.now();
    await human.dwell(ms, { wander });
    return this.now() - began;
  }

  /**
   * Before a step: the time a person takes before the next action. A step
   * that only reads waits for nobody.
   *
   * @returns the milliseconds it took; 0 when there was nothing to wait for.
   */
  async before(action: FlowAction): Promise<number> {
    if (this.rested || READING.has(action.action)) {
      return 0;
    }
    const human = await this.host.human();
    const pause = Math.min(human.drawThinking(), this.room());
    // A hand that is about to act does not wander off.
    return pause > 0 ? this.rest(human, pause, false) : 0;
  }

  /**
   * After a step that succeeded: the dwell on a page it arrived on.
   *
   * @returns the milliseconds it took; 0 for a step that arrived nowhere.
   */
  async after(performed: Performed): Promise<number> {
    this.rested = false;
    if (!performed.arrived) {
      return 0;
    }
    const human = await this.host.human();
    const wanted = human.drawPacedDwell({
      median: this.pace.dwell_ms ?? null,
      floor: this.pace.min_dwell_ms ?? 0,
    });
    // Never past the flow's budget, whatever the floor says.
    const dwell = Math.min(wanted, this.room());
    if (dwell <= 0) {
      return 0;
    }
    // The idle hand wanders unless the flow asked it to rest (contract 1.5.0).
    const took = await this.rest(human, dwell, this.pace.wander ?? true);
    this.dwelled += dwell;
    this.rested = true;
    return took;
  }
}

/**
 * Runs the steps of a flow in order (spec §10.5) and reports each as it
 * finishes. The first step that fails ends the flow; so does the flow's
 * own budget, and so does `signal` once it is aborted (its caller went
 * away, F175): the step under way is finished, and no other begins.
 *
 * A flow with `behavior.pace` paces itself: its pauses belong to the step
 * they come before or after, count in that step's `duration_ms` and are
 * reported as its `paced_ms`. They are bounded by the flow's budget, not by
 * the step's, which is for the step's own work. Without `pace` nothing is
 * drawn and nothing waits.
 */
export async function runFlow(
  request: FlowRequest,
  host: FlowHost,
  emit: (event: FlowEvent) => void,
  now: () => number = Date.now,
  signal?: AbortSignal,
): Promise<FlowOutcome> {
  const deadline = now() + (request.timeout_ms ?? DEFAULT_FLOW_TIMEOUT_MS);
  const pace = request.behavior?.pace ?? null;
  const pacer = pace === null ? null : new Pacer(pace, host, now, deadline);
  for (const [index, action] of request.actions.entries()) {
    if (signal?.aborted === true) {
      // Nobody waits for the flow any more: nothing more is asked of any
      // site for it (F175).
      return { ok: false, dwellMs: pacer?.dwelled ?? 0, stopped: true };
    }
    const started = now();
    let left = deadline - started;
    let paced = 0;
    let capture: PageResult | null = null;
    let error: string | null = null;
    if (left <= 0) {
      error = 'the budget of the flow ran out before this step';
    } else {
      try {
        if (pacer !== null) {
          const thought = await pacer.before(action);
          paced += thought;
          left -= thought;
        }
        // Every step starts on the flow's page, never on a tab it opened (F184).
        if (!READING.has(action.action)) {
          await host.keepFront();
        }
        const timeout = Math.min(action.timeout_ms ?? DEFAULT_STEP_TIMEOUT_MS, Math.max(0, left));
        // Human steps take the time a person takes; the step's budget still ends them.
        const performed = await withTimeout(
          (stopping) => perform(action, host, timeout, pacer !== null, stopping),
          timeout,
          250,
          action.action,
          host.log,
        );
        capture = performed.capture;
        // A tab the step opened goes behind the flow's page before the
        // persona dwells there (F184).
        await host.keepFront();
        if (pacer !== null) {
          paced += await pacer.after(performed);
        }
      } catch (thrown) {
        error = (thrown instanceof Error ? thrown.message : String(thrown)).split('\n')[0] ?? '';
      }
    }
    emit({
      type: 'step',
      index,
      action: action.action,
      ok: error === null,
      duration_ms: Math.max(0, Math.round(now() - started)),
      paced_ms: pacer === null ? null : Math.max(0, Math.round(paced)),
      error,
      capture,
      opened_tabs: host.openedTabs(),
    });
    if (error !== null) {
      return { ok: false, dwellMs: pacer?.dwelled ?? 0 };
    }
  }
  return { ok: true, dwellMs: pacer?.dwelled ?? 0 };
}
