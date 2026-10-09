import type { SeededRandom } from '../seeded-random.js';

import type { Box, Point } from './device.js';
import type { MotorProfile } from './profile.js';

/** One position the mouse reports, `t` milliseconds after the movement began. */
export interface PointerSample {
  readonly t: number;
  readonly x: number;
  readonly y: number;
}

/** A planned movement of the pointer. */
export interface PointerPath {
  readonly from: Point;
  readonly to: Point;
  /** Positions in order; the last one is the target. Empty when there is nowhere to go. */
  readonly samples: readonly PointerSample[];
  /** Whether the movement went past its target and came back. */
  readonly overshoot: boolean;
  /** Milliseconds from the first to the last sample. */
  readonly duration: number;
}

/** A movement longer than this may overshoot. */
export const OVERSHOOT_MIN_DISTANCE = 250;

/** Bounds of the duration of one movement, whatever Fitts' law says. */
const MIN_DURATION_MS = 70;
const MAX_DURATION_MS = 2600;

const distance = (a: Point, b: Point) => Math.hypot(b.x - a.x, b.y - a.y);

/** A cubic Bézier curve at parameter `u`. */
function bezier(p0: Point, p1: Point, p2: Point, p3: Point, u: number): Point {
  const v = 1 - u;
  const a = v * v * v;
  const b = 3 * v * v * u;
  const c = 3 * v * u * u;
  const d = u * u * u;
  return {
    x: a * p0.x + b * p1.x + c * p2.x + d * p3.x,
    y: a * p0.y + b * p1.y + c * p2.y + d * p3.y,
  };
}

/**
 * How far along its path a hand is after a share `tau` of the movement's
 * time. The minimum-jerk profile (a bell-shaped speed) with its peak moved
 * to `peak`: a hand accelerates faster than it brakes.
 */
export function progress(tau: number, peak: number): number {
  const clamped = Math.max(0, Math.min(1, tau));
  // Warps time so that the symmetric profile's middle falls on `peak`.
  const warped = clamped ** (Math.log(0.5) / Math.log(peak));
  return 10 * warped ** 3 - 15 * warped ** 4 + 6 * warped ** 5;
}

/** Fitts' law: the time a hand needs to reach a target of a size at a distance. */
export function fittsTime(profile: MotorProfile, length: number, targetSize: number): number {
  const bits = Math.log2(length / Math.max(4, targetSize) + 1);
  return profile.fitts.a + profile.fitts.b * bits;
}

interface Segment {
  readonly from: Point;
  readonly to: Point;
  readonly duration: number;
  /** Signed bend of the path, as a share of its length. */
  readonly bend: number;
  /** Whether the two control points bend to opposite sides (an S-shaped path). */
  readonly sShaped: boolean;
  readonly inner: readonly [number, number];
}

function segment(
  from: Point,
  to: Point,
  targetSize: number,
  profile: MotorProfile,
  random: SeededRandom,
  speed: number,
): Segment {
  const length = distance(from, to);
  const duration = Math.max(
    MIN_DURATION_MS,
    Math.min(MAX_DURATION_MS, fittsTime(profile, length, targetSize) * speed),
  );
  return {
    from,
    to,
    duration,
    bend: profile.curvature.side * random.normal(profile.curvature.bias, profile.curvature.spread),
    sShaped: random.chance(0.12),
    inner: [random.uniform(0.22, 0.4), random.uniform(0.6, 0.8)],
  };
}

