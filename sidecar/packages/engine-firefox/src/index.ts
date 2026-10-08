import { createStubEngine, type Engine } from '@kage/engine-core';

/**
 * Creates the Firefox engine.
 *
 * Scaffold: it launches no browser yet. M5 replaces the stub with camoufox-js
 * driving the pinned Camoufox build under its virtual display.
 */
export function createEngine(): Engine {
  return createStubEngine('firefox');
}
