import type { SeededRandom } from '../seeded-random.js';

import type { KeyLevels } from './device.js';
import { HUMAN_BOUNDS, type MotorProfile } from './profile.js';

/** One key going down or up, `t` milliseconds after typing began. */
export interface KeyAction {
  readonly t: number;
  readonly action: 'down' | 'up';
  /** One character, or a key name (`Shift`, `AltGraph`, `Backspace`, …). */
  readonly key: string;
}

/** A planned stretch of typing. */
export interface TypingPlan {
  /** Every key action in the order it is sent. */
  readonly actions: readonly KeyAction[];
  /** How many wrong characters are typed and taken back. */
  readonly typos: number;
  /** Milliseconds until the last key is up. */
  readonly duration: number;
}

/** What the planner asks of a keyboard: which modifiers a key needs. */
export type LevelsOf = (key: string) => KeyLevels;

/** Keys next to each letter on a keyboard, for typos a finger really makes. */
const NEIGHBOURS: Readonly<Record<string, string>> = {
  a: 'qsy',
  b: 'vghn',
  c: 'xdfv',
  d: 'serfcx',
  e: 'wrsd',
  f: 'drtgvc',
  g: 'ftzhbv',
  h: 'gzujnb',
  i: 'ujko',
  j: 'huikmn',
  k: 'jiolm',
  l: 'kop',
  m: 'njk',
  n: 'bhjm',
  o: 'iklp',
  p: 'ol',
  q: 'wa',
  r: 'edft',
  s: 'awedxy',
  t: 'rfgz',
  u: 'zhji',
  v: 'cfgb',
  w: 'qase',
  x: 'ysdc',
  y: 'asx',
  z: 'tghu',
};

const clamp = (value: number, min: number, max: number) => Math.max(min, Math.min(max, value));

/**
 * Milliseconds between a key going up and the same key going down again,
 * where the next character is on the key still held (F202): the finger
 * lifts it and presses again. Even after the longest hold (160 ms) the
 * second press stays inside the spec's 220 ms between two presses.
 */
const REPRESS_MS: readonly [number, number] = [15, 45];

/**
 * The physical key a character is typed on: the keyboard's own key, so
 * that every character on one key, on any of its levels, is one key (`5`
 * and `%`, `q` and `@` with AltGr; QA round three, browser-06). Where the
 * keyboard cannot say, a letter and its capital share one and other
 * characters are told apart by what they are.
 */
const physicalKey = (key: string, levels: KeyLevels) =>
  levels.keycode !== null
    ? `keycode:${levels.keycode}`
    : `char:${key.length === 1 ? key.toLowerCase() : key}`;

/**
 * Plans the keys of a text: when each goes down and up.
 *
 * Every interval between two key presses lies inside the spec's 60 to
 * 220 ms, around the persona's own median. A key stays down for the
 * persona's hold time, so a fast typist's keys overlap as real ones do.
 * `Shift` and `AltGr` are keys with their own timing, held across
 * neighbouring characters that need them. With `typos`, a letter is now
 * and then replaced by a neighbouring key, noticed after a moment, deleted
 * with `Backspace` and typed again.
 *
 * A key is up before it goes down again (F202): where the next character
 * is on the key still held, on any of its levels (a doubled letter, `5`
 * then `%`, `q` then `@`), its press waits until a moment after the
 * release, as a finger must lift the key first. A key pressed while it is
 * down sends nothing on a real keyboard, and the X display drops such a
 * press too. Only those presses move, and the moment is drawn only for
 * them, so a plan without one is the plan it was.
 */
