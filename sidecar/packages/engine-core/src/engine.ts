import type {
  AppliedIdentity,
  EngineName,
  ErrorCode,
  ExportStateRequest,
  ExportStateResponse,
  FlowEvent,
  FlowRequest,
  GeneratePersonaRequest,
  GeneratePersonaResponse,
  ImportStateRequest,
  ImportStateResponse,
  NavigateRequest,
  OpenContextRequest,
  PageResult,
  SelftestPage,
  SelftestRequest,
  SelftestResponse,
} from '@kage/contract';

import type { ArtifactWriter } from './artifacts.js';

/** Facts an engine reports through `GET /v1/health`. */
export interface EngineStatus {
  /** Full version of the browser the engine drives; `null` for a stub. */
  readonly browserVersion: string | null;
}

/**
 * A failure the server turns into an error answer of the contract. Anything
 * else an engine throws becomes `internal`.
 */
export class EngineError extends Error {
  override readonly name = 'EngineError';

  constructor(
    readonly code: ErrorCode,
    message: string,
    options?: ErrorOptions,
  ) {
    super(message, options);
  }
}

/** What an operation on a context may use besides its request. */
export interface OperationEnv {
  /** Writes files below the artifacts volume. */
  readonly artifacts: ArtifactWriter;
  /** Directory below the artifacts volume when the request names none. */
  readonly defaultArtifactPrefix: string;
}

/** One open persona context: a browser, its profile, its display, its proxy. */
export interface EngineContext {
  /** Full version of the browser that runs the context. */
  readonly browserVersion: string;
  /** What the engine really applied. */
  readonly applied: AppliedIdentity;
  navigate(request: NavigateRequest, env: OperationEnv): Promise<PageResult>;
  /**
   * Runs a flow. Every finished step and the final result are handed to
   * `emit` as they happen; the promise resolves after the final event.
   */
  flow(request: FlowRequest, env: OperationEnv, emit: (event: FlowEvent) => void): Promise<void>;
  exportState(request: ExportStateRequest): Promise<ExportStateResponse>;
  importState(request: ImportStateRequest): Promise<ImportStateResponse>;
  selftest(request: SelftestRequest, env: OperationEnv): Promise<SelftestResponse>;
  /** Closes the browser and flushes the profile. Safe to call more than once. */
  close(): Promise<void>;
}

/**
 * What every engine package implements. The server only talks to this
 * interface, so both engines stay interchangeable behind one contract.
 */
export interface Engine {
  readonly name: EngineName;
  status(): EngineStatus;
  generatePersona(request: GeneratePersonaRequest): Promise<GeneratePersonaResponse>;
  /** Opens a context. The server enforces the cap, the idle close and the one-context-per-persona rule. */
  openContext(request: OpenContextRequest): Promise<EngineContext>;
  /** Releases every browser resource. Safe to call more than once. */
  close(): Promise<void>;
}

/** What an engine is created with: the server's configuration, as far as engines need it. */
export interface EngineOptions {
  /** Root of the personas volume; profile directories live below it. */
  readonly personasDir: string;
  /** Whether a persona may claim another operating system than the host's. */
  readonly allowCrossOsClaims: boolean;
  /** Where WebGL comes from. */
  readonly gl: 'none' | 'swiftshader' | 'llvmpipe' | 'host';
  /** Default page set of a self-test: Kage's own pages only. */
  readonly selftestPages: readonly SelftestPage[];
  /** Receives log lines; the server passes its pino logger. */
  readonly log: EngineLog;
}

/** The part of a pino logger engines use. */
export interface EngineLog {
  info(fields: Record<string, unknown>, message: string): void;
  warn(fields: Record<string, unknown>, message: string): void;
  error(fields: Record<string, unknown>, message: string): void;
}

/** A log that drops everything, for tests. */
export const silentLog: EngineLog = {
  info: () => undefined,
  warn: () => undefined,
  error: () => undefined,
};

/** Shape of an engine package: the server loads it through the `ENGINE` switch. */
export interface EngineModule {
  createEngine(options: EngineOptions): Engine | Promise<Engine>;
}

/**
 * An engine that launches no browser: health works, everything else answers
 * `not_implemented`. The Firefox package uses it until M5.
 */
export function createStubEngine(name: EngineName): Engine {
  const refuse = (): Promise<never> =>
    Promise.reject(new EngineError('not_implemented', `the ${name} engine is not implemented yet`));
  return {
    name,
    status: () => ({ browserVersion: null }),
    generatePersona: refuse,
    openContext: refuse,
    close: () => Promise.resolve(),
  };
}
