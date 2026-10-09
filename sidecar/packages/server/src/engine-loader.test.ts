import { tmpdir } from 'node:os';

import { silentLog } from '@kage/engine-core';
import { describe, expect, it } from 'vitest';

import { loadEngine } from './engine-loader.js';

const options = {
  personasDir: tmpdir(),
  allowCrossOsClaims: false,
  gl: 'none' as const,
  selftestPages: [],
  log: silentLog,
};

describe('loadEngine (the ENGINE switch)', () => {
  it('loads the firefox engine package: a stub until M5', async () => {
    const engine = await loadEngine('firefox', options);
    expect(engine.name).toBe('firefox');
    expect(engine.status()).toEqual({ browserVersion: null });
    await engine.close();
  });

  it('loads the chromium engine package and finds the installed Chrome', async () => {
    const engine = await loadEngine('chromium', options);
    expect(engine.name).toBe('chromium');
    expect(engine.status().browserVersion).toMatch(/^\d+\.\d+\.\d+\.\d+$/);
    await engine.close();
  });
});