export function planTyping(
  text: string,
  levelsOf: LevelsOf,
  profile: MotorProfile,
  random: SeededRandom,
  options: { readonly typos: boolean },
): TypingPlan {
  const [minInterval, maxInterval] = HUMAN_BOUNDS.keyIntervalMs;
  const actions: KeyAction[] = [];
  const held = { shift: false, altGr: false };
  // Not at zero: a modifier may have to go down before the first key.
  let now = random.uniform(100, 160);
  let typos = 0;
  // A modifier never changes before the previous key is down, or that key would get it.
  let lastPress = 0;
  // When each key last goes up (F202).
  const upAt = new Map<string, number>();

  const interval = () =>
    clamp(
      random.logNormal(profile.typing.interval, profile.typing.sigma),
      minInterval,
      maxInterval,
    );
  const hold = () => clamp(random.normal(profile.typing.hold, 14), 35, 160);
  const emit = (t: number, action: 'down' | 'up', key: string) => {
    actions.push({ t: Math.round(t * 10) / 10, action, key });
  };
  const modifier = (name: 'shift' | 'altGr', key: string, wanted: boolean) => {
    if (held[name] === wanted) {
      return;
    }
    if (wanted) {
      // The modifier goes down a little before the key it is for.
      emit(Math.max(lastPress, now - random.uniform(35, 95)), 'down', key);
    } else {
      emit(Math.max(lastPress, now - random.uniform(5, 30)), 'up', key);
    }
    held[name] = wanted;
  };
  const stroke = (key: string) => {
    const levels = levelsOf(key);
    const physical = physicalKey(key, levels);
    const up = upAt.get(physical);
    if (up !== undefined && now <= up) {
      now = up + random.uniform(REPRESS_MS[0], REPRESS_MS[1]);
    }
    modifier('shift', 'Shift', levels.shift);
    modifier('altGr', 'AltGraph', levels.altGr);
    emit(now, 'down', key);
    const release = now + hold();
    emit(release, 'up', key);
    upAt.set(physical, release);
    lastPress = now;
    return release;
  };

  let end = 0;
  for (const typed of text) {
    const code = typed.codePointAt(0);
    const character = code === 0x0a || code === 0x0d ? 'Enter' : code === 0x09 ? 'Tab' : typed;
    const wrong = options.typos ? NEIGHBOURS[character.toLowerCase()] : undefined;
    if (wrong !== undefined && random.chance(profile.typing.typoRate)) {
      const slip = wrong[random.integer(0, wrong.length - 1)] ?? character;
      typos += 1;
      end = Math.max(
        end,
        stroke(character === character.toLowerCase() ? slip : slip.toUpperCase()),
      );
      now += random.uniform(profile.typing.correctionPause[0], profile.typing.correctionPause[1]);
      end = Math.max(end, stroke('Backspace'));
      now += interval();
    }
    end = Math.max(end, stroke(character));
    now += interval();
  }
  if (held.shift) {
    emit(end + random.uniform(10, 60), 'up', 'Shift');
  }
  if (held.altGr) {
    emit(end + random.uniform(10, 60), 'up', 'AltGraph');
  }
  // Keys overlap, so releases are planned out of order; they are sent in time order.
  const ordered = actions
    .map((action, index) => ({ action, index }))
    .sort((a, b) => a.action.t - b.action.t || a.index - b.index)
    .map((entry) => entry.action);
  return { actions: ordered, typos, duration: ordered.at(-1)?.t ?? 0 };
}

/**
 * The intervals between consecutive key presses of a plan. Modifiers are
 * left out, and so is the pause before a `Backspace`: noticing a typo is
 * a reaction, not typing cadence.
 */
export function keyIntervals(actions: readonly KeyAction[]): number[] {
  const presses = actions.filter(
    (action) => action.action === 'down' && action.key !== 'Shift' && action.key !== 'AltGraph',
  );
  const intervals: number[] = [];
  for (let index = 1; index < presses.length; index += 1) {
    const current = presses[index];
    const previous = presses[index - 1];
    if (current !== undefined && previous !== undefined && current.key !== 'Backspace') {
      intervals.push(current.t - previous.t);
    }
  }
  return intervals;
}
