/**
 * The engine parity suite on the Firefox engine (docs/02-plan.md M5): real
 * Camoufox on its own X display, through a local authenticated proxy,
 * against the fixture site. It needs the pinned Camoufox build and Xvfb
 * (`just setup-camoufox`) and a user that is not root.
 */
import type { ClaimedOs } from '@kage/contract';
import type { FingerprintDocument } from '@kage/engine-core';
import { defineParitySuite, type DocumentReads } from '@kage/engine-core/testing';

import { createEngine, firefoxUserAgentOf, generatedOf } from './index.js';

/**
 * The work area Firefox reports in place of the device's: its baseline
 * fingerprinting protection, on by default, gives the whole screen less a
 * fixed taskbar of the system (nsRFPService::GetSpoofedScreenAvailSize),
 * and Camoufox 156 applies it to the persona's screen as Firefox does to a
 * real one. Camoufox 152 showed the document's own work area instead.
 */
const BASELINE_TASKBAR: Readonly<Record<ClaimedOs, number>> = { windows: 48, macos: 76, linux: 0 };

/** What a page reads of a Firefox persona where its document fixes it. */
function documentReads(document: FingerprintDocument): DocumentReads {
  const { config } = generatedOf(document);
  const text = (key: string) => {
    const value = config[key];
    if (typeof value !== 'string') {
      throw new TypeError(`the document has no ${key}`);
    }
    return value;
  };
  const number = (key: string) => {
    const value = config[key];
    if (typeof value !== 'number') {
      throw new TypeError(`the document has no ${key}`);
    }
    return value;
  };
  return {
    user_agent: text('navigator.userAgent'),
    platform: text('navigator.platform'),
    oscpu: text('navigator.oscpu'),
    app_version: text('navigator.appVersion'),
    hardware_concurrency: number('navigator.hardwareConcurrency'),
    screen: {
      width: number('screen.width'),
      height: number('screen.height'),
      avail_width: number('screen.width'),
      avail_height: number('screen.height') - BASELINE_TASKBAR[document.claimed_os],
      color_depth: number('screen.colorDepth'),
    },
    webgl: { vendor: text('webGl:vendor'), renderer: text('webGl:renderer') },
  };
}

/** The persona's document as the previous Firefox major made it: the same device, an older browser. */
function olderDocument(document: FingerprintDocument): FingerprintDocument {
  const major = Number(document.browser_version.split('.')[0]);
  const userAgent = firefoxUserAgentOf(document.user_agent, major, major - 1);
  const generated = generatedOf(document);
  return {
    ...document,
    browser_version: `${String(major - 1)}.0.1`,
    user_agent: userAgent,
    generated: {
      ...generated,
      config: { ...generated.config, 'navigator.userAgent': userAgent },
    },
  };
}

defineParitySuite({
  name: 'firefox',
  // A Camoufox persona claims Windows or macOS (spec §6.1); Windows is the
  // larger share and the claim the plan asks to be proven.
  claimedOs: 'windows',
  browserMajor: 156,
  createEngine,
  // Firefox draws the whole document from the browser side: the page sees nothing.
  fullPageSideEffects: [],
  // Firefox encodes PNG and JPEG only.
  webpScreenshot: 'image/png',
  // What a stock Firefox 152 and 156 say, measured with no driver at all.
  // The 156 build ships Firefox's own default; the launch plan sets it as
  // well, so that a build that changes it is still answered this way.
  autoplayPolicy: { mediaelement: 'allowed-muted', audiocontext: 'disallowed' },
  netErrors: {
    tunnel: 'NS_ERROR_PROXY_FORBIDDEN',
    deadProxy: 'NS_ERROR_PROXY_CONNECTION_REFUSED',
  },
  documentReads,
  olderDocument,
  // Firefox's own cookie behaviour (5) partitions it.
  thirdPartyStoragePartitioned: true,
});
