import type { Engine, EngineOptions } from '@kage/engine-core';

import { ChromiumEngine } from './engine.js';
import { DEFAULT_CHROME_PATH, detectHost } from './host.js';

export { CHROME_WRAPPER, ChromiumEngine } from './engine.js';
export { ChromiumContext, classifyNavigationError } from './context.js';
export {
  DEFAULT_POLL_MS,
  PageReadError,
  PageReader,
  ReadTimeoutError,
  pageWithReader,
  type PageReaderOptions,
} from './page-reader.js';
export {
  GENERATOR_ID,
  HOST_BOUND,
  buildInjection,
  generateFingerprint,
  suiteVersion,
} from './fingerprint.js';
export * from './host.js';
export * from './launch-plan.js';
export * from './profile.js';

/**
 * Creates the Chromium engine: patchright driving the real Chrome of the
 * image, headful on one virtual display per persona.
 *
 * `SIDECAR_CHROME_PATH` names another Chrome binary than the image's.
 *
 * @throws {EngineError} `browser_launch_failed` when there is no working Chrome.
 */
export async function createEngine(options: EngineOptions): Promise<Engine> {
  const host = await detectHost(process.env['SIDECAR_CHROME_PATH'] ?? DEFAULT_CHROME_PATH);
  options.log.info(
    { chrome: host.chromeVersion, cpus: host.cpus.length, gl: options.gl },
    'chromium engine ready',
  );
  return new ChromiumEngine(options, host);
}
