/**
 * The engine parity suite on the Chromium engine (docs/02-plan.md M5): the
 * same contract, capture and behavior tests the Firefox engine runs. What
 * only Chrome does is in `engine.integration.test.ts`.
 */
import { defineParitySuite } from '@kage/engine-core/testing';

import { createEngine } from './index.js';

defineParitySuite({
  name: 'chromium',
  claimedOs: 'linux',
  browserMajor: 155,
  createEngine,
  // Capturing beyond the viewport lays the page out at its full height once.
  fullPageSideEffects: ['resize'],
  webpScreenshot: 'image/webp',
  netErrors: { tunnel: 'ERR_TUNNEL_CONNECTION_FAILED', deadProxy: 'ERR_PROXY_CONNECTION_FAILED' },
});
