import { readFileSync } from 'node:fs';

import {
  EngineNameSchema,
  GlModeSchema,
  type EngineName,
  type GlMode,
  type SelftestPage,
} from '@kage/contract';
import { z } from 'zod';

const LogLevelSchema = z.enum(['fatal', 'error', 'warn', 'info', 'debug', 'trace', 'silent']);
export type LogLevel = z.infer<typeof LogLevelSchema>;

/** Shortest shared secret the server accepts. `just setup` generates 64 hexadecimal characters. */
export const MIN_TOKEN_LENGTH = 24;

const flag = z
  .enum(['true', 'false'])
  .default('false')
  .transform((value) => value === 'true');

// An empty variable is how compose passes an optional value that is not set.
const url = z.preprocess(
  (value) => (value === '' ? undefined : value),
  z.url({ protocol: /^https?$/ }).optional(),
);

/**
 * Environment variables the server reads (spec §10.1, §10.6). `ENGINE` is
 * required and has no default: a container must say which engine it runs.
 * The shared secret is required too, as `SIDECAR_TOKEN` or as a file named
 * by `SIDECAR_TOKEN_FILE`.
 */
const EnvironmentSchema = z.object({
  ENGINE: EngineNameSchema,
  SIDECAR_HOST: z.string().min(1).default('0.0.0.0'),
  SIDECAR_PORT: z.coerce.number().int().min(1).max(65_535).default(8090),
  LOG_LEVEL: LogLevelSchema.default('info'),
  SIDECAR_TOKEN: z.string().optional(),
  SIDECAR_TOKEN_FILE: z.string().optional(),
  SIDECAR_MAX_CONTEXTS: z.coerce.number().int().min(1).max(32).default(3),
  SIDECAR_CONTEXT_IDLE_MS: z.coerce.number().int().min(1000).max(86_400_000).default(120_000),
  SIDECAR_MAX_PAGES_PER_BROWSER: z.coerce.number().int().min(1).max(1_000_000).default(500),
  SIDECAR_PERSONAS_DIR: z.string().min(1).default('/var/lib/kage/personas'),
  SIDECAR_ARTIFACTS_DIR: z.string().min(1).default('/var/lib/kage/artifacts'),
  SIDECAR_ALLOW_CROSS_OS_CLAIMS: flag,
  SIDECAR_GL: GlModeSchema.default('llvmpipe'),
  SIDECAR_SELFTEST_PROBE_URL: url,
  SIDECAR_SELFTEST_ECHO_URL: url,
  SIDECAR_BODY_LIMIT_BYTES: z.coerce.number().int().min(1024).max(268_435_456).default(16_777_216),
});

/** Validated server configuration. */
export interface ServerConfig {
  readonly engine: EngineName;
  readonly host: string;
  readonly port: number;
  readonly logLevel: LogLevel;
  /** The shared secret Kage sends as `X-Kage-Sidecar-Token`. */
  readonly token: string;
  /** Most persona contexts open at once (spec §10.1). */
  readonly maxContexts: number;
  /** A context that was not used for this long is closed (spec §10.6). */
  readonly contextIdleMs: number;
  /** A context's browser is restarted after this many navigations (spec §10.6). */
  readonly maxPagesPerBrowser: number;
  /** Where the personas volume is mounted. */
  readonly personasDir: string;
  /** Where the artifacts volume shared with Kage is mounted. */
  readonly artifactsDir: string;
  /**
   * Whether a persona may claim another operating system than the host's.
   * Off by default: on the pinned Chrome such a claim is contradicted by the
   * browser's own workers (ADR 0009).
   */
  readonly allowCrossOsClaims: boolean;
  /** Where WebGL comes from. */
  readonly gl: GlMode;
  /** The self-test's default pages: Kage's own, never a third party's. */
  readonly selftestPages: readonly SelftestPage[];
  /** Largest request body. */
  readonly bodyLimitBytes: number;
}

/** The environment does not describe a valid server configuration. */
export class ConfigError extends Error {
  override readonly name = 'ConfigError';
}

function readToken(
  env: { SIDECAR_TOKEN?: string | undefined; SIDECAR_TOKEN_FILE?: string | undefined },
  readFile: (path: string) => string,
): string {
  const direct = env.SIDECAR_TOKEN ?? '';
  const file = env.SIDECAR_TOKEN_FILE ?? '';
  if (direct !== '' && file !== '') {
    throw new ConfigError(
      'invalid sidecar configuration: set SIDECAR_TOKEN or SIDECAR_TOKEN_FILE, not both',
    );
  }
  let token = direct;
  if (file !== '') {
    try {
      token = readFile(file).trim();
    } catch (error) {
      throw new ConfigError(
        `invalid sidecar configuration: SIDECAR_TOKEN_FILE cannot be read: ${error instanceof Error ? error.message : String(error)}`,
      );
    }
  }
  if (token.length < MIN_TOKEN_LENGTH) {
    // Never echo the value: say only what is wrong with it.
    throw new ConfigError(
      token === ''
        ? 'invalid sidecar configuration: SIDECAR_TOKEN is required (or SIDECAR_TOKEN_FILE): the shared secret of Kage and its sidecars'
        : `invalid sidecar configuration: SIDECAR_TOKEN must have at least ${MIN_TOKEN_LENGTH} characters`,
    );
  }
  return token;
}

/**
 * Reads the server configuration from environment variables.
 *
 * @param readFile reads the token file; injectable for tests.
 * @throws {ConfigError} naming every variable that is missing or invalid, never a secret's value.
 */
export function loadConfig(
  env: NodeJS.ProcessEnv,
  readFile: (path: string) => string = (path) => readFileSync(path, 'utf8'),
): ServerConfig {
  const parsed = EnvironmentSchema.safeParse(env);
  if (!parsed.success) {
    const problems = parsed.error.issues
      .map((issue) => `${issue.path.join('.')}: ${issue.message}`)
      .join('; ');
    throw new ConfigError(`invalid sidecar configuration: ${problems}`);
  }
  const data = parsed.data;
  const selftestPages: SelftestPage[] = [];
  if (data.SIDECAR_SELFTEST_PROBE_URL !== undefined) {
    selftestPages.push({ id: 'probe', url: data.SIDECAR_SELFTEST_PROBE_URL, kind: 'probe' });
  }
  if (data.SIDECAR_SELFTEST_ECHO_URL !== undefined) {
    selftestPages.push({ id: 'echo', url: data.SIDECAR_SELFTEST_ECHO_URL, kind: 'echo' });
  }
  return {
    engine: data.ENGINE,
    host: data.SIDECAR_HOST,
    port: data.SIDECAR_PORT,
    logLevel: data.LOG_LEVEL,
    token: readToken(data, readFile),
    maxContexts: data.SIDECAR_MAX_CONTEXTS,
    contextIdleMs: data.SIDECAR_CONTEXT_IDLE_MS,
    maxPagesPerBrowser: data.SIDECAR_MAX_PAGES_PER_BROWSER,
    personasDir: data.SIDECAR_PERSONAS_DIR,
    artifactsDir: data.SIDECAR_ARTIFACTS_DIR,
    allowCrossOsClaims: data.SIDECAR_ALLOW_CROSS_OS_CLAIMS,
    gl: data.SIDECAR_GL,
    selftestPages,
    bodyLimitBytes: data.SIDECAR_BODY_LIMIT_BYTES,
  };
}
