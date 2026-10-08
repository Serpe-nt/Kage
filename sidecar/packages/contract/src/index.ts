import { z } from 'zod';

/** Version of the sidecar HTTP contract. It is also the path prefix. */
export const CONTRACT_VERSION = 'v1';

/** Path of the health endpoint (spec §10.2). */
export const HEALTH_PATH = `/${CONTRACT_VERSION}/health`;

/** Browser engines the sidecar can run. One container runs exactly one. */
export const EngineNameSchema = z.enum(['chromium', 'firefox']);
export type EngineName = z.infer<typeof EngineNameSchema>;

/** Response of `GET /v1/health` (spec §10.2). */
export const HealthResponseSchema = z.strictObject({
  status: z.literal('ok'),
  engine: EngineNameSchema,
  /** Version of the browser the engine drives; `null` while no browser is running. */
  browser_version: z.string().min(1).nullable(),
  /** Persona contexts currently open. */
  contexts_in_use: z.number().int().nonnegative(),
  /** Seconds since the server started. */
  uptime_seconds: z.number().nonnegative(),
});
export type HealthResponse = z.infer<typeof HealthResponseSchema>;
