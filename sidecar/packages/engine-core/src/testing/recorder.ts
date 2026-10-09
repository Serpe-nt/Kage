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
  readonly events: readonly RecordedEvent[];
}

/** Reads the recorder's report out of a captured document. */
export function inputReportOf(domHtml: string | null): InputReport {
  const json = /<script type="application\/json" id="kage-input-report"[^>]*>([\s\S]*?)<\/script>/.exec(
    domHtml ?? '',
  )?.[1];
  // The document is HTML: the characters a serializer escapes come back as entities.
  const text = (json ?? 'null').replaceAll('&lt;', '<').replaceAll('&gt;', '>').replaceAll('&amp;', '&');
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
  readonly wheel: { readonly count: number; readonly deltas: readonly number[]; readonly modes: readonly number[] };
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
      deltas: [...new Set(wheels.map((event) => Math.abs(event.dy ?? 0)))],
      modes: [...new Set(wheels.map((event) => event.mode ?? 0))],
    },
    scrollEvents: events.filter((event) => event.type === 'scroll').length,
  };
}
