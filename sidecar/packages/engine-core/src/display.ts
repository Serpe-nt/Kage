import { execFile, spawn, type ChildProcess } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { promisify } from 'node:util';

import { EngineError } from './engine.js';
import { X11Connection } from './x11/connection.js';
import { announceWorkArea, type WorkArea } from './x11/workarea.js';

const run = promisify(execFile);

/** First display number the sidecar uses; low numbers belong to a desktop session. */
export const FIRST_DISPLAY = 100;

/** How many display numbers are tried before giving up. */
const DISPLAY_RANGE = 200;

/** How long an X server may take to create its socket. */
const START_TIMEOUT_MS = 10_000;

const socketOf = (display: number) => `/tmp/.X11-unix/X${display}`;
const lockOf = (display: number) => `/tmp/.X${display}-lock`;

/** How many numbers another X server took first are passed over before a start gives up. */
const TAKEN_RETRIES = 10;

/**
 * The live process that holds a display: the pid Xvfb wrote into its lock
 * file, or `null` while there is no lock or its process is gone (a server
 * that starts then takes the stale lock over).
 */
function holderOf(display: number): number | null {
  let pid: number;
  try {
    pid = Number(readFileSync(lockOf(display), 'utf8').trim());
  } catch {
    return null;
  }
  if (!Number.isInteger(pid) || pid <= 0) {
    return null;
  }
  try {
    process.kill(pid, 0);
    return pid;
  } catch (error) {
    // EPERM: alive, another user's.
    return (error as NodeJS.ErrnoException).code === 'EPERM' ? pid : null;
  }
}

/**
 * Another X server holds the display number that was asked for: one that
 * another process (another engine, another sidecar on the host) started
 * between the number's check and this one's start.
 */
class DisplayTaken extends EngineError {
  constructor(readonly display: number) {
    super('browser_launch_failed', `the display :${display} is another X server's`);
  }
}

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

  /**
   * Reserves a display number and starts an X server on it: the number this
   * process's pool knows to be free, and, when another process's server
   * took it first, the next one. Two pools know nothing of each other's
   * numbers: two vitest workers started `:100` at the same moment, and the
   * one whose Xvfb lost went on to prepare the other's display (a
   * `setxkbmap` there hung for its 10 s on a GitHub runner).
   *
   * @throws {EngineError} `browser_launch_failed` as {@link startXvfb} does,
   *   or when every number tried was another server's.
   */
  async start(
    geometry: DisplayGeometry,
    options: DisplayOptions = {},
  ): Promise<{ readonly number: number; readonly display: VirtualDisplay }> {
    for (let passed = 0; ; passed += 1) {
      const number = this.acquire();
      try {
        return { number, display: await startXvfb(number, geometry, options) };
      } catch (error) {
        // Given back either way: while another server holds it, its lock
        // keeps `acquire` off it, and the next number is tried.
        this.release(number);
        if (!(error instanceof DisplayTaken) || passed >= TAKEN_RETRIES) {
          throw error;
        }
      }
    }
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

/** What a display is given besides its size. */
export interface DisplayOptions {
  /**
   * The keyboard layout of the display, as the X keyboard extension names
   * it (`de`, `fr`, `gb`, …). The keys the behavior engine presses produce
   * this layout's characters, and `KeyboardEvent.code` relates to `key` as
   * on that keyboard. Absent: the server's default, `us`.
   */
  readonly layout?: string;
  /**
   * The part of the screen a panel or a taskbar leaves free. Published as
   * the desktop's work area, so that the browser's `screen.avail*` values
   * are smaller than the screen, as on a desktop. Absent: no hint.
   */
  readonly workArea?: WorkArea;
}

const LAYOUT_BY_REGION: Readonly<Record<string, string>> = {
  de: 'de',
  at: 'at',
  ch: 'ch',
  fr: 'fr',
  be: 'be',
  gb: 'gb',
  ie: 'ie',
  it: 'it',
  es: 'es',
  pt: 'pt',
  us: 'us',
  nl: 'us',
  pl: 'pl',
  se: 'se',
  no: 'no',
  dk: 'dk',
  fi: 'fi',
  cz: 'cz',
};

const LAYOUT_BY_LANGUAGE: Readonly<Record<string, string>> = {
  de: 'de',
  fr: 'fr',
  it: 'it',
  es: 'es',
  pt: 'pt',
  pl: 'pl',
  sv: 'se',
  da: 'dk',
  fi: 'fi',
  cs: 'cz',
  nb: 'no',
};

/**
 * The keyboard layout people of a locale type on. The region decides where
 * there is one; a language alone falls back to its main country, and
 * anything unknown to the layout most keyboards are sold with.
 */
export function keyboardLayoutOf(locale: string): string {
  const [language = 'en', region] = locale.toLowerCase().split('-');
  return (
    (region === undefined ? undefined : LAYOUT_BY_REGION[region]) ??
    LAYOUT_BY_LANGUAGE[language] ??
    'us'
  );
}

/**
 * Starts an X virtual framebuffer with one screen of the given geometry and
 * waits until it accepts clients.
 *
 * The server listens on its Unix socket only, never on TCP. The display is
 * up when its lock names this Xvfb and the server answers a client; a lock
 * of another live process means another server holds the number, and this
 * one stops (see {@link DisplayPool.start}).
 *
 * @throws {EngineError} `browser_launch_failed` when Xvfb is missing, exits
 *   or does not come up in time, or when the display cannot be prepared.
 */
export async function startXvfb(
  display: number,
  geometry: DisplayGeometry,
  options: DisplayOptions = {},
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

  const deadline = Date.now() + START_TIMEOUT_MS;
  const later = async () => {
    if (Date.now() > deadline) {
      throw new EngineError(
        'browser_launch_failed',
        `Xvfb ${name} did not come up within ${START_TIMEOUT_MS} ms`,
      );
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  };
  const ready = (async () => {
    // The lock comes first, and names the server that holds the display.
    for (;;) {
      const holder = holderOf(display);
      if (holder !== null && holder !== child.pid) {
        throw new DisplayTaken(display);
      }
      if (holder === child.pid && existsSync(socketOf(display))) {
        break;
      }
      await later();
    }
    // The socket is there once the server listens; it answers once it has
    // started.
    for (;;) {
      const answered = await X11Connection.connect(name, Math.max(1, deadline - Date.now())).then(
        (connection) => {
          connection.close();
          return true;
        },
        () => false,
      );
      if (answered) {
        break;
      }
      await later();
    }
  })();

  let hint: { close(): void } | null = null;
  try {
    await Promise.race([ready, failure]);
    if (options.layout !== undefined) {
      await run('setxkbmap', ['-display', name, '-layout', options.layout], { timeout: 10_000 });
    }
    if (options.workArea !== undefined) {
      hint = await announceWorkArea(name, options.workArea);
    }
  } catch (error) {
    await stopProcess(child);
    // An Xvfb that found the display held exits on its own: it is the
    // other server's number all the same.
    const holder = holderOf(display);
    if (holder !== null && holder !== child.pid) {
      throw new DisplayTaken(display);
    }
    if (error instanceof EngineError) {
      throw error;
    }
    const reason = (error instanceof Error ? error.message : String(error)).split('\n')[0] ?? '';
    throw new EngineError(
      'browser_launch_failed',
      `the display ${name} could not be prepared: ${reason}`,
      { cause: error },
    );
  }
  const held = hint;
  return {
    name,
    stop: async () => {
      held?.close();
      await stopProcess(child);
    },
  };
}
