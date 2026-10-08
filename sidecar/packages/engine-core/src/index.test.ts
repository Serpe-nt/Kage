import { describe, expect, it } from 'vitest';

import { createStubEngine } from './index.js';

describe('createStubEngine', () => {
  it.each(['chromium', 'firefox'] as const)('reports an idle %s engine', (name) => {
    const engine = createStubEngine(name);

    expect(engine.name).toBe(name);
    expect(engine.status()).toEqual({ browserVersion: null, contextsInUse: 0 });
  });

  it('closes more than once without error', async () => {
    const engine = createStubEngine('chromium');

    await expect(engine.close()).resolves.toBeUndefined();
    await expect(engine.close()).resolves.toBeUndefined();
  });
});
