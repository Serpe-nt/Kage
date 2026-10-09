import type { EngineName } from '@kage/contract';
import type { Engine, EngineModule, EngineOptions } from '@kage/engine-core';

/**
 * The `ENGINE` switch. One lazy import per engine: only the selected engine
 * package is ever loaded, so each container image can ship a single engine
 * and its own playwright version.
 */
const loaders: Record<EngineName, () => Promise<EngineModule>> = {
  chromium: () => import('@kage/engine-chromium'),
  firefox: () => import('@kage/engine-firefox'),
};

/** Loads the engine package for `name` and creates its engine. */
export async function loadEngine(name: EngineName, options: EngineOptions): Promise<Engine> {
  const engineModule = await loaders[name]();
  return engineModule.createEngine(options);
}
