import { createStubEngine, type Engine } from '@kage/engine-core';

/**
 * Creates the Chromium engine.
 *
 * Scaffold: it launches no browser yet. M4 replaces the stub with patchright
 * driving the real Chrome channel, headful under a virtual display.
 */
export function createEngine(): Engine {
  return createStubEngine('chromium');
}
