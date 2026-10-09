import type { ChildProcess } from 'node:child_process';
import { EventEmitter } from 'node:events';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';

import { describe, expect, it } from 'vitest';

import { audioScript, startAudioServer, type Launch } from './audio.js';
import { EngineError, silentLog } from './engine.js';

/** A process that does what a test tells it to. */
function fakeProcess() {
  const child = new EventEmitter() as EventEmitter & {
    stderr: EventEmitter;
    kill: (signal: string) => boolean;
    killed: string[];
  };
  child.stderr = new EventEmitter();
  child.killed = [];
  child.kill = (signal) => {
    child.killed.push(signal);
    setImmediate(() => child.emit('exit', null, signal));
    return true;
  };
  return child;
}

const directory = () => mkdtemp(path.join(tmpdir(), 'kage-audio-'));

describe('the sound server', () => {
  it('is configured with one output and one input that go nowhere', () => {
    const script = audioScript('/tmp/a/native');
    expect(script).toContain('module-native-protocol-unix socket=/tmp/a/native auth-anonymous=1');
    expect(script).toContain('module-null-sink sink_name=speakers');
    expect(script).toContain('module-null-source source_name=microphone');
    expect(script).toContain('set-default-sink speakers');
    // No hardware, no network, no discovery.
    expect(script).not.toMatch(/alsa|udev|tcp|zeroconf|bluetooth/);
  });

  it('starts in its own directory, answers with its address, and stops', async () => {
    const dir = await directory();
    const child = fakeProcess();
    const started: { command: string; args: readonly string[]; env: Record<string, string> }[] = [];
    const launch: Launch = (command, args, env) => {
      started.push({ command, args, env });
      // The server opens its socket a moment after it starts.
      setTimeout(() => void writeFile(path.join(dir, 'native'), ''), 60);
      return child as unknown as ChildProcess;
    };
    const warnings: string[] = [];
    const server = await startAudioServer(
      dir,
      { ...silentLog, warn: (_fields, message) => warnings.push(message) },
      launch,
    );
    expect(server.address).toBe(`unix:${path.join(dir, 'native')}`);
    expect(started).toHaveLength(1);
    expect(started[0]?.command).toBe('pulseaudio');
    expect(started[0]?.args).toEqual(
      expect.arrayContaining([
        '--daemonize=no',
        '--exit-idle-time=-1',
        '-n',
        `--file=${path.join(dir, 'kage.pa')}`,
      ]),
    );
    // Its whole environment: nothing of the sidecar's own.
    expect(Object.keys(started[0]?.env ?? {}).sort()).toEqual([
      'HOME',
      'PATH',
      'XDG_CONFIG_HOME',
      'XDG_RUNTIME_DIR',
    ]);
    expect(await readFile(path.join(dir, 'kage.pa'), 'utf8')).toBe(
      audioScript(path.join(dir, 'native')),
    );
    await server.stop();
    expect(child.killed).toEqual(['SIGTERM']);
    expect(warnings).toEqual(['the sound server exited']);
    await server.stop();
    expect(child.killed).toEqual(['SIGTERM']);
  });

  it('says why when the server is not installed, or gives up', async () => {
    const missing = fakeProcess();
    const notInstalled: unknown = await startAudioServer(await directory(), silentLog, () => {
      setImmediate(() => missing.emit('error', new Error('spawn pulseaudio ENOENT')));
      return missing as unknown as ChildProcess;
    }).catch((error: unknown) => error);
    expect(notInstalled).toBeInstanceOf(EngineError);
    expect(notInstalled).toMatchObject({
      code: 'browser_launch_failed',
      message: 'PulseAudio could not be started: spawn pulseaudio ENOENT',
    });

    const dying = fakeProcess();
    const died: unknown = await startAudioServer(await directory(), silentLog, () => {
      setImmediate(() => {
        dying.stderr.emit('data', Buffer.from('E: something\nE: module failed to load\n'));
        dying.emit('exit', 1, null);
      });
      return dying as unknown as ChildProcess;
    }).catch((error: unknown) => error);
    expect(died).toMatchObject({
      code: 'browser_launch_failed',
      message: 'PulseAudio exited with 1: E: module failed to load',
    });
  });
});