function sample(
  part: Segment,
  offset: number,
  profile: MotorProfile,
  random: SeededRandom,
  out: PointerSample[],
): void {
  const { from, to } = part;
  const length = distance(from, to);
  if (length < 1) {
    return;
  }
  const along = { x: (to.x - from.x) / length, y: (to.y - from.y) / length };
  const across = { x: -along.y, y: along.x };
  const control = (share: number, bend: number): Point => ({
    x: from.x + along.x * length * share + across.x * length * bend,
    y: from.y + along.y * length * share + across.y * length * bend,
  });
  const p1 = control(part.inner[0], part.bend);
  const p2 = control(part.inner[1], part.sShaped ? -part.bend * 0.6 : part.bend * 0.8);
  // Tremor is slow compared with the reporting rate: each position keeps
  // most of the previous one's error.
  let tremorX = 0;
  let tremorY = 0;
  const steps = Math.max(2, Math.round(part.duration / profile.pollInterval));
  for (let step = 1; step <= steps; step += 1) {
    const last = step === steps;
    const jitter = last ? 0 : random.uniform(-0.15, 0.15) * profile.pollInterval;
    const time = Math.min(part.duration, step * profile.pollInterval + jitter);
    const point = last
      ? to
      : bezier(from, p1, p2, to, progress(time / part.duration, profile.velocityPeak));
    tremorX = tremorX * 0.7 + random.normal(0, profile.tremor) * 0.55;
    tremorY = tremorY * 0.7 + random.normal(0, profile.tremor) * 0.55;
    const x = Math.round(point.x + (last ? 0 : tremorX));
    const y = Math.round(point.y + (last ? 0 : tremorY));
    const previous = out.at(-1);
    // A mouse reports movement, not time: a position equal to the last one is not sent.
    if (previous?.x !== x || previous.y !== y) {
      out.push({ t: Math.round((offset + time) * 10) / 10, x, y });
    }
  }
}

/**
 * Plans how the pointer gets from one point to another.
 *
 * A curved path (never a straight line for a movement of any length), a
 * duration from Fitts' law and the persona's own constants, a speed that
 * rises fast and falls slowly, positions at the mouse's reporting rate in
 * whole pixels with the hand's tremor, and for a long movement sometimes a
 * miss past the target and a short correction back.
 *
 * @param targetSize the smaller side of what is aimed at, in pixels.
 */
export function planMove(
  from: Point,
  to: Point,
  targetSize: number,
  profile: MotorProfile,
  random: SeededRandom,
): PointerPath {
  const start = { x: Math.round(from.x), y: Math.round(from.y) };
  const end = { x: Math.round(to.x), y: Math.round(to.y) };
  const length = distance(start, end);
  const samples: PointerSample[] = [];
  if (length < 1) {
    return { from: start, to: end, samples, overshoot: false, duration: 0 };
  }
  // One person is not equally fast every time.
  const speed = Math.max(0.7, Math.min(1.6, random.logNormal(1, 0.14)));
  const overshoot =
    length >= OVERSHOOT_MIN_DISTANCE && random.chance(profile.overshoot.probability);
  if (!overshoot) {
    sample(segment(start, end, targetSize, profile, random, speed), 0, profile, random, samples);
  } else {
    const past = length * profile.overshoot.size * random.uniform(0.6, 1.4);
    const sideways = random.normal(0, past * 0.35);
    const along = { x: (end.x - start.x) / length, y: (end.y - start.y) / length };
    const miss = {
      x: Math.round(end.x + along.x * past - along.y * sideways),
      y: Math.round(end.y + along.y * past + along.x * sideways),
    };
    const first = segment(start, miss, targetSize * 2, profile, random, speed * 0.92);
    sample(first, 0, profile, random, samples);
    const pause = random.uniform(profile.overshoot.pause[0], profile.overshoot.pause[1]);
    const back = segment(miss, end, targetSize, profile, random, speed);
    sample(back, first.duration + pause, profile, random, samples);
  }
  const final = samples.at(-1);
  if (final?.x !== end.x || final.y !== end.y) {
    samples.push({ t: (final?.t ?? 0) + profile.pollInterval, x: end.x, y: end.y });
  }
  return { from: start, to: end, samples, overshoot, duration: samples.at(-1)?.t ?? 0 };
}

/**
 * Where on an element a click lands: near the middle, never on the exact
 * centre every time, never outside.
 */
export function planClickPoint(box: Box, random: SeededRandom): Point {
  const inside = (center: number, size: number) => {
    const reach = Math.max(0, size / 2 - Math.min(3, size / 4));
    const offset = Math.max(-reach, Math.min(reach, random.normal(0, size / 6)));
    return Math.round(center + offset);
  };
  return {
    x: inside(box.x + box.width / 2, box.width),
    y: inside(box.y + box.height / 2, box.height),
  };
}

