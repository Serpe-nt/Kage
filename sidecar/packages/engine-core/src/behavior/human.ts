import type { SeededRandom } from '../seeded-random.js';

import {
  systemClock,
  type Box,
  type Clock,
  type InputDevice,
  type PageSurface,
  type Point,
  type SurfaceMetrics,
} from './device.js';
import { planClickPoint, planMove, planWander, type PointerPath } from './mouse.js';
import { DEFAULT_TEMPO, drawPacedDwell, drawThinking, type DwellPace, type Tempo } from './pace.js';
import type { MotorProfile } from './profile.js';
import { ASSUMED_NOTCH_PX, drawDwell, planFlick, readingPause, scrollsBack } from './scroll.js';
import { planTyping } from './typing.js';

/** The behavior engine could not do what was asked of it on this page. */
export class BehaviorError extends Error {
  override readonly name = 'BehaviorError';

  constructor(
    readonly reason: 'element_not_found' | 'element_not_reachable',
    message: string,
  ) {
    super(message);
  }
}

/** What a stretch of behavior consisted of; returned with the result of the request. */
export interface BehaviorCounts {
  /** Movements of the pointer, corrections after an overshoot included in one. */
  moves: number;
  /** Positions the mouse reported. */
  positions: number;
  overshoots: number;
  clicks: number;
  /** Notches of the wheel. */
  notches: number;
  /** Keys pressed, modifiers and corrections included. */
  keys: number;
  typos: number;
  /** Idle movements while dwelling. */
  wanders: number;
}

/** What a {@link Human} is made of. */
export interface HumanParts {
  readonly device: InputDevice;
  readonly surface: PageSurface;
  /** The persona's hand. */
  readonly profile: MotorProfile;
  /** How quickly the persona goes from one action to the next; the middle of the population when absent. */
  readonly tempo?: Tempo;
  /** The stream of this request. */
  readonly random: SeededRandom;
  readonly clock?: Clock;
}

/**
 * Why a scroll to a depth ended. `depth`: the depth was reached. `deadline`:
 * the time it was given was used up first (or, on a page without end, the
 * bound on its flicks). `page`: the page does not scroll, or not any
 * further.
 */
export type ScrollOutcome = 'depth' | 'deadline' | 'page';

/** How long the browser's smooth scrolling of one flick is given before the page is measured. */
const SCROLL_SETTLE_MS = 260;
/** Most flicks one scroll may take: a bound, not a behavior. */
const MAX_FLICKS = 400;

/**
 * One persona at the mouse and keyboard of its own display for the length
 * of one request (spec §10.4).
 *
 * It looks at the page through the engine (where is this element, how far
 * is the page scrolled, how much text is in view) and acts only through
 * the display's input device. Nothing here knows which browser runs.
 */
export class Human {
  readonly counts: BehaviorCounts = {
    moves: 0,
    positions: 0,
    overshoots: 0,
    clicks: 0,
    notches: 0,
    keys: 0,
    typos: 0,
    wanders: 0,
  };
  private pointer: Point | null = null;
  /** Whether this persona has moved the mouse yet. */
  private touched = false;
  private notchPx = ASSUMED_NOTCH_PX;
  private readonly clock: Clock;

  constructor(private readonly parts: HumanParts) {
    this.clock = parts.clock ?? systemClock;
  }

  private async position(): Promise<Point> {
    this.pointer ??= await this.parts.device.pointerPosition();
    return this.pointer;
  }

  private async play(path: PointerPath): Promise<void> {
    const started = this.clock.now();
    for (const sample of path.samples) {
      const wait = started + sample.t - this.clock.now();
      if (wait > 0) {
        await this.clock.sleep(wait);
      }
      await this.parts.device.pointerMove(sample.x, sample.y);
    }
    this.pointer = path.to;
    this.touched = true;
    this.counts.positions += path.samples.length;
    if (path.overshoot) {
      this.counts.overshoots += 1;
    }
  }

  /** Moves the pointer to a point of the screen, aiming at something of the given size. */
  private async moveToScreen(to: Point, targetSize: number): Promise<void> {
    const path = planMove(
      await this.position(),
      to,
      targetSize,
      this.parts.profile,
      this.parts.random,
    );
    if (path.samples.length > 0) {
      this.counts.moves += 1;
      await this.play(path);
    }
  }

  private static inView(box: Box, metrics: SurfaceMetrics): boolean {
    const middle = box.y + box.height / 2;
    return middle >= 12 && middle <= metrics.viewport.height - 12;
  }

