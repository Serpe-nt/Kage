import { createHash } from 'node:crypto';

/**
 * Seeded, reproducible randomness for the sidecar (house rule: randomness
 * that affects behavior or fingerprints is seeded and the seed is recorded).
 *
 * The generator is xoshiro256**, a named algorithm with a fixed output per
 * seed. Its state comes from SHA-256 of a tag, a label and the seed, so one
 * recorded seed drives independent streams for independent purposes.
 */

const MASK = (1n << 64n) - 1n;
const TAG = 'kage-sidecar-seed-v1';
const SEED_PATTERN = /^[0-9a-f]{64}$/;

const rotl = (value: bigint, bits: bigint): bigint =>
  ((value << bits) | (value >> (64n - bits))) & MASK;

/** A deterministic stream of numbers for one seed and one purpose. */
export class SeededRandom {
  private readonly state: [bigint, bigint, bigint, bigint];

  /**
   * @param seed 64 lower-case hexadecimal characters, as Kage records seeds.
   * @param label what the stream is for; different labels give unrelated streams.
   * @throws when the seed is not 64 hexadecimal characters.
   */
  constructor(seed: string, label: string) {
    if (!SEED_PATTERN.test(seed)) {
      throw new TypeError('a seed is 64 lower-case hexadecimal characters');
    }
    const digest = createHash('sha256')
      .update(TAG)
      .update('\0')
      .update(label)
      .update('\0')
      .update(seed)
      .digest();
    this.state = [
      digest.readBigUInt64LE(0),
      digest.readBigUInt64LE(8),
      digest.readBigUInt64LE(16),
      digest.readBigUInt64LE(24),
    ];
  }

  /** The next 64 random bits. */
  nextUint64(): bigint {
    const [s0, s1, s2, s3] = this.state;
    const result = (rotl((s1 * 5n) & MASK, 7n) * 9n) & MASK;
    const shifted = (s1 << 17n) & MASK;
    const n2 = s2 ^ s0;
    const n3 = s3 ^ s1;
    this.state[1] = s1 ^ n2;
    this.state[0] = s0 ^ n3;
    this.state[2] = n2 ^ shifted;
    this.state[3] = rotl(n3, 45n);
    return result;
  }

  /** A float in `[0, 1)` with 53 random bits: a drop-in for `Math.random()`. */
  next(): number {
    return Number(this.nextUint64() >> 11n) / 2 ** 53;
  }

  /** An integer in `[min, max]`, both included. */
  integer(min: number, max: number): number {
    if (!Number.isInteger(min) || !Number.isInteger(max) || max < min) {
      throw new RangeError(`not an integer range: [${min}, ${max}]`);
    }
    return min + Math.floor(this.next() * (max - min + 1));
  }

  /** A float in `[min, max)`. */
  uniform(min: number, max: number): number {
    return min + this.next() * (max - min);
  }

  /** Whether an event of the given probability happens. */
  chance(probability: number): boolean {
    return this.next() < probability;
  }

  /** A draw from a normal distribution (Box-Muller; two numbers of the stream per draw). */
  normal(mean: number, deviation: number): number {
    // 1 - next() is in (0, 1]: the logarithm is finite.
    const radius = Math.sqrt(-2 * Math.log(1 - this.next()));
    return mean + deviation * radius * Math.cos(2 * Math.PI * this.next());
  }

  /** A draw from a log-normal distribution given by its median and the sigma of its logarithm. */
  logNormal(median: number, sigma: number): number {
    return median * Math.exp(this.normal(0, sigma));
  }
}

let swapped = false;

/**
 * Runs a synchronous function with `Math.random` replaced by a seeded
 * stream, and restores it afterwards whatever happens.
 *
 * This is how a library that draws from `Math.random()` and takes no seed
 * (the fingerprint generator) is made reproducible. The function must be
 * synchronous: nothing else runs between the swap and the restore, so no
 * other code ever sees the seeded generator. A nested or overlapping call is
 * refused instead of silently sharing one stream.
 *
 * @throws when called while another call is running, and whatever `run` throws.
 */
export function withSeededMathRandom<T>(seed: string, label: string, run: () => T): T {
  if (swapped) {
    throw new Error('withSeededMathRandom is already running: seeded sections must not nest');
  }
  const random = new SeededRandom(seed, label);
  const original = Math.random;
  swapped = true;
  Math.random = () => random.next();
  try {
    const result = run();
    if (result instanceof Promise) {
      throw new TypeError('withSeededMathRandom takes a synchronous function');
    }
    return result;
  } finally {
    Math.random = original;
    swapped = false;
  }
}
