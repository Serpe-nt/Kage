import { Worker } from 'node:worker_threads';

/**
 * The watchdog of the sidecar's event loop (F103).
 *
 * A page can make the driver spin on the main thread without end (QA round
 * one, extract-02: a renderer that crashed sends patchright 1.63.0 into an
 * endless chain of microtasks), and then nothing of the server runs again:
 * no request is answered, not even the health check, and no timer fires.
 * Docker restarts a container whose main process ends, not one that is only
 * unhealthy. So a thread of its own watches the main thread's heartbeat and
 * ends the process when the beat stops for longer than `stallMs`: the
 * container's restart policy (`restart: unless-stopped`) then starts the
 * sidecar again, Kage reopens the contexts it needs from their profiles,
 * and the one page that stopped the engine fails alone.
 *
 * The heartbeat is a timestamp the main thread writes into shared memory
 * once a second; the watchdog reads it, so it needs nothing of the main
 * thread to decide. It ends the process with `SIGKILL`: no handler of the
 * stuck thread could run anyway, and the browsers die with the container.
 *
 * Only time in which the watchdog itself ran counts: each of its looks adds
 * the time since the last one, at most two looks' worth. A pause of the
 * whole process or machine (a stopped container, a host that froze for
 * minutes under load) stops the watchdog too, and adds no more than that
 * when it ends: the main thread then beats again at once. Measured
 * 2026-11-02: a host that stalled 150 s under the load of other test stacks
 * had both sidecars ended by a watchdog that counted the pause.
 */

/** What the watchdog reports before it ends a stalled process. */
export interface StallReport {
  readonly stalled_ms: number;
  readonly limit_ms: number;
}

/** A running watchdog: stops the heartbeat and the watching thread. */
export interface Watchdog {
  stop(): Promise<void>;
}

/**
 * How often the main thread beats, and how often the watchdog looks, at
 * most: a beat comes at least four times within the limit, so that a
 * thread that answers is never taken for one that stopped.
 */
const BEAT_MS = 1000;

/** What the watchdog knows between two of its looks. */
export interface Looking {
  /** The beat it saw last. */
  readonly seen: number;
  /** How long the beat has not moved, in time of the watchdog's own. */
  readonly stalled: number;
  /** When it looked last. */
  readonly looked: number;
}

/**
 * One look of the watchdog at the beat: the state after it. A beat that
 * moved starts the count again; one that did not adds the time since the
 * last look, at most two looks' worth, so that a pause of the whole
 * process or machine counts as no more than that. The watching thread runs
 * this very function (its source is embedded there).
 */
export function look(state: Looking, now: number, beat: number, lookMs: number): Looking {
  if (beat !== state.seen) {
    return { seen: beat, stalled: 0, looked: now };
  }
  const since = Math.max(0, Math.min(now - state.looked, 2 * lookMs));
  return { seen: state.seen, stalled: state.stalled + since, looked: now };
}

// The watching thread, as source of its own so that it runs from the
// built package and from the sources alike. It writes one line of JSON to
// standard error, as the server's log does, and ends the process.
const WATCHER = `
// Read as a module or as a script, whatever the package around says: the
// built-in modules are taken without import or require.
const { workerData, parentPort } = process.getBuiltinModule('node:worker_threads');
const { writeSync } = process.getBuiltinModule('node:fs');
const beat = new Float64Array(workerData.buffer);
const { limitMs, lookMs } = workerData;
let stopped = false;
const look = ${look.toString()};
let state = { seen: beat[0], stalled: 0, looked: Date.now() };
parentPort.on('message', (message) => {
  if (message === 'stop') {
    stopped = true;
    clearInterval(timer);
    parentPort.close();
  }
});
const timer = setInterval(() => {
  if (stopped) return;
  state = look(state, Date.now(), beat[0], lookMs);
  if (state.stalled > limitMs) {
    // The log's shape (log.ts, F144), written by hand: this thread loads
    // no module of the server's.
    const line = JSON.stringify({
      level: 'FATAL',
      timestamp: new Date().toISOString(),
      fields: {
        message: 'the event loop stopped answering: the sidecar ends itself so that it is restarted',
        stalled_ms: Math.round(state.stalled),
        limit_ms: limitMs,
      },
    });
    try { writeSync(2, line + '\\n'); } catch {}
    process.kill(process.pid, workerData.signal);
  }
}, lookMs);
`;

/**
 * Starts the heartbeat and the watching thread.
 *
 * @param stallMs how long the main thread may not beat; 0 starts nothing.
 * @param signal how the process is ended; `SIGKILL` unless a test says.
 */
export function startWatchdog(stallMs: number, signal: NodeJS.Signals = 'SIGKILL'): Watchdog {
  if (stallMs <= 0) {
    return { stop: () => Promise.resolve() };
  }
  const buffer = new SharedArrayBuffer(Float64Array.BYTES_PER_ELEMENT);
  const beat = new Float64Array(buffer);
  beat[0] = Date.now();
  const every = Math.max(10, Math.min(BEAT_MS, Math.floor(stallMs / 4)));
  const heart = setInterval(() => {
    beat[0] = Date.now();
  }, every);
  // The heartbeat never keeps the process alive, nor does the watcher.
  heart.unref();
  const watcher = new Worker(WATCHER, {
    eval: true,
    workerData: { buffer, limitMs: stallMs, lookMs: every, signal },
  });
  watcher.unref();
  return {
    stop: async () => {
      clearInterval(heart);
      watcher.postMessage('stop');
      await watcher.terminate();
    },
  };
}
