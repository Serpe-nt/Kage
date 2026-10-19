import { pathStatistics, type PathStatistics, type PointerSample } from '../behavior/mouse.js';

/** One event the fixture site's input recorder saw (`tests/fixtures/site/input/`). */
export interface RecordedEvent {
  readonly type: string;
  /** Milliseconds since the page loaded. */
  readonly t: number;
  readonly trusted: boolean;
  readonly target: string | null;
  readonly x?: number;
  readonly y?: number;
  readonly sx?: number;
  readonly sy?: number;
  readonly pointer_type?: string | null;
  readonly button?: number;
  readonly dy?: number;
  readonly mode?: number;
  readonly top?: number;
  readonly key?: string;
  readonly code?: string;
  readonly value?: string | null;
}

/** What the recorder publishes. */
export interface InputReport {
  readonly kage_input: 1;
  readonly window: {
    readonly inner: readonly [number, number];
    readonly outer: readonly [number, number];
    readonly scroll_top: number;
    readonly scroll_height: number;
  };
  readonly field: string;
  readonly choice: string;
  /**
   * Milliseconds since the recorder started at which the page drew its
   * first two frames; absent from a recorder older than the field.
   */
  readonly drawn?: readonly number[];
  readonly events: readonly RecordedEvent[];
}

/** Reads the recorder's report out of a captured document. */
export function inputReportOf(domHtml: string | null): InputReport {
  const json =
    /<script type="application\/json" id="kage-input-report"[^>]*>([\s\S]*?)<\/script>/.exec(
      domHtml ?? '',
    )?.[1];
  // The document is HTML: the characters a serializer escapes come back as entities.
  const text = (json ?? 'null')
    .replaceAll('&lt;', '<')
    .replaceAll('&gt;', '>')
    .replaceAll('&amp;', '&');
  const report = JSON.parse(text) as InputReport | null;
  if (report?.kage_input !== 1) {
    throw new Error('the input recorder published no report');
  }
  return report;
}

const median = (values: readonly number[]): number => {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length === 0
    ? Number.NaN
    : sorted.length % 2 === 1
      ? (sorted[middle] ?? Number.NaN)
      : ((sorted[middle - 1] ?? 0) + (sorted[middle] ?? 0)) / 2;
};

/** What a page could compute about the person at the keyboard and mouse. */
export interface InputStatistics {
  /** Events a script did not come from a device. */
  readonly untrusted: number;
  /** Every `pointerType` pointer events carried. */
  readonly pointerTypes: readonly string[];
  /** Movements of the mouse longer than 100 pixels, each measured. */
  readonly moves: readonly PathStatistics[];
  /** Milliseconds between consecutive key presses, modifiers and corrections left out. */
  readonly keyIntervals: readonly number[];
  readonly medianKeyInterval: number;
  /** Milliseconds each key stayed down. */
  readonly keyHolds: readonly number[];
  /** Milliseconds the mouse button stayed down per click. */
  readonly buttonHolds: readonly number[];
  /** Milliseconds between the last movement and the button going down, per click. */
  readonly hovers: readonly number[];
  readonly wheel: {
    readonly count: number;
    /** Sum of the distances of every wheel event, whatever its direction. */
    readonly total: number;
    readonly modes: readonly number[];
  };
  /** How many scroll events followed each notch of the wheel: more than one means the page glided. */
  readonly scrollEvents: number;
}

const MODIFIERS = new Set(['Shift', 'AltGraph', 'Control', 'Alt', 'Meta']);

/** Measures what the input recorder saw. */
export function inputStatistics(report: InputReport): InputStatistics {
  const { events } = report;
  const moves: PathStatistics[] = [];
  let current: PointerSample[] = [];
  const flush = () => {
    const measured = pathStatistics(current);
    if (measured !== null && measured.chord > 100) {
      moves.push(measured);
    }
    current = [];
  };
  for (const event of events) {
    if (event.type !== 'mousemove' || event.x === undefined || event.y === undefined) {
      continue;
    }
    const previous = current.at(-1);
    if (previous !== undefined && event.t - previous.t > 150) {
      flush();
    }
    current.push({ t: event.t, x: event.x, y: event.y });
  }
  flush();

  const presses = events.filter(
    (event) => event.type === 'keydown' && event.key !== undefined && !MODIFIERS.has(event.key),
  );
  const keyIntervals: number[] = [];
  for (let index = 1; index < presses.length; index += 1) {
    const now = presses[index];
    const before = presses[index - 1];
    if (now !== undefined && before !== undefined && now.key !== 'Backspace') {
      keyIntervals.push(now.t - before.t);
    }
  }
  const keyHolds: number[] = [];
  const down = new Map<string, number>();
  for (const event of events) {
    if (event.code === undefined || event.key === undefined || MODIFIERS.has(event.key)) {
      continue;
    }
    if (event.type === 'keydown') {
      down.set(event.code, event.t);
    } else if (event.type === 'keyup') {
      const since = down.get(event.code);
      if (since !== undefined) {
        keyHolds.push(event.t - since);
        down.delete(event.code);
      }
    }
  }
  const buttonHolds: number[] = [];
  const hovers: number[] = [];
  let pressed: number | null = null;
  let lastMove: number | null = null;
  for (const event of events) {
    if (event.type === 'mousemove') {
      lastMove = event.t;
    } else if (event.type === 'mousedown') {
      pressed = event.t;
      if (lastMove !== null) {
        hovers.push(event.t - lastMove);
      }
    } else if (event.type === 'mouseup' && pressed !== null) {
      buttonHolds.push(event.t - pressed);
      pressed = null;
    }
  }
  const wheels = events.filter((event) => event.type === 'wheel');
  return {
    untrusted: events.filter((event) => !event.trusted).length,
    pointerTypes: [
      ...new Set(
        events
          .filter((event) => event.type.startsWith('pointer'))
          .map((event) => event.pointer_type ?? ''),
      ),
    ],
    moves,
    keyIntervals,
    medianKeyInterval: median(keyIntervals),
    keyHolds,
    buttonHolds,
    hovers,
    wheel: {
      count: wheels.length,
      total: wheels.reduce((sum, event) => sum + Math.abs(event.dy ?? 0), 0),
      modes: [...new Set(wheels.map((event) => event.mode ?? 0))],
    },
    scrollEvents: events.filter((event) => event.type === 'scroll').length,
  };
}

