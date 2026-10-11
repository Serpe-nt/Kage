import { createHash } from 'node:crypto';

import { SeededRandom } from '../seeded-random.js';

/** Label of the stream a motor profile is drawn from. A new version is a new population. */
export const MOTOR_PROFILE_LABEL = 'motor-profile-v1';

/**
 * How one persona moves, scrolls, reads and types (spec §10.4).
 *
 * A profile is drawn once from the persona's seed and never from a request,
 * so a persona keeps its hand for life, and twenty personas have twenty
 * hands. A request only adds the variation one person shows from one
 * movement to the next.
 */
export interface MotorProfile {
  /** First 16 hexadecimal characters of the SHA-256 of the profile; recorded with every result. */
  readonly id: string;
  /** Fitts' law, `time = a + b * log2(distance / width + 1)`, in milliseconds and milliseconds per bit. */
  readonly fitts: { readonly a: number; readonly b: number };
  /** Where in a movement the speed peaks, as a share of its duration. Below 0.5: a fast start, a slow approach. */
  readonly velocityPeak: number;
  /** How a path bends: to which side, by how much of the distance, and how much that varies. */
  readonly curvature: { readonly side: -1 | 1; readonly bias: number; readonly spread: number };
  /** Standard deviation of the hand's tremor in pixels. */
  readonly tremor: number;
  /** Milliseconds between two pointer positions the mouse reports. */
  readonly pollInterval: number;
  readonly overshoot: {
    /** Share of long movements that go past their target. */
    readonly probability: number;
    /** How far past, as a share of the distance. */
    readonly size: number;
    /** Pause before the correction, in milliseconds. */
    readonly pause: readonly [number, number];
  };
  /** Rest on an element before clicking it: a log-normal, median in milliseconds. */
  readonly hover: { readonly median: number; readonly sigma: number };
  /** How long a button stays down: mean and deviation in milliseconds. */
  readonly buttonHold: { readonly mean: number; readonly deviation: number };
  readonly wheel: {
    /** Mean notches per flick of the wheel. */
    readonly notches: number;
    /** Milliseconds between two notches of one flick. */
    readonly interval: readonly [number, number];
  };
  readonly reading: {
    /** Words per minute when reading. */
    readonly wordsPerMinute: number;
    /** Share of the visible text that is actually read: people skim. */
    readonly attention: number;
    /** Chance that a flick is followed by a short scroll back up. */
    readonly scrollBack: number;
  };
  /** Dwell on a page: a log-normal around the spec's median, per persona. */
  readonly dwell: { readonly median: number; readonly sigma: number };
  readonly typing: {
    /** Median milliseconds between two key presses. */
    readonly interval: number;
    readonly sigma: number;
    /** Mean milliseconds a key stays down. */
    readonly hold: number;
    /** Share of letters typed wrong and corrected, when typos are on. */
    readonly typoRate: number;
    /** Milliseconds before a typo is noticed. */
    readonly correctionPause: readonly [number, number];
  };
  /** Seconds between two idle movements of the mouse while dwelling. */
  readonly wander: readonly [number, number];
}

/** The bounds every profile lies in. Tests and the judge of recorded input use the same numbers. */
export const HUMAN_BOUNDS = {
  fittsA: [90, 190],
  fittsB: [95, 175],
  velocityPeak: [0.34, 0.48],
  curvatureBias: [0.02, 0.12],
  tremor: [0.25, 0.9],
  overshootProbability: [0.12, 0.38],
  overshootSize: [0.02, 0.09],
  hoverMedian: [140, 320],
  buttonHold: [65, 135],
  wheelNotches: [1.4, 4.2],
  wordsPerMinute: [190, 320],
  attention: [0.06, 0.22],
  scrollBack: [0.05, 0.2],
  /** The spec's median of 6 s, scaled per persona. */
  dwellMedian: [4500, 8100],
  dwellSigma: [0.45, 0.85],
  /** Cap of a drawn dwell (spec §10.4). */
  dwellCapMs: 45_000,
  /** Per-key delay of the spec: every interval between two key presses lies in it. */
  keyIntervalMs: [60, 220],
  keyIntervalMedian: [105, 185],
  keyHold: [55, 115],
  typoRate: [0.008, 0.035],
  /** Median thinking time between two actions on one page, before a persona's own factor. */
  thinkingBaseMs: 600,
  /** The factor a persona scales that median by (`pace.ts`): quick ones and slow ones. */
  thinkingFactor: [0.7, 1.5],
  thinkingSigma: 0.5,
  /** Every thinking time lies in it. */
  thinkingMs: [80, 5000],
} as const;

const round = (value: number, digits: number) => {
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
};

