import { HEALTH_PATH, HealthResponseSchema, type HealthResponse } from '@kage/contract';

/** Upper bound for one probe. The container healthcheck adds its own timeout. */
const PROBE_TIMEOUT_MS = 3000;

/**
 * Calls the health endpoint of the server at `origin` and validates the body
 * against the contract.
 *
 * @throws when the server is unreachable, answers a non-2xx status or
 *   returns a body that does not match the contract.
 */
export async function probeHealth(origin: string): Promise<HealthResponse> {
  const response = await fetch(new URL(HEALTH_PATH, origin), {
    signal: AbortSignal.timeout(PROBE_TIMEOUT_MS),
  });
  if (!response.ok) {
    throw new Error(`health endpoint answered ${response.status}`);
  }
  return HealthResponseSchema.parse(await response.json());
}
