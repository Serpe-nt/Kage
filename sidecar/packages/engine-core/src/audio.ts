import { spawn, type ChildProcess } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';

import { EngineError, type EngineLog } from './engine.js';

/**
 * A sound server for the browsers of one sidecar (docs/02-plan.md F9).
 *
 * A machine a person sits at has speakers and usually a microphone. A
 * container has neither, and a browser in it lists no media device and
 * opens an `AudioContext` without channels, which is what a detection
 * script sees of "no sound card". This starts a PulseAudio with one output
 * and one input that go nowhere: the browsers find real devices with real
 * sample rates, and nothing is ever played or recorded.
 */
export interface AudioServer {
  /** What a browser is given as `PULSE_SERVER`. */
  readonly address: string;
  /** Stops the server. Safe to call more than once. */
  stop(): Promise<void>;
}

/** How long the server may take to open its socket. */
const START_TIMEOUT_MS = 8000;

/**
 * The configuration the server is started with: two devices, no hardware, no network.
 *
 * The two modules name their device differently: the sink takes
 * `sink_properties`, the source only `description` (PulseAudio 17 refuses
 * to load it with `source_properties`, and the server then runs with no
 * input but the output's monitor).
 */
export function audioScript(socket: string): string {
  return [
    `load-module module-native-protocol-unix socket=${socket} auth-anonymous=1`,
    'load-module module-null-sink sink_name=speakers sink_properties=device.description=Speakers',
    'load-module module-null-source source_name=microphone description=Microphone',
    'set-default-sink speakers',
    'set-default-source microphone',
    '',
  ].join('\n');
}

/** How the server process is started; injectable for tests. */
export type Launch = (
  command: string,
  args: readonly string[],
  env: Record<string, string>,
) => ChildProcess;

const launchProcess: Launch = (command, args, env) =>
  spawn(command, [...args], { env, stdio: ['ignore', 'ignore', 'pipe'] });

/**
 * Starts the sound server in a directory of its own.
 *
 * @param directory a writable directory (on the image: below `/tmp`); the socket and the server's state live in it.
 * @throws {EngineError} `browser_launch_failed` when PulseAudio is missing or does not come up.
 */
export async function startAudioServer(
  directory: string,
  log: EngineLog,
  launch: Launch = launchProcess,
): Promise<AudioServer> {
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const socket = path.join(directory, 'native');
  const script = path.join(directory, 'kage.pa');
  await writeFile(script, audioScript(socket), { mode: 0o600 });
  const child = launch(
    'pulseaudio',
    [
      '--daemonize=no',
      '--exit-idle-time=-1',
      '--use-pid-file=no',
      '--log-target=stderr',
      '--log-level=error',
      '-n',
      `--file=${script}`,
    ],
    {
      PATH: process.env['PATH'] ?? '/usr/local/bin:/usr/bin:/bin',
      HOME: directory,
      XDG_RUNTIME_DIR: directory,
      XDG_CONFIG_HOME: path.join(directory, 'config'),
    },
  );
  let stderr = '';
  child.stderr?.on('data', (chunk: Buffer) => {
    stderr = (stderr + chunk.toString('utf8')).slice(-1000);
  });
  let exited = false;
  const failure = new Promise<never>((_resolve, reject) => {
    child.once('error', (error) => {
      exited = true;
      reject(
        new EngineError(
          'browser_launch_failed',
          `PulseAudio could not be started: ${error.message}`,
          {
            cause: error,
          },
        ),
      );
    });
    child.once('exit', (code, signal) => {
      exited = true;
      reject(
        new EngineError(
          'browser_launch_failed',
          `PulseAudio exited with ${code ?? signal ?? 'no status'}: ${stderr.trim().split('\n').at(-1) ?? ''}`,
        ),
      );
    });
  });
  // An exit after a clean stop must not become an unhandled rejection.
  failure.catch(() => undefined);
  const stop = (): Promise<void> => {
    if (exited) {
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
  };
  const ready = (async () => {
    const deadline = Date.now() + START_TIMEOUT_MS;
    while (!existsSync(socket)) {
      if (Date.now() > deadline) {
        throw new EngineError(
          'browser_launch_failed',
          `PulseAudio did not open ${socket} within ${START_TIMEOUT_MS} ms`,
        );
      }
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
  })();
  try {
    await Promise.race([ready, failure]);
  } catch (error) {
    await stop();
    throw error;
  }
  child.once('exit', (code, signal) => {
    // The browsers that are open keep running; they just lose their devices.
    log.warn({ code, signal }, 'the sound server exited');
  });
  return { address: `unix:${socket}`, stop };
}
