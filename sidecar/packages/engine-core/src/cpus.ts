import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { availableParallelism } from 'node:os';

/**
 * The CPUs of the host and of a persona. A browser reports as many logical
 * processors as its process may run on, so a persona's count is made true
 * by a CPU affinity mask (ADR 0009); both engines use the same rule.
 */

/**
 * Reads a CPU list as the kernel writes it (`0-3,8,10-11`).
 *
 * @throws {TypeError} when the text is not such a list.
 */
export function parseCpuList(text: string): number[] {
  const cpus = new Set<number>();
  for (const part of text.trim().split(',')) {
    const match = /^(\d+)(?:-(\d+))?$/.exec(part.trim());
    if (match?.[1] === undefined) {
      throw new TypeError(`not a CPU list: ${JSON.stringify(text)}`);
    }
    const first = Number(match[1]);
    const last = match[2] === undefined ? first : Number(match[2]);
    if (last < first) {
      throw new TypeError(`not a CPU list: ${JSON.stringify(text)}`);
    }
    for (let cpu = first; cpu <= last; cpu += 1) {
      cpus.add(cpu);
    }
  }
  return [...cpus].sort((a, b) => a - b);
}

/** Writes CPU ids as a list `taskset --cpu-list` reads: `0-3,8`. */
export function formatCpuList(cpus: readonly number[]): string {
  const sorted = [...new Set(cpus)].sort((a, b) => a - b);
  const ranges: string[] = [];
  for (let index = 0; index < sorted.length; ) {
    const first = sorted[index] ?? 0;
    let end = index;
    while (sorted[end + 1] === (sorted[end] ?? 0) + 1) {
      end += 1;
    }
    const last = sorted[end] ?? first;
    ranges.push(first === last ? String(first) : `${first}-${last}`);
    index = end + 1;
  }
  return ranges.join(',');
}

/**
 * The CPUs this process may run on: the affinity mask the container was
 * given (`/proc/self/status`), or, where that cannot be read, as many CPUs
 * as Node counts.
 */
export function allowedCpus(statusText?: string): number[] {
  const text =
    statusText ??
    (existsSync('/proc/self/status') ? readFileSync('/proc/self/status', 'utf8') : '');
  const line = /^Cpus_allowed_list:\s*(\S+)/m.exec(text)?.[1];
  if (line !== undefined) {
    try {
      const cpus = parseCpuList(line);
      if (cpus.length > 0) {
        return cpus;
      }
    } catch {
      // Fall through to Node's own count.
    }
  }
  return Array.from({ length: availableParallelism() }, (_unused, index) => index);
}

/**
 * The CPUs of a persona: as many as its fingerprint says, at most as many
 * as the host has, starting at a position derived from the seed so that the
 * personas of one host do not all sit on the first CPUs.
 */
export function personaCpus(seed: string, wanted: number, hostCpus: readonly number[]): number[] {
  const count = Math.max(1, Math.min(wanted, hostCpus.length));
  const start =
    createHash('sha256').update('kage-cpus-v1').update(seed).digest().readUInt32LE(0) %
    hostCpus.length;
  return Array.from(
    { length: count },
    (_unused, index) => hostCpus[(start + index) % hostCpus.length] ?? 0,
  ).sort((a, b) => a - b);
}
