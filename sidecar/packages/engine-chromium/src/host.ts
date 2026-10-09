import { execFile } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { availableParallelism } from 'node:os';
import { promisify } from 'node:util';

import { EngineError } from '@kage/engine-core';

const run = promisify(execFile);

/** Where the Chrome package of the image installs its binary. */
export const DEFAULT_CHROME_PATH = '/opt/google/chrome/chrome';

/** What the engine needs to know about the machine it runs on. */
export interface HostFacts {
  /** The Chrome binary. */
  readonly chromePath: string;
  /** Its full version, for example `155.0.8059.39`. */
  readonly chromeVersion: string;
  /** Ids of the CPUs this process may run on, ascending. */
  readonly cpus: readonly number[];
}

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
  for (let index = 0; index < sorted.length;) {
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

/** The version in the output of `chrome --version` (`Google Chrome 155.0.8059.39`). */
export function parseChromeVersion(output: string): string | null {
  return /\b(\d+\.\d+\.\d+\.\d+)\b/.exec(output)?.[1] ?? null;
}

/**
 * The version of the installed Chrome, asked from the binary itself: the
 * version is a fact about the browser, never about a generator or a file
 * that could be out of date.
 *
 * @throws {EngineError} `browser_launch_failed` when there is no working Chrome.
 */
export async function detectChromeVersion(chromePath: string): Promise<string> {
  try {
    const { stdout } = await run(chromePath, ['--version'], { timeout: 15_000 });
    const version = parseChromeVersion(stdout);
    if (version !== null) {
      return version;
    }
    throw new Error(`unexpected output: ${stdout.trim().slice(0, 100)}`);
  } catch (error) {
    throw new EngineError(
      'browser_launch_failed',
      `no working Chrome at ${chromePath}: ${error instanceof Error ? error.message : String(error)}. Install the pinned Chrome (\`just setup-chrome\`) or set SIDECAR_CHROME_PATH.`,
      { cause: error },
    );
  }
}

/** Collects the facts about this machine. */
export async function detectHost(chromePath: string = DEFAULT_CHROME_PATH): Promise<HostFacts> {
  return { chromePath, chromeVersion: await detectChromeVersion(chromePath), cpus: allowedCpus() };
}

/**
 * The `User-Agent` of desktop Chrome on Linux for a major version. Chrome
 * reduces its user agent to this fixed form; the engine compares it with
 * what the running browser reports and never sets it.
 */
export function linuxChromeUserAgent(major: number): string {
  return `Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/${major}.0.0.0 Safari/537.36`;
}

/**
 * Chrome's client-hint brand list for a major version: the browser, the
 * engine and one made-up brand whose name, version and position Chromium
 * derives from the major alone. The same computation as
 * `kage_http::profile::chrome_brands`; a test pins both to what real Chrome
 * 155 sends.
 */
export function chromeBrands(major: number): { brand: string; version: string }[] {
  const orders = [
    [0, 1, 2],
    [0, 2, 1],
    [1, 0, 2],
    [1, 2, 0],
    [2, 0, 1],
    [2, 1, 0],
  ] as const;
  const separators = [' ', '(', ':', '-', '.', '/', ')', ';', '=', '?', '_'] as const;
  const versions = ['8', '99', '24'] as const;
  const madeUp = {
    brand: `Not${separators[major % separators.length] ?? ' '}A${separators[(major + 1) % separators.length] ?? ' '}Brand`,
    version: versions[major % versions.length] ?? '8',
  };
  const order = orders[major % orders.length] ?? orders[0];
  const list: { brand: string; version: string }[] = [];
  list[order[0]] = madeUp;
  list[order[1]] = { brand: 'Chromium', version: String(major) };
  list[order[2]] = { brand: 'Google Chrome', version: String(major) };
  return list;
}
