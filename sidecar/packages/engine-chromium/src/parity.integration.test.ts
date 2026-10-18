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
  // Chrome has no navigator.getAutoplayPolicy().
  autoplayPolicy: null,
  netErrors: { tunnel: 'ERR_TUNNEL_CONNECTION_FAILED', deadProxy: 'ERR_PROXY_CONNECTION_FAILED' },
  // A known tell, measured in the Camoufox 156 milestone and not fixed here:
  // the driver's default switches (patchright 1.63.0, Playwright's
  // chromiumSwitches) start Chrome with ThirdPartyStoragePartitioning off,
  // so a frame of another site reads its site's first-party storage, where
  // a Chrome nobody configured partitions it.
  thirdPartyStoragePartitioned: false,
});