  /**
   * Puts the pointer over the page: a wheel scrolls what it is over. A
   * mouse that has not moved yet is moved first even when it rests over the
   * page: a browser does not scroll for a pointer it has never seen move,
   * and neither does a person scroll without touching the mouse.
   */
  private async overPage(metrics: SurfaceMetrics): Promise<void> {
    const pointer = await this.position();
    const { origin, viewport } = metrics;
    const inside =
      pointer.x >= origin.x &&
      pointer.x < origin.x + viewport.width &&
      pointer.y >= origin.y &&
      pointer.y < origin.y + viewport.height;
    if (!inside || !this.touched) {
      const { random } = this.parts;
      await this.moveToScreen(
        {
          x: origin.x + viewport.width * random.uniform(0.3, 0.7),
          y: origin.y + viewport.height * random.uniform(0.3, 0.7),
        },
        200,
      );
    }
  }

  private async flick(direction: 'up' | 'down', remaining: number): Promise<number> {
    const flick = planFlick(
      direction,
      remaining,
      this.notchPx,
      this.parts.profile,
      this.parts.random,
    );
    for (const gap of flick.gaps) {
      if (gap > 0) {
        await this.clock.sleep(gap);
      }
      await this.parts.device.wheelNotch(direction);
    }
    this.counts.notches += flick.gaps.length;
    await this.clock.sleep(SCROLL_SETTLE_MS);
    return flick.gaps.length;
  }

  /**
   * Finds an element and scrolls with the wheel until it is in view.
   *
   * @throws {BehaviorError} when no element matches, or it cannot be brought into view.
   */
  private async reveal(selector: string): Promise<{ box: Box; metrics: SurfaceMetrics }> {
    const { surface, random } = this.parts;
    for (let attempt = 0; attempt < 60; attempt += 1) {
      const [box, metrics] = await Promise.all([surface.boxOf(selector), surface.metrics()]);
      if (box === null) {
        throw new BehaviorError('element_not_found', `no element matches ${selector}`);
      }
      if (Human.inView(box, metrics)) {
        return { box, metrics };
      }
      await this.overPage(metrics);
      const middle = box.y + box.height / 2;
      const direction = middle < 0 ? 'up' : 'down';
      const remaining = Math.abs(middle - metrics.viewport.height * random.uniform(0.35, 0.6));
      const before = metrics.scrollY;
      await this.flick(direction, remaining);
      const after = await surface.metrics();
      if (after.scrollY === before && attempt > 1) {
        break;
      }
      await this.clock.sleep(random.uniform(90, 260));
    }
    throw new BehaviorError('element_not_reachable', `${selector} cannot be scrolled into view`);
  }

  /** Moves the pointer onto an element and rests there as a person does before acting. */
  async hover(selector: string): Promise<void> {
    const { box, metrics } = await this.reveal(selector);
    const point = planClickPoint(box, this.parts.random);
    await this.moveToScreen(
      { x: metrics.origin.x + point.x, y: metrics.origin.y + point.y },
      Math.min(box.width, box.height),
    );
    const { hover } = this.parts.profile;
    await this.clock.sleep(Math.min(1500, this.parts.random.logNormal(hover.median, hover.sigma)));
  }

  /** Clicks an element: the pointer travels there, rests, the button goes down and up. */
  async click(selector: string): Promise<void> {
    await this.hover(selector);
    await this.pressButton();
  }

  /**
   * Presses the left button where the pointer rests and lets it go: the
   * second half of a click, for a caller that looks at what the pointer
   * rests on (after {@link Human.hover}) before it clicks.
   */
  async pressButton(): Promise<void> {
    const { buttonHold } = this.parts.profile;
    await this.parts.device.buttonDown('left');
    await this.clock.sleep(
      Math.max(35, this.parts.random.normal(buttonHold.mean, buttonHold.deviation)),
    );
    await this.parts.device.buttonUp('left');
    await this.parts.device.flush();
    this.counts.clicks += 1;
  }

