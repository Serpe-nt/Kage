import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import { describe, expect, it } from 'vitest';

import { look, startWatchdog, type Looking } from './watchdog.js';

const source = fileURLToPath(new URL('./watchdog.ts', import.meta.url));

/** Runs a script in a node process of its own; resolves with how it ended. */
function run(
  script: string,
  limitMs: number,
): Promise<{ code: number | null; signal: NodeJS.Signals | null; stderr: string; ms: number }> {
  return new Promise((resolve, reject) => {
    const began = Date.now();
    const child = spawn(process.execPath, ['--input-type=module', '-e', script], {
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    let stderr = '';
    child.stderr.on('data', (chunk: Buffer) => (stderr += chunk.toString()));
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`the child did not end within ${limitMs} ms: ${stderr}`));
    }, limitMs);
    child.on('exit', (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, stderr, ms: Date.now() - began });
    });
  });
}

describe('the watchdog of the event loop (F103)', () => {
  it('ends a process whose main thread stops answering', async () => {
    const script = `
      const { startWatchdog } = await import(${JSON.stringify(source)});
      startWatchdog(800);
      // Like a driver that spins on microtasks without end: nothing else of
      // this thread ever runs again.
      setTimeout(() => { for (;;) {} }, 100);
    `;
    const ended = await run(script, 15_000);
    expect(ended.signal, ended.stderr).toBe('SIGKILL');
    // Once the limit is past, within about a look of the watchdog.
    expect(ended.ms).toBeGreaterThanOrEqual(800);
    expect(ended.ms).toBeLessThan(8000);
    const line = JSON.parse(ended.stderr.trim().split('\n').at(-1) ?? '{}') as Record<
      string,
      unknown
    >;
    // Kage's shape (F144).
    expect(line['level']).toBe('FATAL');
    expect(typeof line['timestamp']).toBe('string');
    const fields = line['fields'] as Record<string, unknown>;
    expect(fields['message']).toContain('the event loop stopped answering');
    expect(fields['limit_ms']).toBe(800);
  });

  it('leaves a process alone whose main thread answers, and keeps none alive', async () => {
    const script = `
      const { startWatchdog } = await import(${JSON.stringify(source)});
      const watchdog = startWatchdog(500);
      // Busy now and then, never for long.
      let left = 12;
      const tick = setInterval(() => {
        const until = Date.now() + 150;
        while (Date.now() < until) {}
        left -= 1;
        if (left === 0) clearInterval(tick);
      }, 50);
    `;
    const ended = await run(script, 15_000);
    expect(ended, ended.stderr).toMatchObject({ code: 0, signal: null });
  });

  it('leaves a process alone that was paused whole, however long', async () => {
    // A host that freezes under load, or a stopped container, pauses the
    // watchdog with the main thread: when they go on, the main thread beats
    // at once and nothing is ended (measured: a host stalled 150 s).
    const script = `
      const { startWatchdog } = await import(${JSON.stringify(source)});
      startWatchdog(800);
      setTimeout(() => process.exit(0), 1500);
    `;
    const began = Date.now();
    const child = spawn(process.execPath, ['--input-type=module', '-e', script], {
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    let stderr = '';
    child.stderr.on('data', (chunk: Buffer) => (stderr += chunk.toString()));
    const ended = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolve) => {
      child.on('exit', (code, signal) => {
        resolve({ code, signal });
      });
    });
    await new Promise((resolve) => setTimeout(resolve, 400));
    child.kill('SIGSTOP');
    // Three times the limit, the whole process standing still.
    await new Promise((resolve) => setTimeout(resolve, 2400));
    child.kill('SIGCONT');
    expect(await ended, stderr).toEqual({ code: 0, signal: null });
    expect(Date.now() - began).toBeGreaterThanOrEqual(2800);
  });

  it('counts only time in which it looked itself', () => {
    const lookMs = 250;
    const looks = (state: Looking, times: number[], beat: number) =>
      times.reduce((current, now) => look(current, now, beat, lookMs), state);
    const start: Looking = { seen: 1, stalled: 0, looked: 0 };
    // A main thread that stopped: every look on time adds a look.
    const stuck = looks(start, [250, 500, 750, 1000], 1);
    expect(stuck).toEqual({ seen: 1, stalled: 1000, looked: 1000 });
    // A beat that moved starts the count again.
    expect(look(stuck, 1250, 2, lookMs)).toEqual({ seen: 2, stalled: 0, looked: 1250 });
    // A pause of everything (the host froze 150 s, measured 2026-11-02): the
    // look after it counts as two looks, not as the pause.
    const paused = look(start, 150_000, 1, lookMs);
    expect(paused.stalled).toBe(500);
    // A clock that went back counts nothing.
    expect(look(start, -5000, 1, lookMs).stalled).toBe(0);
  });

  it('starts nothing when the limit is 0', async () => {
    const watchdog = startWatchdog(0);
    await expect(watchdog.stop()).resolves.toBeUndefined();
  });
});
