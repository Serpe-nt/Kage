import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';

import { EngineError } from '@kage/engine-core';

/** First display number the sidecar uses; low numbers belong to a desktop session. */
export const FIRST_DISPLAY = 100;

/** How many display numbers are tried before giving up. */
const DISPLAY_RANGE = 200;

/** How long an X server may take to create its socket. */
const START_TIMEOUT_MS = 10_000;

const socketOf = (display: number) => `/tmp/.X11-unix/X${display}`;
const lockOf = (display: number) => `/tmp/.X${display}-lock`;

/** A running virtual display. */
export interface VirtualDisplay {
  /** The value of `DISPLAY`, for example `:100`. */
  readonly name: string;
  /** Stops the X server. Safe to call more than once. */
  stop(): Promise<void>;
}

/** The geometry of a display in physical pixels. */
export interface DisplayGeometry {
  readonly width: number;
  readonly height: number;
  readonly depth: number;
}

/**
 * Hands out display numbers. Every persona context gets its own X server
 * with the persona's screen size (spec §10.3), so the number is taken for as
 * long as the context lives.
 */
export class DisplayPool {
  private readonly taken = new Set<number>();

  /** @param isFree whether a display number is unused on this machine; injectable for tests. */
  constructor(
    private readonly isFree: (display: number) => boolean = (display) =>
      !existsSync(socketOf(display)) && !existsSync(lockOf(display)),
  ) {}

  /**
   * Reserves a display number.
   *
   * @throws {EngineError} `browser_launch_failed` when none is free.
   */
  acquire(): number {
    for (let display = FIRST_DISPLAY; display < FIRST_DISPLAY + DISPLAY_RANGE; display += 1) {
      if (!this.taken.has(display) && this.isFree(display)) {
        this.taken.add(display);
        return display;
      }
    }
    throw new EngineError('browser_launch_failed', 'no free X display number');
  }

  /** Gives a display number back. */
  release(display: number): void {
    this.taken.delete(display);
  }
}

function stopProcess(child: ChildProcess): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    const force = setTimeout(() => child.kill('SIGKILL'), 2000);
    child.once('exit', () => {
      clearTimeout(force);
      resolve();
    });
    child.kill('SIGTERM');
  });
}

/**
 * Starts an X virtual framebuffer with one screen of the given geometry and
 * waits until it accepts clients.
 *
 * The server listens on its Unix socket only, never on TCP.
 *
 * @throws {EngineError} `browser_launch_failed` when Xvfb is missing, exits
 *   or does not come up in time.
 */
export async function startXvfb(
  display: number,
  geometry: DisplayGeometry,
): Promise<VirtualDisplay> {
  const name = `:${display}`;
  const screen = `${geometry.width}x${geometry.height}x${geometry.depth}`;
  const child = spawn('Xvfb', [name, '-screen', '0', screen, '-nolisten', 'tcp', '-noreset'], {
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (chunk: Buffer) => {
    stderr = (stderr + chunk.toString('utf8')).slice(-2000);
  });

  const failure = new Promise<never>((_resolve, reject) => {
    child.once('error', (error) => {
      reject(
        new EngineError('browser_launch_failed', `Xvfb could not be started: ${error.message}`, {
          cause: error,
        }),
      );
    });
    child.once('exit', (code, signal) => {
      reject(
        new EngineError(
          'browser_launch_failed',
          `Xvfb ${name} (${screen}) exited with ${code ?? signal ?? 'no status'}: ${stderr.trim().split('\n').at(-1) ?? ''}`,
        ),
      );
    });
  });
  // The rejection is observed below or after the server is up; an exit
  // after a clean stop must not become an unhandled rejection.
  failure.catch(() => undefined);

  const ready = (async () => {
    const deadline = Date.now() + START_TIMEOUT_MS;
    while (!existsSync(socketOf(display))) {
      if (Date.now() > deadline) {
        throw new EngineError(
          'browser_launch_failed',
          `Xvfb ${name} did not come up within ${START_TIMEOUT_MS} ms`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
  })();

  try {
    await Promise.race([ready, failure]);
  } catch (error) {
    await stopProcess(child);
    throw error;
  }
  return { name, stop: () => stopProcess(child) };
}
