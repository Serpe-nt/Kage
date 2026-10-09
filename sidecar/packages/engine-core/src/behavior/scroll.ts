import type { SeededRandom } from '../seeded-random.js';

import { HUMAN_BOUNDS, type MotorProfile } from './profile.js';

/** A turn of the wheel: some notches in one direction, a short time apart. */
export interface Flick {
  readonly direction: 'up' | 'down';
  /** Milliseconds to wait before each notch; the first is 0. */
  readonly gaps: readonly number[];
}

/** Shortest and longest pause after a flick, whatever the text says. */
const MIN_PAUSE_MS = 220;
const MAX_PAUSE_MS = 9000;

/** What a notch of the wheel scrolls before the first one was measured: browsers use 100 to 120. */
export const ASSUMED_NOTCH_PX = 110;

/**
 * Plans one flick of the wheel towards a position.
 *
 * The number of notches varies around the persona's own, and never
 * scrolls much further than is left to go.
 *
 * @param remaining pixels still to scroll in the direction; at least one notch is planned.
 * @param notchPx what one notch scrolled when it was last measured.
 */
export function planFlick(
  direction: 'up' | 'down',
  remaining: number,
  notchPx: number,
  profile: MotorProfile,
  random: SeededRandom,
): Flick {
  const wanted = Math.max(1, Math.round(random.normal(profile.wheel.notches, 0.9)));
  const needed = Math.max(1, Math.ceil(remaining / Math.max(1, notchPx)));
  const notches = Math.min(wanted, needed, 6);
  const gaps = Array.from({ length: notches }, (_unused, index) =>
    index === 0 ? 0 : Math.round(random.uniform(profile.wheel.interval[0], profile.wheel.interval[1])),
  );
  return { direction, gaps };
}

/**
 * How long a person rests after a flick, given how much text came into
 * view: proportional to the words, at the persona's reading speed, times
 * the share of a page a person actually reads.
 */
export function readingPause(words: number, profile: MotorProfile, random: SeededRandom): number {
  const reading =
    (Math.max(0, words) / profile.reading.wordsPerMinute) * 60_000 * profile.reading.attention;
  const pause = reading * random.logNormal(1, 0.25) + random.uniform(120, 320);
  return Math.round(Math.max(MIN_PAUSE_MS, Math.min(MAX_PAUSE_MS, pause)));
}

/** Whether this flick is followed by a short scroll back up, as when a line was missed. */
export function scrollsBack(profile: MotorProfile, random: SeededRandom): boolean {
  return random.chance(profile.reading.scrollBack);
}

/**
 * How long a persona stays on a page when nobody fixed it: a log-normal
 * around its own median, never longer than the cap (spec §10.4).
 */
export function drawDwell(profile: MotorProfile, random: SeededRandom): number {
  const drawn = random.logNormal(profile.dwell.median, profile.dwell.sigma);
  return Math.round(Math.max(600, Math.min(HUMAN_BOUNDS.dwellCapMs, drawn)));
}
