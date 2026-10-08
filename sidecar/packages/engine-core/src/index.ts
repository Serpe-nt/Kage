import type { EngineName } from '@kage/contract';

/** Facts an engine reports through `GET /v1/health`. */
export interface EngineStatus {
  /** Version of the browser the engine drives; `null` while no browser is running. */
  readonly browserVersion: string | null;
  /** Persona contexts currently open. */
  readonly contextsInUse: number;
}

/**
 * What every engine package implements. The server only talks to this
 * interface, so both engines stay interchangeable behind one contract. It
 * grows with milestones M4 (chromium) and M5 (firefox).
 */
export interface Engine {
  readonly name: EngineName;
  status(): EngineStatus;
  /** Releases every browser resource. Safe to call more than once. */
  close(): Promise<void>;
}

/** Shape of an engine package: the server loads it through the `ENGINE` switch. */
export interface EngineModule {
  createEngine(): Engine | Promise<Engine>;
}

/**
 * An engine that launches no browser. Scaffold placeholder used by both
 * engine packages until M4 and M5 replace it with real launchers.
 */
export function createStubEngine(name: EngineName): Engine {
  return {
    name,
    status: () => ({ browserVersion: null, contextsInUse: 0 }),
    close: () => Promise.resolve(),
  };
}
