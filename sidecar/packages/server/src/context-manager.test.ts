import type { OpenContextRequest } from '@kage/contract';
import { EngineError, silentLog } from '@kage/engine-core';
import { describe, expect, it, vi } from 'vitest';

import { ContextManager, type CloseReason } from './context-manager.js';
import { createFakeEngine, exampleOf } from './testing/fake-engine.js';

function setup(
  limits: { maxContexts?: number; idleMs?: number; maxPagesPerBrowser?: number } = {},
) {
  const engine = createFakeEngine();
  const clock = { now: 0 };
  const events: string[] = [];
  const manager = new ContextManager(
    engine,
    {
      maxContexts: limits.maxContexts ?? 2,
      idleMs: limits.idleMs ?? 1000,
      maxPagesPerBrowser: limits.maxPagesPerBrowser ?? 100,
    },
    {
      opened: (result) => events.push(`opened:${result}`),
      closed: (reason: CloseReason) => events.push(`closed:${reason}`),
      recycled: () => events.push('recycled'),
    },
    silentLog,
    () => clock.now,
  );
  return { engine, clock, events, manager };
}

const request = (persona = 1): OpenContextRequest => ({
  ...exampleOf<OpenContextRequest>('OpenContextRequest'),
  persona_id: `0199c1f2-6b7a-7c3e-9d41-00000000000${persona}`,
});

describe('ContextManager', () => {
  it('starts one browser when two opens for a persona arrive together', async () => {
    const { engine, manager, events } = setup();
    let release: () => void = () => undefined;
    engine.beforeOpen = () => new Promise((resolve) => (release = resolve));
    const first = manager.open(request());
    const second = manager.open(request());
    expect(manager.count).toBe(1);
    await new Promise((resolve) => setTimeout(resolve, 10));
    release();
    const [a, b] = await Promise.all([first, second]);
    expect(a.managed.id).toBe(b.managed.id);
    expect([a.reused, b.reused]).toEqual([false, true]);
    expect(engine.contexts).toHaveLength(1);
    expect(events).toEqual(['opened:opened', 'opened:reused']);
  });

  it('counts a context that is still opening toward the cap', async () => {
    const { engine, manager } = setup({ maxContexts: 1 });
    let release: () => void = () => undefined;
    engine.beforeOpen = () => new Promise((resolve) => (release = resolve));
    const first = manager.open(request(1));
    await expect(manager.open(request(2))).rejects.toMatchObject({ code: 'context_limit' });
    release();
    await first;
  });

  it('frees the slot when an open fails, and lets the waiting open try itself', async () => {
    const { engine, manager, events } = setup({ maxContexts: 1 });
    let fail: (error: Error) => void = () => undefined;
    engine.beforeOpen = () => new Promise((_resolve, reject) => (fail = reject));
    const first = manager.open(request());
    const second = manager.open(request());
    await new Promise((resolve) => setTimeout(resolve, 10));
    engine.beforeOpen = () => Promise.resolve();
    fail(new EngineError('browser_launch_failed', 'no display'));
    await expect(first).rejects.toMatchObject({ code: 'browser_launch_failed' });
    await expect(second).resolves.toMatchObject({ reused: false });
    expect(manager.count).toBe(1);
    expect(events).toEqual(['opened:failed', 'opened:opened']);
  });

  it('treats the same request with its keys in another order as the same request', async () => {
    const { manager } = setup();
    const original = request();
    const { managed } = await manager.open(original);
    const reordered = Object.fromEntries(Object.entries(original).reverse()) as OpenContextRequest;
    await expect(manager.open(reordered)).resolves.toMatchObject({
      reused: true,
      managed: { id: managed.id },
    });
    await expect(
      manager.open({ ...original, languages: [...original.languages].reverse() }),
    ).rejects.toMatchObject({ code: 'context_conflict' });
  });

  it('keeps a context whose operation fails with a contract error and closes one that fails otherwise', async () => {
    const { engine, manager, events } = setup();
    const { managed } = await manager.open(request());
    await expect(
      manager.run(managed.id, false, () =>
        Promise.reject(new EngineError('invalid_request', 'no')),
      ),
    ).rejects.toMatchObject({ code: 'invalid_request' });
    expect(manager.count).toBe(1);
    await expect(
      manager.run(managed.id, false, () => Promise.reject(new Error('crash'))),
    ).rejects.toThrow('crash');
    expect(manager.count).toBe(0);
    expect(engine.contexts[0]?.closes).toBe(1);
    expect(events.at(-1)).toBe('closed:failed');
  });

  it('survives a context that fails to close', async () => {
    const { engine, manager } = setup();
    const { managed } = await manager.open(request());
    const [context] = engine.contexts;
    if (context !== undefined) {
      context.close = () => Promise.reject(new Error('already gone'));
    }
    await expect(manager.close(managed.id)).resolves.toBeUndefined();
    expect(manager.count).toBe(0);
  });

  it('does not close a context that is in use, however long it runs', async () => {
    const { manager, clock } = setup({ idleMs: 1000 });
    const { managed } = await manager.open(request());
    let finish: () => void = () => undefined;
    const running = manager.run(
      managed.id,
      true,
      () => new Promise<void>((resolve) => (finish = resolve)),
    );
    clock.now += 60_000;
    expect(await manager.sweep()).toBe(0);
    finish();
    await running;
    // Idleness counts from the end of the operation.
    clock.now += 999;
    expect(await manager.sweep()).toBe(0);
    clock.now += 1;
    expect(await manager.sweep()).toBe(1);
  });

  it('sweeps on a timer once started, and stops when everything is closed', async () => {
    vi.useFakeTimers();
    try {
      const { manager, clock, events } = setup({ idleMs: 4000 });
      manager.start();
      manager.start();
      await manager.open(request());
      clock.now += 5000;
      await vi.advanceTimersByTimeAsync(1000);
      expect(events).toContain('closed:idle');
      await manager.open(request());
      await manager.closeAll();
      expect(events.at(-1)).toBe('closed:shutdown');
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it('waits for an open in flight before it shuts down', async () => {
    const { engine, manager } = setup();
    let release: () => void = () => undefined;
    engine.beforeOpen = () => new Promise((resolve) => (release = resolve));
    const opening = manager.open(request());
    const closing = manager.closeAll();
    release();
    await opening;
    await closing;
    expect(engine.contexts[0]?.closes).toBe(1);
  });
});
