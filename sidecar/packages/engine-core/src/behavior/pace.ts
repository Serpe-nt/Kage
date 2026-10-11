import { SeededRandom } from '../seeded-random.js';

import { HUMAN_BOUNDS, type MotorProfile } from './profile.js';
import { drawDwell } from './scroll.js';

/**
 * How a persona paces a flow (spec §10.5): how long it stays on a page it
 * arrived on, and how long it takes between two actions on one page.
 *
 * A flow that does not pace itself runs its steps one right after the
 * other: the first click a second after the page loaded, and a constant
 * time between two clicks. Both are things a page can measure.
 */

/** Label of the stream a persona's tempo is drawn from. A new version is a new population. */
export const TEMPO_LABEL = 'tempo-v1';

/**
 * How quickly one persona goes from one action to the next.
 *
 * Like the motor profile it is drawn once from the persona's seed and never
 * from a request. It has a stream of its own, so that the motor profile,
 * whose id is recorded with every result, stays what it was.
 */
export interface Tempo {
  /** Thinking time between two actions on one page: a log-normal, median in milliseconds. */
  readonly thinking: { readonly median: number; readonly sigma: number };
}

/** The tempo of a persona nobody drew one for: the middle of the population. */
export const DEFAULT_TEMPO: Tempo = {
  thinking: { median: HUMAN_BOUNDS.thinkingBaseMs, sigma: HUMAN_BOUNDS.thinkingSigma },
};

/** Draws the tempo of a persona from its seed: the same seed, the same tempo. */
export function tempo(seed: string): Tempo {
  const random = new SeededRandom(seed, TEMPO_LABEL);
  const [quickest, slowest] = HUMAN_BOUNDS.thinkingFactor;
  return {
    thinking: {
      median: Math.round(HUMAN_BOUNDS.thinkingBaseMs * random.uniform(quickest, slowest)),
      sigma: HUMAN_BOUNDS.thinkingSigma,
    },
  };
}

/**
 * Sigma of the log-normal a caller's dwell is drawn from: its 95th
 * percentile is twice its median (`ln 2 / 1.645`).
 */
export const PACED_DWELL_SIGMA = Math.LN2 / 1.6448536269514722;

/** How often a time that came out of its bounds is drawn again before the bound is used. */
export const MAX_REDRAWS = 64;

/** What a paced flow says about its dwells. */
export interface DwellPace {
  /** Median of a dwell in milliseconds; `null` for the persona's own human dwell. */
  readonly median: number | null;
  /** No dwell is shorter than this. */
  readonly floor: number;
}

/**
 * How long a persona stays on a page a paced flow arrived on.
 *
 * A draw below the floor is drawn again and never raised to it: a pile of
 * dwells at exactly the floor would be a signature of its own. Only when
 * {@link MAX_REDRAWS} draws stayed below it (a floor far above the median)
 * is the floor itself used.
 */
export function drawPacedDwell(
  pace: DwellPace,
  profile: MotorProfile,
  random: SeededRandom,
): number {
  for (let draw = 0; draw < MAX_REDRAWS; draw += 1) {
    const drawn =
      pace.median === null
        ? drawDwell(profile, random)
        : Math.round(random.logNormal(pace.median, PACED_DWELL_SIGMA));
    if (drawn >= pace.floor) {
      return drawn;
    }
  }
  return pace.floor;
}

/**
 * How long a persona takes before its next action on a page it is already
 * on: a log-normal around its own median, inside the bounds of
 * {@link HUMAN_BOUNDS}. A draw outside them is drawn again, so that nothing
 * piles up at either bound.
 */
export function drawThinking(own: Tempo, random: SeededRandom): number {
  const [shortest, longest] = HUMAN_BOUNDS.thinkingMs;
  const { median, sigma } = own.thinking;
  for (let draw = 0; draw < MAX_REDRAWS; draw += 1) {
    const drawn = Math.round(random.logNormal(median, sigma));
    if (drawn >= shortest && drawn <= longest) {
      return drawn;
    }
  }
  return Math.max(shortest, Math.min(longest, median));
}
