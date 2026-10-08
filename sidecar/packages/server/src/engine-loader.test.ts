import { describe, expect, it } from 'vitest';

import { loadEngine } from './engine-loader.js';

describe('loadEngine (the ENGINE switch)', () => {
  it.each(['chromium', 'firefox'] as const)('loads the %s engine package', async (name) => {
    const engine = await loadEngine(name);

    expect(engine.name).toBe(name);
    await engine.close();
  });
});
