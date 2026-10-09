import { z } from 'zod';

import { EngineNameSchema } from './common.js';

/** Response of `GET /v1/health` (spec §10.2). */
export const HealthResponseSchema = z
  .strictObject({
    status: z.literal('ok').describe('Always `ok`: an unhealthy server does not answer 200.'),
    engine: EngineNameSchema.describe('The engine this container runs.'),
    browser_version: z
      .string()
      .min(1)
      .nullable()
      .describe(
        'Full version of the browser the engine drives, for example `155.0.8059.39`; null when the engine is a stub without a browser.',
      ),
    contexts_in_use: z.number().int().nonnegative().describe('Persona contexts currently open.'),
    max_contexts: z
      .number()
      .int()
      .positive()
      .describe('The most contexts this container opens at once (`SIDECAR_MAX_CONTEXTS`).'),
    uptime_seconds: z.number().nonnegative().describe('Seconds since the server started.'),
    contract_version: z.string().describe('Semantic version of the contract this server speaks.'),
  })
  .meta({ id: 'HealthResponse', description: 'What `GET /v1/health` answers.' });
export type HealthResponse = z.infer<typeof HealthResponseSchema>;