/** One reading of the fixture site's activation recorder (`tests/fixtures/site/activation/`). */
export interface ActivationSample {
  /** `start`, `load`, `load+300ms`, `load+4000ms`, or `now`. */
  readonly at: string;
  /** Milliseconds since the page started. */
  readonly ms: number;
  readonly is_active: boolean | null;
  readonly has_been_active: boolean | null;
  /** State of an `AudioContext` made for this sample: Chrome decides here whether it may run. */
  readonly audio_new: string | null;
  /** State of the one made for the sample before: Firefox lets it run a moment after it was made. */
  readonly audio_earlier: string | null;
  readonly trusted_input_events: number;
  /** Of them, the ones a browser grants activation for. */
  readonly activating_input_events: number;
}

/** What the activation recorder publishes. */
export interface ActivationReport {
  readonly kage_activation: 1;
  /** The fixed samples, in order. */
  readonly samples: readonly ActivationSample[];
  /** The page as it is, renewed four times a second once the fixed samples are complete. */
  readonly now: ActivationSample | null;
  /** What `navigator.getAutoplayPolicy()` said when the page started; `null` in a browser without the call. */
  readonly autoplay_policy: { readonly mediaelement: string; readonly audiocontext: string } | null;
  readonly untrusted_input_events: number;
  readonly input_types: readonly string[];
}

/** Reads the activation recorder's report out of a captured document. */
export function activationReportOf(domHtml: string | null): ActivationReport {
  const json =
    /<script type="application\/json" id="kage-activation-report"[^>]*>([\s\S]*?)<\/script>/.exec(
      domHtml ?? '',
    )?.[1];
  const text = (json ?? 'null')
    .replaceAll('&lt;', '<')
    .replaceAll('&gt;', '>')
    .replaceAll('&amp;', '&');
  const report = JSON.parse(text) as ActivationReport | null;
  if (report?.kage_activation !== 1) {
    throw new Error('the activation recorder published no report');
  }
  return report;
}

/** One thing the fixture site's leave recorder noted (`/leave/` in `tests/fixtures/site/server.mjs`). */
export interface LeaveEvent {
  /** The event's type, or `untrusted-mousemove` for a movement no device made. */
  readonly type: string;
  /** Milliseconds since the page started. */
  readonly t: number;
  /** Of a button or a key: whether a device made it. */
  readonly trusted?: boolean;
  /** Of a button or a key: the id of the element it met. */
  readonly target?: string | null;
  readonly key?: string | null;
  /** Of `pagehide`: whether the browser keeps the page. */
  readonly persisted?: boolean;
}

/**
 * What a page of the leave recorder reports when it goes away: what its
 * visitor did on it, as far as a page can see (follow-up F28).
 */
export interface LeaveReport {
  readonly page: 'leave';
  /** The `run` of the page's query: one load of the page. */
  readonly run: string;
  /** Buttons, keys, focus and what a page is told when it is left, in order. */
  readonly events: readonly LeaveEvent[];
  /** Movements of the mouse the page saw. */
  readonly moves: number;
  readonly firstMove: number | null;
  readonly lastMove: number | null;
  /** Wheel events the page saw. */
  readonly wheels: number;
  readonly scrollY: number;
  /** `navigator.userActivation.hasBeenActive` when the page went away. */
  readonly activation: boolean | null;
}

/** The buttons a page of the leave recorder saw go down and up and click, in order: type, trusted, target. */
export function pressesOf(report: LeaveReport): [string, boolean | undefined, string | null][] {
  return report.events
    .filter((event) => ['mousedown', 'mouseup', 'click'].includes(event.type))
    .map((event) => [event.type, event.trusted, event.target ?? null]);
}

/** Whether a sample shows anything a browser only gives a page its user has touched. */
export function touched(sample: ActivationSample): boolean {
  return (
    sample.is_active !== false ||
    sample.has_been_active !== false ||
    sample.audio_new === 'running' ||
    sample.audio_earlier === 'running'
  );
}
