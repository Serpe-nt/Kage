import {
  EXAMPLES,
  type AppliedIdentity,
  type OpenContextRequest,
  type PageResult,
} from '@kage/contract';
import type { Engine, EngineContext, OperationEnv } from '@kage/engine-core';

// The caller names the type of the example it asks for; that is the whole
// point of the parameter.
// eslint-disable-next-line @typescript-eslint/no-unnecessary-type-parameters
const example = <T>(id: string, index = 0): T => structuredClone(EXAMPLES[id]?.[index]) as T;

/** The token the fake server is built with. */
export const TEST_TOKEN = 'test-token-0123456789abcdef-0123456789';

/** What a fake context did, for assertions. */
export interface FakeContext extends EngineContext {
  readonly request: OpenContextRequest;
  readonly navigated: { url: string; env: OperationEnv }[];
  closes: number;
}

/** A scripted engine: no browser, every call recorded. */
export interface FakeEngine extends Engine {
  readonly contexts: FakeContext[];
  closes: number;
  /** Replaced by a test to make the next open fail or wait. */
  beforeOpen: (request: OpenContextRequest) => Promise<void>;
  /** Replaced by a test to change what a navigation returns or throws. */
  onNavigate: (url: string) => Promise<PageResult>;
}

/** Builds a fake engine whose answers are the contract's own examples. */
export function createFakeEngine(): FakeEngine {
  const applied = example<{ applied: AppliedIdentity }>('OpenContextResponse').applied;
  const engine: FakeEngine = {
    name: 'chromium',
    contexts: [],
    closes: 0,
    beforeOpen: () => Promise.resolve(),
    onNavigate: (url) => Promise.resolve({ ...example<PageResult>('PageResult'), url }),
    status: () => ({ browserVersion: '155.0.8059.39' }),
    generatePersona: () => Promise.resolve(example('GeneratePersonaResponse')),
    openContext: async (request) => {
      await engine.beforeOpen(request);
      const context: FakeContext = {
        request,
        navigated: [],
        closes: 0,
        browserVersion: '155.0.8059.39',
        applied,
        navigate: (navigate, env) => {
          context.navigated.push({ url: navigate.url, env });
          return engine.onNavigate(navigate.url);
        },
        exportState: () => Promise.resolve(example('ExportStateResponse')),
        importState: () => Promise.resolve(example('ImportStateResponse')),
        selftest: () => Promise.resolve(example('SelftestResponse')),
        close: () => {
          context.closes += 1;
          return Promise.resolve();
        },
      };
      engine.contexts.push(context);
      return context;
    },
    close: () => {
      engine.closes += 1;
      return Promise.resolve();
    },
  };
  return engine;
}

/** The contract's example of a request body, by schema id. */
export const exampleOf = example;
