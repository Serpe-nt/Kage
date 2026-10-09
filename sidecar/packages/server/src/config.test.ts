import { describe, expect, it } from 'vitest';

import { ConfigError, MIN_TOKEN_LENGTH, loadConfig } from './config.js';

const TOKEN = 'a-shared-secret-of-enough-length';
const base = { ENGINE: 'chromium', SIDECAR_TOKEN: TOKEN };

describe('loadConfig', () => {
  it.each(['chromium', 'firefox'] as const)(
    'selects the %s engine from ENGINE, with the spec defaults',
    (engine) => {
      expect(loadConfig({ ...base, ENGINE: engine })).toEqual({
        engine,
        host: '0.0.0.0',
        port: 8090,
        logLevel: 'info',
        token: TOKEN,
        maxContexts: 3,
        contextIdleMs: 120_000,
        maxPagesPerBrowser: 500,
        personasDir: '/var/lib/kage/personas',
        artifactsDir: '/var/lib/kage/artifacts',
        allowCrossOsClaims: false,
        gl: 'llvmpipe',
        audio: 'none',
        selftestPages: [],
        bodyLimitBytes: 16_777_216,
      });
    },
  );

  it('reads every override', () => {
    const config = loadConfig({
      ...base,
      ENGINE: 'firefox',
      SIDECAR_HOST: '127.0.0.1',
      SIDECAR_PORT: '9000',
      LOG_LEVEL: 'debug',
      SIDECAR_MAX_CONTEXTS: '6',
      SIDECAR_CONTEXT_IDLE_MS: '30000',
      SIDECAR_MAX_PAGES_PER_BROWSER: '50',
      SIDECAR_PERSONAS_DIR: '/data/personas',
      SIDECAR_ARTIFACTS_DIR: '/data/artifacts',
      SIDECAR_ALLOW_CROSS_OS_CLAIMS: 'true',
      SIDECAR_GL: 'host',
      SIDECAR_AUDIO: 'pulse',
      SIDECAR_SELFTEST_PROBE_URL: 'https://echo.example.org/probe/',
      SIDECAR_SELFTEST_ECHO_URL: 'https://echo.example.org/',
      SIDECAR_BODY_LIMIT_BYTES: '1048576',
    });
    expect(config).toMatchObject({
      engine: 'firefox',
      host: '127.0.0.1',
      port: 9000,
      logLevel: 'debug',
      maxContexts: 6,
      contextIdleMs: 30_000,
      maxPagesPerBrowser: 50,
      personasDir: '/data/personas',
      artifactsDir: '/data/artifacts',
      allowCrossOsClaims: true,
      gl: 'host',
      audio: 'pulse',
      bodyLimitBytes: 1_048_576,
    });
    // The default self-test pages are Kage's own, never a third party's.
    expect(config.selftestPages).toEqual([
      { id: 'probe', url: 'https://echo.example.org/probe/', kind: 'probe' },
      { id: 'echo', url: 'https://echo.example.org/', kind: 'echo' },
    ]);
  });

  it('takes an empty self-test URL for an unset one, as compose passes it', () => {
    const config = loadConfig({
      ...base,
      SIDECAR_SELFTEST_PROBE_URL: '',
      SIDECAR_SELFTEST_ECHO_URL: '',
    });
    expect(config.selftestPages).toEqual([]);
  });

  it('requires ENGINE: there is no default engine', () => {
    expect(() => loadConfig({ SIDECAR_TOKEN: TOKEN })).toThrow(ConfigError);
    expect(() => loadConfig({ SIDECAR_TOKEN: TOKEN })).toThrow(/ENGINE/);
  });

  it('requires the shared secret and never repeats it', () => {
    expect(() => loadConfig({ ENGINE: 'chromium' })).toThrow(/SIDECAR_TOKEN is required/);
    expect(() => loadConfig({ ENGINE: 'chromium', SIDECAR_TOKEN: '' })).toThrow(
      /SIDECAR_TOKEN is required/,
    );
    const short = 'x'.repeat(MIN_TOKEN_LENGTH - 1);
    const error = (() => {
      try {
        loadConfig({ ENGINE: 'chromium', SIDECAR_TOKEN: short });
        return null;
      } catch (thrown) {
        return thrown as Error;
      }
    })();
    expect(error).toBeInstanceOf(ConfigError);
    expect(error?.message).toContain(`at least ${MIN_TOKEN_LENGTH} characters`);
    expect(error?.message).not.toContain(short);
  });

  it('reads the secret from a file, and refuses both forms at once', () => {
    const files: Record<string, string> = { '/run/secrets/token': `${TOKEN}\r\n` };
    const read = (path: string) => {
      const text = files[path];
      if (text === undefined) {
        throw new Error(`ENOENT: ${path}`);
      }
      return text;
    };
    expect(
      loadConfig({ ENGINE: 'chromium', SIDECAR_TOKEN_FILE: '/run/secrets/token' }, read).token,
    ).toBe(TOKEN);
    // An empty variable does not conflict with its file form.
    expect(
      loadConfig(
        { ENGINE: 'chromium', SIDECAR_TOKEN: '', SIDECAR_TOKEN_FILE: '/run/secrets/token' },
        read,
      ).token,
    ).toBe(TOKEN);
    expect(() => loadConfig({ ...base, SIDECAR_TOKEN_FILE: '/run/secrets/token' }, read)).toThrow(
      /not both/,
    );
    expect(() => loadConfig({ ENGINE: 'chromium', SIDECAR_TOKEN_FILE: '/missing' }, read)).toThrow(
      /SIDECAR_TOKEN_FILE cannot be read: ENOENT/,
    );
    expect(() => loadConfig({ ENGINE: 'chromium', SIDECAR_TOKEN_FILE: '/missing' })).toThrow(
      ConfigError,
    );
  });

  it.each([
    ['an unknown engine', { ENGINE: 'webkit' }, /ENGINE/],
    ['an engine in the wrong case', { ENGINE: 'Chromium' }, /ENGINE/],
    ['a port that is not a number', { SIDECAR_PORT: 'http' }, /SIDECAR_PORT/],
    ['a port out of range', { SIDECAR_PORT: '70000' }, /SIDECAR_PORT/],
    ['port zero', { SIDECAR_PORT: '0' }, /SIDECAR_PORT/],
    ['an unknown log level', { LOG_LEVEL: 'loud' }, /LOG_LEVEL/],
    ['no context at all', { SIDECAR_MAX_CONTEXTS: '0' }, /SIDECAR_MAX_CONTEXTS/],
    ['an idle time under a second', { SIDECAR_CONTEXT_IDLE_MS: '10' }, /SIDECAR_CONTEXT_IDLE_MS/],
    [
      'a page limit of zero',
      { SIDECAR_MAX_PAGES_PER_BROWSER: '0' },
      /SIDECAR_MAX_PAGES_PER_BROWSER/,
    ],
    [
      'a flag that is not true or false',
      { SIDECAR_ALLOW_CROSS_OS_CLAIMS: 'yes' },
      /SIDECAR_ALLOW_CROSS_OS_CLAIMS/,
    ],
    ['an unknown GL mode', { SIDECAR_GL: 'nvidia' }, /SIDECAR_GL/],
    [
      'a probe URL that is none',
      { SIDECAR_SELFTEST_PROBE_URL: 'probe' },
      /SIDECAR_SELFTEST_PROBE_URL/,
    ],
    [
      'an echo URL with another scheme',
      { SIDECAR_SELFTEST_ECHO_URL: 'ftp://echo.example.org/' },
      /SIDECAR_SELFTEST_ECHO_URL/,
    ],
  ])('rejects %s', (_label, env, variable) => {
    expect(() => loadConfig({ ...base, ...env })).toThrow(ConfigError);
    expect(() => loadConfig({ ...base, ...env })).toThrow(variable);
  });

  it('names every invalid variable in one error', () => {
    expect(() => loadConfig({ ...base, ENGINE: 'webkit', SIDECAR_PORT: '0' })).toThrow(
      /ENGINE.*SIDECAR_PORT/,
    );
  });
});