  /**
   * Scrolls down to a depth of the document with the wheel, a flick at a
   * time, resting after each for as long as the text that came into view
   * takes this persona to skim, and now and then scrolling back a little.
   *
   * @param depth where to end, as a share of the scrollable height.
   * @param deadline a time of the clock after which no further flick starts
   *   and no rest goes on: the scrolling ends at most one flick after it.
   * @returns why the scrolling ended. A depth that was reached with the
   *   last flick before the deadline counts as reached.
   */
  async scrollTo(depth: number, deadline = Number.POSITIVE_INFINITY): Promise<ScrollOutcome> {
    const { surface, profile, random } = this.parts;
    let metrics = await surface.metrics();
    await this.overPage(metrics);
    for (let count = 0; count < MAX_FLICKS; count += 1) {
      const scrollable = Math.max(0, metrics.scrollHeight - metrics.viewport.height);
      const remaining = scrollable * depth - metrics.scrollY;
      if (remaining < Math.max(8, this.notchPx / 3)) {
        // A document that ends inside its window has no depth to reach.
        return scrollable === 0 ? 'page' : 'depth';
      }
      if (this.clock.now() >= deadline) {
        return 'deadline';
      }
      const before = metrics.scrollY;
      const notches = await this.flick('down', remaining);
      metrics = await surface.metrics();
      const moved = metrics.scrollY - before;
      if (moved <= 0) {
        // The page does not scroll any further (or at all).
        return 'page';
      }
      this.notchPx = Math.max(20, moved / notches);
      // A rest never outlasts the time the scrolling was given: whoever set
      // the deadline has something to do after it. (The draws are the same
      // either way, so a page replays.)
      const left = () => Math.max(0, deadline - this.clock.now());
      const rest = readingPause(await surface.visibleWords(), profile, random);
      await this.clock.sleep(Math.min(rest, left()));
      if (scrollsBack(profile, random) && this.clock.now() < deadline) {
        await this.flick('up', this.notchPx * random.uniform(0.5, 2));
        const settle = random.uniform(350, 1100);
        await this.clock.sleep(Math.min(settle, left()));
        metrics = await surface.metrics();
      }
    }
    // The bound on the flicks: a page that grows as fast as it is scrolled.
    return 'deadline';
  }

  /** Types text on the keyboard, key by key. The caller has put the focus where it belongs. */
  async type(text: string, options: { readonly typos: boolean }): Promise<void> {
    const { device, profile, random } = this.parts;
    const plan = planTyping(text, (key) => device.levelsOf(key), profile, random, options);
    const started = this.clock.now();
    for (const action of plan.actions) {
      const wait = started + action.t - this.clock.now();
      if (wait > 0) {
        await this.clock.sleep(wait);
      }
      if (action.action === 'down') {
        await device.keyDown(action.key);
        this.counts.keys += 1;
      } else {
        await device.keyUp(action.key);
      }
    }
    await device.flush();
    this.counts.typos += plan.typos;
  }

  /** Presses one key, with the modifiers the keyboard needs for it. */
  press(key: string): Promise<void> {
    const levels = this.parts.device.levelsOf(key);
    return this.chord(
      [...(levels.shift ? ['Shift'] : []), ...(levels.altGr ? ['AltGraph'] : [])],
      key,
    );
  }

  /** Presses a key while holding others, as for a keyboard shortcut. */
  async chord(modifiers: readonly string[], key: string): Promise<void> {
    const { device, profile, random } = this.parts;
    for (const modifier of modifiers) {
      await device.keyDown(modifier);
      await this.clock.sleep(random.uniform(35, 95));
    }
    await device.keyDown(key);
    await this.clock.sleep(Math.max(35, random.normal(profile.typing.hold, 14)));
    await device.keyUp(key);
    for (const modifier of [...modifiers].reverse()) {
      await this.clock.sleep(random.uniform(10, 60));
      await device.keyUp(modifier);
    }
    await device.flush();
    this.counts.keys += 1 + modifiers.length;
  }

  /** How long this persona would stay on a page when nobody says. */
  drawDwell(): number {
    return drawDwell(this.parts.profile, this.parts.random);
  }

  /**
   * How long this persona stays on a page a paced flow arrived on (spec
   * §10.5): around the flow's median or the persona's own, never below the
   * flow's floor. Drawn from the stream of the request, so the same seeds
   * give the same flow.
   */
  drawPacedDwell(pace: DwellPace): number {
    return drawPacedDwell(pace, this.parts.profile, this.parts.random);
  }

  /** How long this persona takes before its next action on the page it is on. */
  drawThinking(): number {
    return drawThinking(this.parts.tempo ?? DEFAULT_TEMPO, this.parts.random);
  }

  /**
   * Spends time on the page. With `wander`, the idle hand moves the mouse a
   * little every few seconds, inside the page.
   */
  async dwell(ms: number, options: { readonly wander: boolean }): Promise<void> {
    const { profile, random, surface } = this.parts;
    const end = this.clock.now() + ms;
    while (this.clock.now() < end) {
      const left = end - this.clock.now();
      const pause = random.uniform(profile.wander[0], profile.wander[1]) * 1000;
      if (!options.wander || pause >= left) {
        await this.clock.sleep(left);
        return;
      }
      await this.clock.sleep(pause);
      const metrics = await surface.metrics();
      const area = { ...metrics.origin, ...metrics.viewport };
      const path = planWander(await this.position(), area, profile, random);
      if (path.samples.length > 0 && path.duration < end - this.clock.now()) {
        await this.play(path);
        this.counts.wanders += 1;
      }
    }
  }
}