/**
 * Draws the motor profile of a persona from its seed.
 *
 * The same seed always gives the same profile; the draw uses its own
 * stream, so nothing else that is derived from the seed changes it.
 */
export function motorProfile(seed: string): MotorProfile {
  const random = new SeededRandom(seed, MOTOR_PROFILE_LABEL);
  const between = (range: readonly [number, number], digits = 3) =>
    round(random.uniform(range[0], range[1]), digits);
  const correction = between([70, 110], 0);
  const noticed = between([180, 300], 0);
  const wanderEvery = between([4, 8], 1);
  const notchGap = between([28, 48], 0);
  const body: Omit<MotorProfile, 'id'> = {
    fitts: { a: between(HUMAN_BOUNDS.fittsA, 0), b: between(HUMAN_BOUNDS.fittsB, 0) },
    velocityPeak: between(HUMAN_BOUNDS.velocityPeak),
    curvature: {
      side: random.chance(0.5) ? 1 : -1,
      bias: between(HUMAN_BOUNDS.curvatureBias),
      spread: between([0.015, 0.05]),
    },
    tremor: between(HUMAN_BOUNDS.tremor),
    // Most mice report 125 times a second; one persona in five has a faster one.
    pollInterval: random.chance(0.2) ? 4 : 8,
    overshoot: {
      probability: between(HUMAN_BOUNDS.overshootProbability),
      size: between(HUMAN_BOUNDS.overshootSize),
      pause: [correction, correction + between([30, 50], 0)],
    },
    hover: { median: between(HUMAN_BOUNDS.hoverMedian, 0), sigma: between([0.3, 0.55]) },
    buttonHold: { mean: between(HUMAN_BOUNDS.buttonHold, 0), deviation: between([10, 22], 0) },
    wheel: {
      notches: between(HUMAN_BOUNDS.wheelNotches, 2),
      interval: [notchGap, notchGap + between([20, 37], 0)],
    },
    reading: {
      wordsPerMinute: between(HUMAN_BOUNDS.wordsPerMinute, 0),
      attention: between(HUMAN_BOUNDS.attention),
      scrollBack: between(HUMAN_BOUNDS.scrollBack),
    },
    dwell: {
      median: between(HUMAN_BOUNDS.dwellMedian, 0),
      sigma: between(HUMAN_BOUNDS.dwellSigma),
    },
    typing: {
      interval: between(HUMAN_BOUNDS.keyIntervalMedian, 0),
      sigma: between([0.16, 0.3]),
      hold: between(HUMAN_BOUNDS.keyHold, 0),
      typoRate: between(HUMAN_BOUNDS.typoRate, 4),
      correctionPause: [noticed, noticed + between([120, 220], 0)],
    },
    wander: [wanderEvery, wanderEvery + between([3, 6], 1)],
  };
  const id = createHash('sha256').update(JSON.stringify(body)).digest('hex').slice(0, 16);
  return { id, ...body };
}

/**
 * A profile as a vector of comparable numbers, each scaled to its human
 * range. The distance of two vectors says how differently two personas
 * move; the diversity test keeps it above a floor.
 */
export function profileVector(profile: MotorProfile): number[] {
  const scaled = (value: number, range: readonly [number, number]) =>
    (value - range[0]) / (range[1] - range[0]);
  return [
    scaled(profile.fitts.a, HUMAN_BOUNDS.fittsA),
    scaled(profile.fitts.b, HUMAN_BOUNDS.fittsB),
    scaled(profile.velocityPeak, HUMAN_BOUNDS.velocityPeak),
    scaled(profile.curvature.bias * profile.curvature.side, [-0.12, 0.12]),
    scaled(profile.tremor, HUMAN_BOUNDS.tremor),
    scaled(profile.overshoot.probability, HUMAN_BOUNDS.overshootProbability),
    scaled(profile.overshoot.size, HUMAN_BOUNDS.overshootSize),
    scaled(profile.hover.median, HUMAN_BOUNDS.hoverMedian),
    scaled(profile.buttonHold.mean, HUMAN_BOUNDS.buttonHold),
    scaled(profile.wheel.notches, HUMAN_BOUNDS.wheelNotches),
    scaled(profile.reading.wordsPerMinute, HUMAN_BOUNDS.wordsPerMinute),
    scaled(profile.reading.attention, HUMAN_BOUNDS.attention),
    scaled(profile.dwell.median, HUMAN_BOUNDS.dwellMedian),
    scaled(profile.typing.interval, HUMAN_BOUNDS.keyIntervalMedian),
    scaled(profile.typing.hold, HUMAN_BOUNDS.keyHold),
    scaled(profile.typing.typoRate, HUMAN_BOUNDS.typoRate),
  ];
}
