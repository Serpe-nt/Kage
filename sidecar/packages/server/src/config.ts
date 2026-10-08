import { EngineNameSchema, type EngineName } from '@kage/contract';
import { z } from 'zod';

const LogLevelSchema = z.enum(['fatal', 'error', 'warn', 'info', 'debug', 'trace', 'silent']);
export type LogLevel = z.infer<typeof LogLevelSchema>;

/**
 * Environment variables the server reads. `ENGINE` is required and has no
 * default: a container must say which engine it runs.
 */
const EnvironmentSchema = z.object({
  ENGINE: EngineNameSchema,
  SIDECAR_HOST: z.string().min(1).default('0.0.0.0'),
  SIDECAR_PORT: z.coerce.number().int().min(1).max(65_535).default(8090),
  LOG_LEVEL: LogLevelSchema.default('info'),
});

/** Validated server configuration. */
export interface ServerConfig {
  readonly engine: EngineName;
  readonly host: string;
  readonly port: number;
  readonly logLevel: LogLevel;
}

/** The environment does not describe a valid server configuration. */
export class ConfigError extends Error {
  override readonly name = 'ConfigError';
}

/**
 * Reads the server configuration from environment variables.
 *
 * @throws {ConfigError} naming every variable that is missing or invalid.
 */
export function loadConfig(env: NodeJS.ProcessEnv): ServerConfig {
  const parsed = EnvironmentSchema.safeParse(env);
  if (!parsed.success) {
    const problems = parsed.error.issues
      .map((issue) => `${issue.path.join('.')}: ${issue.message}`)
      .join('; ');
    throw new ConfigError(`invalid sidecar configuration: ${problems}`);
  }

  return {
    engine: parsed.data.ENGINE,
    host: parsed.data.SIDECAR_HOST,
    port: parsed.data.SIDECAR_PORT,
    logLevel: parsed.data.LOG_LEVEL,
  };
}
