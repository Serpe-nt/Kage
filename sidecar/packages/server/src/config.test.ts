import { describe, expect, it } from 'vitest';

import { ConfigError, loadConfig } from './config.js';

describe('loadConfig', () => {
  it.each(['chromium', 'firefox'] as const)('selects the %s engine from ENGINE', (engine) => {
    expect(loadConfig({ ENGINE: engine })).toEqual({
      engine,
      host: '0.0.0.0',
      port: 8090,
      logLevel: 'info',
    });
  });

  it('reads host, port and log level overrides', () => {
    const config = loadConfig({
      ENGINE: 'firefox',
      SIDECAR_HOST: '127.0.0.1',
      SIDECAR_PORT: '9000',
      LOG_LEVEL: 'debug',
    });

    expect(config).toEqual({ engine: 'firefox', host: '127.0.0.1', port: 9000, logLevel: 'debug' });
  });

  it('requires ENGINE: there is no default engine', () => {
    expect(() => loadConfig({})).toThrow(ConfigError);
    expect(() => loadConfig({})).toThrow(/ENGINE/);
  });

  it.each([
    ['an unknown engine', { ENGINE: 'webkit' }, /ENGINE/],
    ['an engine in the wrong case', { ENGINE: 'Chromium' }, /ENGINE/],
    ['a port that is not a number', { ENGINE: 'chromium', SIDECAR_PORT: 'http' }, /SIDECAR_PORT/],
    ['a port out of range', { ENGINE: 'chromium', SIDECAR_PORT: '70000' }, /SIDECAR_PORT/],
    ['port zero', { ENGINE: 'chromium', SIDECAR_PORT: '0' }, /SIDECAR_PORT/],
    ['an unknown log level', { ENGINE: 'chromium', LOG_LEVEL: 'loud' }, /LOG_LEVEL/],
  ])('rejects %s', (_label, env, variable) => {
    expect(() => loadConfig(env)).toThrow(ConfigError);
    expect(() => loadConfig(env)).toThrow(variable);
  });

  it('names every invalid variable in one error', () => {
    expect(() => loadConfig({ ENGINE: 'webkit', SIDECAR_PORT: '0' })).toThrow(
      /ENGINE.*SIDECAR_PORT/,
    );
  });
});
