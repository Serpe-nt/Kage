import type { Engine, EngineOptions } from '@kage/engine-core';

import { FirefoxEngine } from './engine.js';
import { DEFAULT_CAMOUFOX_DIR, detectHost } from './host.js';

export { CAMOUFOX_WRAPPER, FirefoxEngine, profilePaths, type ProfilePaths } from './engine.js';
export { FirefoxContext, classifyNavigationError } from './context.js';
export * from './fingerprint.js';
export * from './fontconfig.js';
export * from './host.js';
export * from './launch-plan.js';
export * from './omni.js';
export * from './upgrade.js';

/**
 * Creates the Firefox engine: Camoufox on one virtual display per persona.
 *
 * `SIDECAR_CAMOUFOX_DIR` names another directory than the image's for the
 * unpacked build. The build must be the pinned one, with the archive of its
 * own scripts as it was released (`CAMOUFOX_OMNI_SHA256`).
 *
 * @throws {EngineError} `browser_launch_failed` when there is no such build.
 */
export async function createEngine(options: EngineOptions): Promise<Engine> {
  const host = await detectHost(process.env['SIDECAR_CAMOUFOX_DIR'] ?? DEFAULT_CAMOUFOX_DIR);
  options.log.info(
    { camoufox: `${host.version}-${host.release}`, cpus: host.cpus.length },
    'firefox engine ready',
  );
  return new FirefoxEngine(options, host);
}
