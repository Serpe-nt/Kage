import { describe, expect, it } from 'vitest';

import { DisplayPool, FIRST_DISPLAY, keyboardLayoutOf, startXvfb } from './display.js';
import { EngineError } from './engine.js';
import { X11Connection } from './x11/connection.js';

describe('displays', () => {
  it('hands out free display numbers and takes them back', () => {
    const busy = new Set([FIRST_DISPLAY + 1]);
    const pool = new DisplayPool((display) => !busy.has(display));
    expect(pool.acquire()).toBe(FIRST_DISPLAY);
    // The next number is in use on the machine and is skipped.
    expect(pool.acquire()).toBe(FIRST_DISPLAY + 2);
    pool.release(FIRST_DISPLAY);
    expect(pool.acquire()).toBe(FIRST_DISPLAY);
  });

  it('gives up when no display number is free', () => {
    const pool = new DisplayPool(() => false);
    expect(() => pool.acquire()).toThrow(
      expect.objectContaining({ code: 'browser_launch_failed' }) as Error,
    );
    expect(new DisplayPool().acquire()).toBeGreaterThanOrEqual(FIRST_DISPLAY);
  });

  it('starts an X server with the asked geometry and stops it', async () => {
    const pool = new DisplayPool();
    const number = pool.acquire();
    const display = await startXvfb(number, { width: 1366, height: 768, depth: 24 });
    expect(display.name).toBe(`:${number}`);
    await display.stop();
    await display.stop();
    pool.release(number);
  });

  it('reports an X server that cannot start', async () => {
    const number = new DisplayPool().acquire();
    const error: unknown = await startXvfb(number, { width: 0, height: 0, depth: 7 }).catch(
      (thrown: unknown) => thrown,
    );
    expect(error).toBeInstanceOf(EngineError);
    expect((error as EngineError).code).toBe('browser_launch_failed');
    expect((error as EngineError).message).toContain('Xvfb');
  });
});

describe('keyboard layouts', () => {
  it('gives a locale the layout of its country, a language its main one, the rest the common one', () => {
    expect(keyboardLayoutOf('de-DE')).toBe('de');
    expect(keyboardLayoutOf('de-AT')).toBe('at');
    expect(keyboardLayoutOf('en-GB')).toBe('gb');
    expect(keyboardLayoutOf('fr-FR')).toBe('fr');
    expect(keyboardLayoutOf('it-IT')).toBe('it');
    expect(keyboardLayoutOf('fr')).toBe('fr');
    expect(keyboardLayoutOf('en')).toBe('us');
    expect(keyboardLayoutOf('ja-JP')).toBe('us');
  });

  it('gives a display its layout and its work area, and says so when it cannot', async () => {
    const pool = new DisplayPool();
    const number = pool.acquire();
    const display = await startXvfb(
      number,
      { width: 1280, height: 800, depth: 24 },
      { layout: 'de', workArea: { x: 0, y: 32, width: 1280, height: 768 } },
    );
    try {
      const connection = await X11Connection.connect(display.name);
      // On a German keyboard the key right of T is Z: key code 29 carries z, not y.
      const rows = await connection.keyboardMapping();
      expect(rows[29 - connection.setup.minKeycode]?.[0]).toBe('z'.charCodeAt(0));
      expect(connection.setup).toMatchObject({ width: 1280, height: 800 });
      connection.close();
    } finally {
      await display.stop();
      pool.release(number);
    }
    const second = pool.acquire();
    const error: unknown = await startXvfb(
      second,
      { width: 800, height: 600, depth: 24 },
      { layout: 'no-such-layout' },
    ).catch((thrown: unknown) => thrown);
    pool.release(second);
    expect(error).toBeInstanceOf(EngineError);
    expect((error as EngineError).message).toContain('could not be prepared');
  });
});