/** A small, slow movement of an idle hand, staying inside an area. */
export function planWander(
  from: Point,
  area: Box,
  profile: MotorProfile,
  random: SeededRandom,
): PointerPath {
  const reach = random.uniform(18, 140);
  const angle = random.uniform(0, 2 * Math.PI);
  const clamp = (value: number, min: number, max: number) => Math.max(min, Math.min(max, value));
  const to = {
    x: clamp(from.x + Math.cos(angle) * reach, area.x + 4, area.x + area.width - 4),
    y: clamp(from.y + Math.sin(angle) * reach, area.y + 4, area.y + area.height - 4),
  };
  // An idle hand does not aim: a large target, an unhurried movement.
  const slow: MotorProfile = {
    ...profile,
    fitts: { a: profile.fitts.a * 1.6, b: profile.fitts.b * 1.5 },
    overshoot: { ...profile.overshoot, probability: 0 },
  };
  return planMove(from, to, 120, slow, random);
}

/** What can be measured on one planned or recorded movement. */
export interface PathStatistics {
  /** Straight distance from the first to the last position. */
  readonly chord: number;
  /** Length of the path as travelled. */
  readonly travelled: number;
  /** `travelled / chord`: exactly 1 only for a straight line. */
  readonly straightness: number;
  /** Largest distance of any position from the straight line, in pixels. */
  readonly maxDeviation: number;
  /** Milliseconds from the first to the last position. */
  readonly duration: number;
  /** Highest speed between two positions, pixels per millisecond. */
  readonly peakSpeed: number;
  /** When the highest speed was reached, as a share of the duration. */
  readonly peakAt: number;
  /** Ratio of the largest to the smallest time between two positions. */
  readonly intervalSpread: number;
  /** How many positions. */
  readonly count: number;
}

/** Measures a movement given as positions over time. */
export function pathStatistics(samples: readonly PointerSample[]): PathStatistics | null {
  const first = samples[0];
  const last = samples.at(-1);
  if (first === undefined || last === undefined || samples.length < 3) {
    return null;
  }
  const chord = distance(first, last);
  let travelled = 0;
  let maxDeviation = 0;
  let peakSpeed = 0;
  let peakAt = 0;
  let shortest = Number.POSITIVE_INFINITY;
  let longest = 0;
  // Speed over a window of a few positions: single positions are whole
  // pixels a few milliseconds apart, too coarse to compare.
  const window = Math.max(1, Math.round(samples.length / 12));
  for (let index = 1; index < samples.length; index += 1) {
    const current = samples[index];
    const previous = samples[index - 1];
    if (current === undefined || previous === undefined) {
      continue;
    }
    travelled += distance(previous, current);
    const interval = current.t - previous.t;
    if (interval > 0) {
      shortest = Math.min(shortest, interval);
      longest = Math.max(longest, interval);
    }
    if (chord > 0) {
      const cross =
        (last.x - first.x) * (first.y - current.y) - (first.x - current.x) * (last.y - first.y);
      maxDeviation = Math.max(maxDeviation, Math.abs(cross) / chord);
    }
    const behind = samples[Math.max(0, index - window)];
    if (behind !== undefined && current.t > behind.t) {
      const speed = distance(behind, current) / (current.t - behind.t);
      if (speed > peakSpeed) {
        peakSpeed = speed;
        peakAt = ((current.t + behind.t) / 2 - first.t) / Math.max(1, last.t - first.t);
      }
    }
  }
  return {
    chord,
    travelled,
    straightness: chord === 0 ? Number.POSITIVE_INFINITY : travelled / chord,
    maxDeviation,
    duration: last.t - first.t,
    peakSpeed,
    peakAt,
    intervalSpread: shortest === Number.POSITIVE_INFINITY ? 1 : longest / shortest,
    count: samples.length,
  };
}
