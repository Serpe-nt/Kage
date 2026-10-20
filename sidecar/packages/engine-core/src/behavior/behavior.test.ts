import { describe, expect, it } from 'vitest';

import { SeededRandom } from '../seeded-random.js';

import type {
  Box,
  Clock,
  InputDevice,
  KeyLevels,
  PageSurface,
  Point,
  SurfaceMetrics,
} from './device.js';
import { BehaviorError, Human } from './human.js';
import {
  OVERSHOOT_MIN_DISTANCE,
  fittsTime,
  pathStatistics,
  planClickPoint,
  planMove,
  planWander,
  progress,
} from './mouse.js';
import {
  DEFAULT_TEMPO,
  MAX_REDRAWS,
  PACED_DWELL_SIGMA,
  TEMPO_LABEL,
  drawPacedDwell,
  drawThinking,
  tempo,
} from './pace.js';
import { HUMAN_BOUNDS, MOTOR_PROFILE_LABEL, motorProfile, profileVector } from './profile.js';
import { drawDwell, planFlick, readingPause, scrollsBack } from './scroll.js';
import { keyIntervals, planTyping } from './typing.js';

/** Twenty personas, as many as the pool has proxies. */
const SEEDS = Array.from({ length: 20 }, (_unused, index) =>
  (index + 1).toString(16).padStart(2, '0').repeat(32),
);
const seed = SEEDS[0] ?? '';
const stream = (label: string, of = seed) => new SeededRandom(of, label);
const usLayout = (key: string): KeyLevels => ({
  shift: /^[A-Z!@#$%^&*()_+{}|:"<>?~]$/.test(key),
  altGr: false,
  onLayout: true,
});

describe('motor profiles', () => {
  it('gives one persona one hand, always the same', () => {
    expect(motorProfile(seed)).toEqual(motorProfile(seed));
    expect(motorProfile(seed).id).toMatch(/^[0-9a-f]{16}$/);
    expect(MOTOR_PROFILE_LABEL).toBe('motor-profile-v1');
  });

  it('keeps every hand inside human bounds', () => {
    const inside = (value: number, range: readonly [number, number]) => {
      expect(value).toBeGreaterThanOrEqual(range[0]);
      expect(value).toBeLessThanOrEqual(range[1]);
    };
    for (const each of SEEDS) {
      const profile = motorProfile(each);
      inside(profile.fitts.a, HUMAN_BOUNDS.fittsA);
      inside(profile.fitts.b, HUMAN_BOUNDS.fittsB);
      inside(profile.velocityPeak, HUMAN_BOUNDS.velocityPeak);
      inside(profile.curvature.bias, HUMAN_BOUNDS.curvatureBias);
      inside(profile.tremor, HUMAN_BOUNDS.tremor);
      inside(profile.overshoot.probability, HUMAN_BOUNDS.overshootProbability);
      inside(profile.overshoot.size, HUMAN_BOUNDS.overshootSize);
      inside(profile.hover.median, HUMAN_BOUNDS.hoverMedian);
      inside(profile.buttonHold.mean, HUMAN_BOUNDS.buttonHold);
      inside(profile.wheel.notches, HUMAN_BOUNDS.wheelNotches);
      inside(profile.reading.wordsPerMinute, HUMAN_BOUNDS.wordsPerMinute);
      inside(profile.reading.attention, HUMAN_BOUNDS.attention);
      inside(profile.reading.scrollBack, HUMAN_BOUNDS.scrollBack);
      inside(profile.dwell.median, HUMAN_BOUNDS.dwellMedian);
      inside(profile.dwell.sigma, HUMAN_BOUNDS.dwellSigma);
      inside(profile.typing.interval, HUMAN_BOUNDS.keyIntervalMedian);
      inside(profile.typing.hold, HUMAN_BOUNDS.keyHold);
      inside(profile.typing.typoRate, HUMAN_BOUNDS.typoRate);
      expect([4, 8]).toContain(profile.pollInterval);
      expect(profile.overshoot.pause[1]).toBeGreaterThan(profile.overshoot.pause[0]);
      expect(profile.wander[1]).toBeGreaterThan(profile.wander[0]);
    }
  });

  it('gives twenty personas twenty different hands', () => {
    const profiles = SEEDS.map(motorProfile);
    expect(new Set(profiles.map((profile) => profile.id)).size).toBe(20);
    const vectors = profiles.map(profileVector);
    let nearest = Number.POSITIVE_INFINITY;
    for (const [index, a] of vectors.entries()) {
      for (const b of vectors.slice(index + 1)) {
        nearest = Math.min(nearest, Math.hypot(...a.map((value, at) => value - (b[at] ?? 0))));
      }
      for (const value of a) {
        expect(value).toBeGreaterThanOrEqual(0);
        expect(value).toBeLessThanOrEqual(1);
      }
    }
    // The two most alike of twenty still differ by a clear margin over
    // sixteen traits; equal hands would be at distance zero.
    expect(nearest).toBeGreaterThan(0.35);
    // No trait is the same for everybody.
    for (let trait = 0; trait < (vectors[0]?.length ?? 0); trait += 1) {
      const values = vectors.map((vector) => vector[trait] ?? 0);
      expect(Math.max(...values) - Math.min(...values)).toBeGreaterThan(0.4);
    }
    // Both sides of every coin turn up.
    expect(new Set(profiles.map((profile) => profile.curvature.side)).size).toBe(2);
  });
});

describe('mouse paths', () => {
  const from = { x: 120, y: 640 };
  const to = { x: 1180, y: 210 };
  const plans = (count: number, of = seed, target = to, size = 40) => {
    const random = stream('paths', of);
    const profile = motorProfile(of);
    return Array.from({ length: count }, () => planMove(from, target, size, profile, random));
  };

  it('plans the same path for the same seeds, and another for another request', () => {
    const profile = motorProfile(seed);
    const again = () => planMove(from, to, 40, profile, stream('behavior:r1'));
    expect(again()).toEqual(again());
    expect(planMove(from, to, 40, profile, stream('behavior:r2')).samples).not.toEqual(
      again().samples,
    );
  });

  it('never draws a straight line, and ends on the target in whole pixels', () => {
    for (const each of SEEDS) {
      for (const path of plans(12, each)) {
        const measured = pathStatistics(path.samples);
        expect(measured).not.toBeNull();
        expect(measured?.straightness).toBeGreaterThan(1.0005);
        expect(measured?.maxDeviation).toBeGreaterThan(1.5);
        expect(path.samples.at(-1)).toMatchObject(to);
        expect(
          path.samples.every((sample) => Number.isInteger(sample.x) && Number.isInteger(sample.y)),
        ).toBe(true);
        // Time only moves forward, and a mouse does not report standing still.
        for (let index = 1; index < path.samples.length; index += 1) {
          const now = path.samples[index];
          const before = path.samples[index - 1];
          expect(now?.t).toBeGreaterThan(before?.t ?? 0);
          expect(now?.x === before?.x && now?.y === before?.y).toBe(false);
        }
      }
    }
  });

  it('moves with one peak of speed, reached early: a fast start and a slow approach', () => {
    for (const each of SEEDS.slice(0, 8)) {
      const profile = motorProfile(each);
      const direct = plans(40, each).filter((path) => !path.overshoot);
      const peaks = direct.map((path) => pathStatistics(path.samples)?.peakAt ?? 0);
      const mean = peaks.reduce((sum, value) => sum + value, 0) / peaks.length;
      // The peak sits where the persona's profile puts it, before the middle.
      expect(mean).toBeGreaterThan(0.18);
      expect(mean).toBeLessThan(0.5);
      expect(Math.abs(mean - profile.velocityPeak)).toBeLessThan(0.14);
      for (const path of direct) {
        const measured = pathStatistics(path.samples);
        // Not a constant speed: the fastest stretch is well above the average.
        expect(
          (measured?.peakSpeed ?? 0) / ((measured?.travelled ?? 0) / (measured?.duration ?? 1)),
        ).toBeGreaterThan(1.4);
        // The reporting interval varies a little, as a real mouse's does.
        expect(measured?.intervalSpread).toBeGreaterThan(1.05);
      }
    }
    expect(progress(0, 0.4)).toBe(0);
    expect(progress(1, 0.4)).toBe(1);
    expect(progress(0.4, 0.4)).toBeCloseTo(0.5, 5);
    expect(progress(2, 0.4)).toBe(1);
  });

  it('takes the time Fitts measured: longer for far and for small targets', () => {
    const profile = motorProfile(seed);
    expect(fittsTime(profile, 800, 20)).toBeGreaterThan(fittsTime(profile, 200, 20));
    expect(fittsTime(profile, 800, 20)).toBeGreaterThan(fittsTime(profile, 800, 200));
    const durations = plans(60)
      .filter((path) => !path.overshoot)
      .map((path) => path.duration);
    const expected = fittsTime(profile, Math.hypot(to.x - from.x, to.y - from.y), 40);
    const mean = durations.reduce((sum, value) => sum + value, 0) / durations.length;
    expect(Math.abs(mean - expected) / expected).toBeLessThan(0.15);
    // One person is not equally fast every time.
    expect(Math.max(...durations) - Math.min(...durations)).toBeGreaterThan(expected * 0.2);
  });

  it('overshoots long movements as often as the persona does, and never short ones', () => {
    for (const each of SEEDS.slice(0, 6)) {
      const profile = motorProfile(each);
      const long = plans(400, each);
      const share = long.filter((path) => path.overshoot).length / long.length;
      expect(Math.abs(share - profile.overshoot.probability)).toBeLessThan(0.07);
      const missed = long.find((path) => path.overshoot);
      // An overshoot goes past the target and comes back to it.
      const farthest = Math.max(
        ...(missed?.samples ?? []).map((sample) =>
          Math.hypot(sample.x - from.x, sample.y - from.y),
        ),
      );
      expect(farthest).toBeGreaterThan(Math.hypot(to.x - from.x, to.y - from.y));
      expect(missed?.samples.at(-1)).toMatchObject(to);
      const near = plans(200, each, { x: from.x + OVERSHOOT_MIN_DISTANCE - 60, y: from.y });
      expect(near.some((path) => path.overshoot)).toBe(false);
    }
  });

  it('has nothing to plan for a pointer that is already there', () => {
    const still = planMove(
      from,
      { x: from.x + 0.2, y: from.y },
      40,
      motorProfile(seed),
      stream('still'),
    );
    expect(still).toMatchObject({ samples: [], duration: 0, overshoot: false });
    expect(pathStatistics(still.samples)).toBeNull();
  });

  it('clicks near the middle of an element, not on its centre every time, never outside', () => {
    const box: Box = { x: 300, y: 200, width: 120, height: 36 };
    const random = stream('clicks');
    const points = Array.from({ length: 300 }, () => planClickPoint(box, random));
    for (const point of points) {
      expect(point.x).toBeGreaterThan(box.x);
      expect(point.x).toBeLessThan(box.x + box.width);
      expect(point.y).toBeGreaterThan(box.y);
      expect(point.y).toBeLessThan(box.y + box.height);
    }
    expect(new Set(points.map((point) => `${point.x},${point.y}`)).size).toBeGreaterThan(100);
    const meanX = points.reduce((sum, point) => sum + point.x, 0) / points.length;
    expect(Math.abs(meanX - 360)).toBeLessThan(6);
    // A tiny element still gets a click inside it.
    const tiny = planClickPoint({ x: 10, y: 10, width: 4, height: 4 }, random);
    expect(Math.abs(tiny.x - 12)).toBeLessThanOrEqual(1);
    expect(Math.abs(tiny.y - 12)).toBeLessThanOrEqual(1);
  });

  it('wanders slowly and stays inside the page', () => {
    const area: Box = { x: 0, y: 80, width: 800, height: 500 };
    const random = stream('wander');
    const profile = motorProfile(seed);
    for (let count = 0; count < 100; count += 1) {
      const path = planWander({ x: 790, y: 570 }, area, profile, random);
      expect(path.overshoot).toBe(false);
      expect(path.to.x).toBeLessThanOrEqual(796);
      expect(path.to.y).toBeLessThanOrEqual(576);
      expect(path.to.y).toBeGreaterThanOrEqual(84);
    }
  });
});

describe('typing', () => {
  const profile = motorProfile(seed);
  const text = 'The quick brown Fox jumps over 13 lazy dogs, twice!';

  it('plans the same keys for the same seeds', () => {
    const plan = () => planTyping(text, usLayout, profile, stream('typing'), { typos: false });
    expect(plan()).toEqual(plan());
  });

  it("keeps every interval inside the spec, around the persona's own median", () => {
    for (const each of SEEDS) {
      const hand = motorProfile(each);
      const plan = planTyping(text.repeat(4), usLayout, hand, stream('typing', each), {
        typos: false,
      });
      const intervals = keyIntervals(plan.actions);
      expect(intervals).toHaveLength(text.length * 4 - 1);
      for (const interval of intervals) {
        expect(interval).toBeGreaterThanOrEqual(HUMAN_BOUNDS.keyIntervalMs[0] - 0.11);
        expect(interval).toBeLessThanOrEqual(HUMAN_BOUNDS.keyIntervalMs[1] + 0.11);
      }
      const sorted = [...intervals].sort((a, b) => a - b);
      const median = sorted[Math.floor(sorted.length / 2)] ?? 0;
      expect(Math.abs(median - hand.typing.interval)).toBeLessThan(18);
      // Not a metronome.
      expect(new Set(intervals.map((interval) => Math.round(interval))).size).toBeGreaterThan(25);
    }
  });

  it('holds Shift for a capital, across neighbouring ones, and lets it go again', () => {
    const plan = planTyping('aBCd', usLayout, profile, stream('shift'), { typos: false });
    const order = plan.actions.map((action) => `${action.action}:${action.key}`);
    const at = (entry: string) => order.indexOf(entry);
    expect(order.filter((entry) => entry === 'down:Shift')).toHaveLength(1);
    expect(at('down:a')).toBeLessThan(at('down:Shift'));
    expect(at('down:Shift')).toBeLessThan(at('down:B'));
    expect(at('down:C')).toBeLessThan(at('up:Shift'));
    expect(at('up:Shift')).toBeLessThan(at('down:d'));
    // A text that ends on a capital still lets the key go.
    const ending = planTyping('aB', usLayout, profile, stream('shift'), { typos: false });
    expect(ending.actions.at(-1)).toMatchObject({ action: 'up' });
    expect(ending.actions.filter((action) => action.key === 'Shift')).toHaveLength(2);
    // Every key that went down came up, and time never runs backwards.
    for (const planned of [plan, ending]) {
      const down = planned.actions.filter((action) => action.action === 'down').length;
      expect(planned.actions.length).toBe(down * 2);
      for (let index = 1; index < planned.actions.length; index += 1) {
        expect(planned.actions[index]?.t).toBeGreaterThanOrEqual(
          planned.actions[index - 1]?.t ?? 0,
        );
      }
      expect(planned.duration).toBe(planned.actions.at(-1)?.t);
    }
  });

  it('uses AltGr where the layout needs it, and Enter and Tab for line breaks and tabs', () => {
    const german = (key: string): KeyLevels => ({
      shift: false,
      altGr: key === '@',
      onLayout: true,
    });
    const plan = planTyping('a@\n\tb', german, profile, stream('altgr'), { typos: false });
    const keys = plan.actions
      .filter((action) => action.action === 'down')
      .map((action) => action.key);
    expect(keys).toEqual(['a', 'AltGraph', '@', 'Enter', 'Tab', 'b']);
    expect(plan.actions.filter((action) => action.key === 'AltGraph')).toHaveLength(2);
  });

  it('makes a typo now and then when asked, notices it, and takes it back', () => {
    const clumsy = { ...profile, typing: { ...profile.typing, typoRate: 0.2 } };
    const plan = planTyping(text.repeat(3), usLayout, clumsy, stream('typos'), { typos: true });
    expect(plan.typos).toBeGreaterThan(8);
    const presses = plan.actions.filter(
      (action) => action.action === 'down' && action.key !== 'Shift',
    );
    expect(presses.filter((action) => action.key === 'Backspace')).toHaveLength(plan.typos);
    // What is left after the corrections is the text.
    const typed: string[] = [];
    for (const press of presses) {
      if (press.key === 'Backspace') {
        typed.pop();
      } else {
        typed.push(press.key);
      }
    }
    expect(typed.join('')).toBe(text.repeat(3));
    // Noticing takes longer than typing; the cadence around it stays a typist's.
    for (const interval of keyIntervals(plan.actions)) {
      expect(interval).toBeLessThanOrEqual(HUMAN_BOUNDS.keyIntervalMs[1] + 0.11);
    }
    const careful = planTyping(text, usLayout, clumsy, stream('typos'), { typos: false });
    expect(careful.typos).toBe(0);
  });
});

describe('scrolling and dwelling', () => {
  const profile = motorProfile(seed);

  it('turns the wheel a few notches at a time, never much further than is left', () => {
    const random = stream('flicks');
    const flicks = Array.from({ length: 300 }, () => planFlick('down', 4000, 114, profile, random));
    const counts = flicks.map((flick) => flick.gaps.length);
    expect(Math.min(...counts)).toBeGreaterThanOrEqual(1);
    expect(Math.max(...counts)).toBeLessThanOrEqual(6);
    const mean = counts.reduce((sum, value) => sum + value, 0) / counts.length;
    expect(Math.abs(mean - profile.wheel.notches)).toBeLessThan(0.6);
    for (const flick of flicks) {
      expect(flick.gaps[0]).toBe(0);
      for (const gap of flick.gaps.slice(1)) {
        expect(gap).toBeGreaterThanOrEqual(profile.wheel.interval[0]);
        expect(gap).toBeLessThanOrEqual(profile.wheel.interval[1]);
      }
    }
    expect(planFlick('up', 30, 114, profile, random).gaps).toHaveLength(1);
    expect(planFlick('down', 230, 114, profile, random).gaps.length).toBeLessThanOrEqual(3);
  });

  it('rests longer over more text, within bounds', () => {
    const mean = (words: number) => {
      const random = stream(`pause-${words}`);
      const pauses = Array.from({ length: 200 }, () => readingPause(words, profile, random));
      for (const pause of pauses) {
        expect(pause).toBeGreaterThanOrEqual(220);
        expect(pause).toBeLessThanOrEqual(6000);
      }
      return pauses.reduce((sum, value) => sum + value, 0) / pauses.length;
    };
    const few = mean(20);
    const some = mean(60);
    const many = mean(150);
    expect(some).toBeGreaterThan(few * 1.5);
    expect(many).toBeGreaterThan(some * 1.5);
    // Proportional: twice the words is about twice the reading time.
    const reading = (words: number) =>
      (words / profile.reading.wordsPerMinute) * 60_000 * profile.reading.attention;
    expect(
      Math.abs(many - few - (reading(150) - reading(20))) / (reading(150) - reading(20)),
    ).toBeLessThan(0.25);
    expect(readingPause(100_000, profile, stream('long'))).toBe(6000);
  });

  it('scrolls back as often as the persona does', () => {
    const random = stream('back');
    const share =
      Array.from({ length: 2000 }, () => scrollsBack(profile, random)).filter(Boolean).length /
      2000;
    expect(Math.abs(share - profile.reading.scrollBack)).toBeLessThan(0.03);
  });

  it('dwells around six seconds, log-normally, never longer than the cap', () => {
    for (const each of SEEDS.slice(0, 6)) {
      const hand = motorProfile(each);
      const random = stream('dwell', each);
      const dwells = Array.from({ length: 4000 }, () => drawDwell(hand, random)).sort(
        (a, b) => a - b,
      );
      const median = dwells[2000] ?? 0;
      expect(Math.abs(median - hand.dwell.median) / hand.dwell.median).toBeLessThan(0.08);
      expect(dwells.at(-1)).toBeLessThanOrEqual(HUMAN_BOUNDS.dwellCapMs);
      expect(dwells[0]).toBeGreaterThanOrEqual(600);
      // A long tail to the right, as a log-normal has.
      expect((dwells[3600] ?? 0) - median).toBeGreaterThan(median - (dwells[400] ?? 0));
    }
  });
});

describe('the pace of a flow', () => {
  it('gives every persona a tempo of its own, always the same, and leaves its hand alone', () => {
    const [quickest, slowest] = HUMAN_BOUNDS.thinkingFactor;
    const medians = SEEDS.map((each) => tempo(each).thinking.median);
    for (const [index, each] of SEEDS.entries()) {
      expect(tempo(each)).toEqual(tempo(each));
      expect(medians[index]).toBeGreaterThanOrEqual(HUMAN_BOUNDS.thinkingBaseMs * quickest);
      expect(medians[index]).toBeLessThanOrEqual(HUMAN_BOUNDS.thinkingBaseMs * slowest);
    }
    // Quick ones and slow ones: the twenty are spread over most of the range.
    expect(new Set(medians).size).toBeGreaterThan(15);
    expect(Math.max(...medians) - Math.min(...medians)).toBeGreaterThan(
      HUMAN_BOUNDS.thinkingBaseMs * (slowest - quickest) * 0.6,
    );
    // The tempo has a stream of its own: the motor profile and its id stay what they were.
    expect(TEMPO_LABEL).toBe('tempo-v1');
    expect(TEMPO_LABEL).not.toBe(MOTOR_PROFILE_LABEL);
    expect(DEFAULT_TEMPO.thinking.median).toBe(HUMAN_BOUNDS.thinkingBaseMs);
  });

  it('thinks around its own median, never outside the bounds, and hardly ever as long as the time before', () => {
    const [shortest, longest] = HUMAN_BOUNDS.thinkingMs;
    for (const each of SEEDS.slice(0, 6)) {
      const own = tempo(each);
      const random = stream('thinking', each);
      const drawn = Array.from({ length: 4000 }, () => drawThinking(own, random));
      const times = [...drawn].sort((a, b) => a - b);
      expect(times[0]).toBeGreaterThanOrEqual(shortest);
      expect(times.at(-1)).toBeLessThanOrEqual(longest);
      expect(Math.abs((times[2000] ?? 0) - own.thinking.median) / own.thinking.median).toBeLessThan(
        0.08,
      );
      // A long tail to the right, and nothing piled up at either bound.
      expect((times[3600] ?? 0) - (times[2000] ?? 0)).toBeGreaterThan(
        (times[2000] ?? 0) - (times[400] ?? 0),
      );
      expect(times.filter((time) => time === shortest || time === longest).length).toBeLessThan(4);
      expect(drawn.filter((time, index) => time === drawn[index - 1]).length).toBeLessThan(40);
    }
    // Bounds no draw can meet end in the nearest bound, not in a loop.
    const impossible = { thinking: { median: 1_000_000, sigma: 0.01 } };
    expect(drawThinking(impossible, stream('thinking'))).toBe(longest);
    expect(drawThinking({ thinking: { median: 1, sigma: 0.01 } }, stream('thinking'))).toBe(
      shortest,
    );
  });

  it('draws a dwell around the median it is given, and again when it falls below the floor', () => {
    const hand = motorProfile(seed);
    const draw = (pace: { median: number | null; floor: number }, count = 4000) => {
      const random = stream('paced-dwell');
      return Array.from({ length: count }, () => drawPacedDwell(pace, hand, random)).sort(
        (a, b) => a - b,
      );
    };
    const free = draw({ median: 5000, floor: 0 });
    expect(Math.abs((free[2000] ?? 0) - 5000) / 5000).toBeLessThan(0.05);
    // The 95th percentile is twice the median.
    expect(Math.abs((free[3800] ?? 0) - 10_000) / 10_000).toBeLessThan(0.08);
    expect(PACED_DWELL_SIGMA).toBeCloseTo(0.4214, 3);
    const floored = draw({ median: 5000, floor: 4000 });
    expect(floored[0]).toBeGreaterThanOrEqual(4000);
    expect(floored.filter((dwell) => dwell === 4000).length).toBeLessThan(8);
    // Without a median it is the persona's own dwell, capped as ever.
    const own = draw({ median: null, floor: 0 });
    expect(Math.abs((own[2000] ?? 0) - hand.dwell.median) / hand.dwell.median).toBeLessThan(0.08);
    expect(own.at(-1)).toBeLessThanOrEqual(HUMAN_BOUNDS.dwellCapMs);
    // A floor above the cap is never reached by a draw: it is used itself.
    expect(new Set(draw({ median: null, floor: 60_000 }, 5))).toEqual(new Set([60_000]));
    expect(new Set(draw({ median: 0, floor: 0 }, 5))).toEqual(new Set([0]));
    expect(MAX_REDRAWS).toBe(64);
  });
});

/**
 * A mouse, a keyboard and a clock that only write down what was asked of
 * them. `dark` is a spell of the rig's time, from its start, in which the
 * page draws nothing: the wheel turned then moves no page. `glideMs` is
 * how long the page takes to get where a notch sent it, as a browser's
 * smooth scrolling glides there (0: at once).
 */
function rig(
  options: {
    scrollHeight?: number;
    boxes?: Record<string, Box>;
    notchPx?: number;
    dark?: readonly [number, number];
    glideMs?: number;
  } = {},
) {
  const log: string[] = [];
  const paints: number[] = [];
  let now = 1000;
  const dark = options.dark;
  const drawing = () => dark === undefined || now - 1000 < dark[0] || now - 1000 >= dark[1];
  let pointer: Point = { x: 960, y: 540 };
  // Where the wheel sent the page, and where it was when the last notch
  // came: the page shows a point between the two until the glide is over.
  let target = 0;
  let glideFrom = 0;
  let glideStart = 0;
  const glideMs = options.glideMs ?? 0;
  const shown = () => {
    const done = glideMs === 0 ? 1 : Math.min(1, (now - glideStart) / glideMs);
    return Math.round(glideFrom + (target - glideFrom) * done);
  };
  const viewport = { width: 1200, height: 800 };
  const scrollHeight = options.scrollHeight ?? 4000;
  const notch = options.notchPx ?? 114;
  const boxes = options.boxes ?? {};
  const clock: Clock = {
    now: () => now,
    sleep: (ms) => {
      now += ms;
      return Promise.resolve();
    },
  };
  const device: InputDevice = {
    screen: { width: 1920, height: 1080 },
    pointerPosition: () => Promise.resolve(pointer),
    pointerMove: (x, y) => {
      pointer = { x, y };
      log.push(`move ${x},${y} @${Math.round(now)}`);
      return Promise.resolve();
    },
    buttonDown: (button) => {
      log.push(`down ${button} @${Math.round(now)}`);
      return Promise.resolve();
    },
    buttonUp: (button) => {
      log.push(`up ${button} @${Math.round(now)}`);
      return Promise.resolve();
    },
    wheelNotch: (direction) => {
      if (drawing()) {
        glideFrom = shown();
        glideStart = now;
        target = Math.max(
          0,
          Math.min(
            scrollHeight - viewport.height,
            target + (direction === 'down' ? notch : -notch),
          ),
        );
      }
      log.push(`wheel ${direction}`);
      return Promise.resolve();
    },
    levelsOf: usLayout,
    keyDown: (key) => {
      log.push(`key ${key}`);
      return Promise.resolve();
    },
    keyUp: (key) => {
      log.push(`keyup ${key}`);
      return Promise.resolve();
    },
    flush: () => Promise.resolve(),
    close: () => Promise.resolve(),
  };
  const metrics = (): SurfaceMetrics => ({
    origin: { x: 0, y: 90 },
    viewport,
    scrollY: shown(),
    scrollHeight,
  });
  const surface: PageSurface = {
    metrics: () => Promise.resolve(metrics()),
    visibleWords: () => Promise.resolve(180),
    boxOf: (selector) => {
      const box = boxes[selector];
      return Promise.resolve(box === undefined ? null : { ...box, y: box.y - shown() });
    },
    painted: (limitMs) => {
      // Two frames of 16 ms, after the dark spell when they fall into it.
      let ready = now + 33;
      if (dark !== undefined && now - 1000 < dark[1] && ready - 1000 > dark[0]) {
        ready = Math.max(ready, 1000 + dark[1] + 33);
      }
      const drawn = ready - now <= limitMs;
      now = drawn ? ready : now + limitMs;
      paints.push(now - 1000);
      return Promise.resolve(drawn);
    },
  };
  const human = new Human({
    device,
    surface,
    profile: motorProfile(seed),
    random: stream('behavior:rig'),
    clock,
  });
  return {
    human,
    log,
    paints,
    pointer: () => pointer,
    scrollY: shown,
    elapsed: () => now - 1000,
    place: (point: Point) => {
      pointer = point;
    },
  };
}

describe('a persona at its mouse and keyboard', () => {
  it('travels to an element, rests, and clicks inside it', async () => {
    const box = { x: 700, y: 300, width: 140, height: 40 };
    const { human, log, pointer, elapsed } = rig({ boxes: { '#go': box } });
    await human.click('#go');
    // Screen coordinates: the viewport begins 90 pixels below the top of the screen.
    expect(pointer().x).toBeGreaterThan(box.x);
    expect(pointer().x).toBeLessThan(box.x + box.width);
    expect(pointer().y).toBeGreaterThan(box.y + 90);
    expect(pointer().y).toBeLessThan(box.y + box.height + 90);
    const moves = log.filter((entry) => entry.startsWith('move')).length;
    expect(moves).toBeGreaterThan(8);
    expect(log.slice(-2).map((entry) => entry.split(' @')[0])).toEqual(['down left', 'up left']);
    const stamp = (entry: string | undefined) => Number(entry?.split('@')[1]);
    // A rest before the click, a button held for a moment.
    expect(stamp(log.at(-2)) - stamp(log.at(-3))).toBeGreaterThan(60);
    expect(stamp(log.at(-1)) - stamp(log.at(-2))).toBeGreaterThan(34);
    expect(human.counts).toMatchObject({ moves: 1, clicks: 1, positions: moves });
    expect(elapsed()).toBeGreaterThan(300);
  });

  it('scrolls an element into view with the wheel before going to it', async () => {
    const { human, log, scrollY } = rig({
      boxes: { '#deep': { x: 100, y: 2600, width: 200, height: 40 } },
    });
    await human.hover('#deep');
    expect(log.filter((entry) => entry === 'wheel down').length).toBeGreaterThan(10);
    expect(2600 - scrollY()).toBeGreaterThan(0);
    expect(2600 - scrollY()).toBeLessThan(800);
    expect(human.counts.notches).toBe(log.filter((entry) => entry.startsWith('wheel')).length);
  });

  it('goes to an element where the page comes to rest, not where it glides past (F66)', async () => {
    // A slow machine: the page takes a second and a half to get where a
    // flick sent it, far longer than the hand's own pause after the wheel.
    const on = (pointer: Point, box: Box, scrollY: number) =>
      pointer.x > box.x &&
      pointer.x < box.x + box.width &&
      pointer.y - 90 > box.y - scrollY &&
      pointer.y - 90 < box.y - scrollY + box.height;
    const deep = { x: 100, y: 2600, width: 200, height: 40 };
    const down = rig({ boxes: { '#deep': deep }, glideMs: 1500 });
    await down.human.hover('#deep');
    expect(on(down.pointer(), deep, down.scrollY())).toBe(true);

    // A reader who scrolled down and goes back up to a link near the top,
    // the page still gliding from the reading when the hand sets out.
    const near = { x: 300, y: 420, width: 160, height: 22 };
    const back = rig({ boxes: { '#next': near }, glideMs: 1500, scrollHeight: 6000 });
    expect(await back.human.scrollTo(0.5)).toBe('depth');
    await back.human.hover('#next');
    expect(on(back.pointer(), near, back.scrollY())).toBe(true);
  });

  it('says so when there is no such element, or it cannot be reached', async () => {
    const { human } = rig({
      boxes: { '#below': { x: 0, y: 9000, width: 10, height: 10 } },
      scrollHeight: 1200,
    });
    await expect(human.click('#missing')).rejects.toMatchObject({ reason: 'element_not_found' });
    const stuck: unknown = await human.hover('#below').catch((error: unknown) => error);
    expect(stuck).toBeInstanceOf(BehaviorError);
    expect(stuck).toMatchObject({ reason: 'element_not_reachable' });
  });

  it('scrolls to a depth in flicks with rests, moving the mouse first, sometimes back', async () => {
    const { human, log, scrollY, elapsed } = rig({ scrollHeight: 12_000 });
    await human.scrollTo(0.8);
    const target = (12_000 - 800) * 0.8;
    expect(scrollY()).toBeGreaterThanOrEqual(target - 40);
    expect(scrollY()).toBeLessThan(target + 114 * 6);
    // A hand touches the mouse before it turns the wheel.
    expect(log[0]).toMatch(/^move/);
    expect(log.indexOf('wheel down')).toBeGreaterThan(0);
    expect(log.filter((entry) => entry === 'wheel up').length).toBeGreaterThan(0);
    // Reading takes time: far more than the wheel alone.
    expect(elapsed()).toBeGreaterThan(20_000);
    expect(human.counts.notches).toBe(log.filter((entry) => entry.startsWith('wheel')).length);
  });

  it('stops scrolling at a deadline, at the end of the page, and on a page that does not scroll', async () => {
    const limited = rig({ scrollHeight: 30_000 });
    await limited.human.scrollTo(1, 1000 + 4000);
    expect(limited.scrollY()).toBeLessThan(6000);
    // It ends at the deadline, give or take the flick that was under way:
    // a rest of several seconds is not sat out after it.
    expect(limited.elapsed()).toBeGreaterThan(3000);
    expect(limited.elapsed()).toBeLessThan(4000 + 1500);
    // The same hand, given more time, scrolls further.
    const longer = rig({ scrollHeight: 30_000 });
    await longer.human.scrollTo(1, 1000 + 12_000);
    expect(longer.scrollY()).toBeGreaterThan(limited.scrollY());
    expect(longer.elapsed()).toBeLessThan(12_000 + 1500);
    const short = rig({ scrollHeight: 800 });
    await short.human.scrollTo(1);
    expect(short.human.counts.notches).toBe(0);
    const frozen = rig({ scrollHeight: 5000, notchPx: 0 });
    await frozen.human.scrollTo(0.5);
    // One flick, and one more once the page has drawn.
    expect(frozen.human.counts.notches).toBeLessThanOrEqual(12);
    expect(frozen.paints).toHaveLength(2);
  });

  it('turns the wheel once the page draws, and again when a flick moved nothing', async () => {
    // A new document draws nothing for its first 700 ms: the hand waits
    // for it before it touches the mouse.
    const late = rig({ scrollHeight: 12_000, dark: [0, 700] });
    expect(await late.human.scrollTo(0.8)).toBe('depth');
    expect(late.paints[0]).toBe(733);
    expect(late.scrollY()).toBeGreaterThanOrEqual((12_000 - 800) * 0.8 - 40);
    // A page that stops drawing after it was first seen loses the first
    // flick; the hand waits for it to draw and goes on to the depth.
    const stalled = rig({ scrollHeight: 12_000, dark: [100, 1500] });
    expect(await stalled.human.scrollTo(0.8)).toBe('depth');
    expect(stalled.paints.slice(0, 2)).toEqual([33, 1533]);
    expect(stalled.scrollY()).toBeGreaterThanOrEqual((12_000 - 800) * 0.8 - 40);
    // A page that never draws is waited for twice, at most two seconds
    // each time, and then it is a page that does not scroll.
    const dead = rig({ scrollHeight: 12_000, dark: [0, 3_600_000] });
    expect(await dead.human.scrollTo(0.8)).toBe('page');
    expect(dead.paints).toHaveLength(2);
    expect(dead.paints[0]).toBe(2000);
    expect((dead.paints[1] ?? 0) - 2000).toBeGreaterThanOrEqual(2000);
    expect(dead.scrollY()).toBe(0);
  });

  it('says why the scrolling ended: the depth, the deadline, or the page', async () => {
    // The depth asked was reached.
    const read = rig({ scrollHeight: 12_000 });
    expect(await read.human.scrollTo(0.8)).toBe('depth');
    expect(read.scrollY()).toBeGreaterThanOrEqual((12_000 - 800) * 0.8 - 40);
    // Already there: no wheel is turned, and it is the depth all the same.
    const notches = read.human.counts.notches;
    expect(await read.human.scrollTo(0.5)).toBe('depth');
    expect(read.human.counts.notches).toBe(notches);

    // The time ran out first, well short of the depth.
    const hurried = rig({ scrollHeight: 30_000 });
    expect(await hurried.human.scrollTo(0.9, 1000 + 5000)).toBe('deadline');
    expect(hurried.scrollY()).toBeGreaterThan(0);
    expect(hurried.scrollY()).toBeLessThan((30_000 - 800) * 0.5);
    expect(hurried.elapsed()).toBeLessThan(5000 + 1500);
    // A deadline that has passed before the first flick turns no wheel.
    const late = rig({ scrollHeight: 30_000 });
    expect(await late.human.scrollTo(0.9, 0)).toBe('deadline');
    expect(late.human.counts.notches).toBe(0);
    // A depth that the last flick before the deadline reached counts as
    // reached: the page was read as far as was asked.
    const justInTime = rig({ scrollHeight: 2400 });
    const needed = rig({ scrollHeight: 2400 });
    expect(await needed.human.scrollTo(0.5)).toBe('depth');
    // The same hand on the same page, with a deadline inside its last rest.
    expect(await justInTime.human.scrollTo(0.5, 1000 + needed.elapsed() - 100)).toBe('depth');
    expect(justInTime.scrollY()).toBe(needed.scrollY());
    expect(justInTime.elapsed()).toBeLessThan(needed.elapsed());

    // The page: one that just fills its window, one shorter than it, and
    // one that does not move for a wheel.
    const whole = rig({ scrollHeight: 800 });
    expect(await whole.human.scrollTo(0.9)).toBe('page');
    expect(whole.human.counts.notches).toBe(0);
    const shorter = rig({ scrollHeight: 640 });
    expect(await shorter.human.scrollTo(0.9)).toBe('page');
    const frozen = rig({ scrollHeight: 5000, notchPx: 0 });
    expect(await frozen.human.scrollTo(0.5)).toBe('page');
    expect(frozen.scrollY()).toBe(0);
    // The time given does not change what the page is.
    const frozenInTime = rig({ scrollHeight: 5000, notchPx: 0 });
    expect(await frozenInTime.human.scrollTo(0.5, 1000 + 60_000)).toBe('page');
  });

  it('splits a click into its two halves for a caller that looks in between', async () => {
    const box = { x: 700, y: 300, width: 140, height: 40 };
    const whole = rig({ boxes: { '#go': box } });
    await whole.human.click('#go');
    const halves = rig({ boxes: { '#go': box } });
    await halves.human.hover('#go');
    expect(halves.log.some((entry) => entry.startsWith('down'))).toBe(false);
    expect(halves.human.counts.clicks).toBe(0);
    await halves.human.pressButton();
    // The same hand and the same seeds: the two halves are the click, move for move.
    expect(halves.log).toEqual(whole.log);
    expect(halves.human.counts).toEqual(whole.human.counts);
  });

  it('types key by key at its pace, and presses a key with its modifiers', async () => {
    const { human, log, elapsed } = rig();
    await human.type('Hi you', { typos: false });
    expect(log).toEqual(
      expect.arrayContaining(['key Shift', 'key H', 'keyup Shift', 'key u', 'keyup u']),
    );
    expect(log.indexOf('key Shift')).toBeLessThan(log.indexOf('key H'));
    expect(human.counts.keys).toBe(7);
    expect(elapsed()).toBeGreaterThan(5 * 60);
    const before = log.length;
    await human.press('A');
    expect(log.slice(before)).toEqual(['key Shift', 'key A', 'keyup A', 'keyup Shift']);
    await human.chord(['Alt'], 'ArrowLeft');
    expect(log.slice(-4)).toEqual(['key Alt', 'key ArrowLeft', 'keyup ArrowLeft', 'keyup Alt']);
    expect(human.counts.keys).toBe(7 + 2 + 2);
  });

  it('dwells for the time asked, with an idle hand that moves a little, or does not', async () => {
    const active = rig();
    await active.human.dwell(40_000, { wander: true });
    expect(active.elapsed()).toBeGreaterThanOrEqual(40_000);
    expect(active.elapsed()).toBeLessThan(42_000);
    expect(active.human.counts.wanders).toBeGreaterThanOrEqual(2);
    expect(active.log.every((entry) => entry.startsWith('move'))).toBe(true);
    const still = rig();
    await still.human.dwell(9000, { wander: false });
    expect(still.elapsed()).toBe(9000);
    expect(still.log).toEqual([]);
    expect(still.human.drawDwell()).toBeGreaterThan(599);
  });
});
