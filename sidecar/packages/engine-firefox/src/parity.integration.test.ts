/**
 * The engine parity suite on the Firefox engine (docs/02-plan.md M5): real
 * Camoufox on its own X display, through a local authenticated proxy,
 * against the fixture site. It needs the pinned Camoufox build and Xvfb
 * (`just setup-camoufox`) and a user that is not root.
 */
import { defineParitySuite } from '@kage/engine-core/testing';

import { createEngine } from './index.js';

defineParitySuite({
  name: 'firefox',
  // A Camoufox persona claims Windows or macOS (spec §6.1); Windows is the
  // larger share and the claim the plan asks to be proven.
  claimedOs: 'windows',
  browserMajor: 152,
  createEngine,
  // Firefox draws the whole document from the browser side: the page sees nothing.
  fullPageSideEffects: [],
  // Firefox encodes PNG and JPEG only.
  webpScreenshot: 'image/png',
  netErrors: {
    tunnel: 'NS_ERROR_PROXY_FORBIDDEN',
    deadProxy: 'NS_ERROR_PROXY_CONNECTION_REFUSED',
  },
});
