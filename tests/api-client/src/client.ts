/**
 * A client of the Kage API whose every path, parameter, body and answer is
 * typed by the API's own OpenAPI document (`schema.ts` is generated from
 * it): a request the document does not describe does not compile.
 */
import createClient, { type Client } from 'openapi-fetch';

import type { components, paths } from './schema.js';

export type FetchRequest = components['schemas']['FetchRequest'];
export type FetchResult = components['schemas']['FetchResult'];
export type FetchAccepted = components['schemas']['FetchAccepted'];
export type Problem = components['schemas']['Problem'];
export type KageClient = Client<paths>;

export interface KageClientOptions {
  /** Where the API listens, for example `http://127.0.0.1:8080`. */
  readonly baseUrl: string;
  /** An API key's token: `kage_<keyid>_<secret>`. */
  readonly token: string;
  /** In place of the global `fetch`. */
  readonly fetch?: typeof globalThis.fetch;
}

/** The API refused a request: its problem document (RFC 9457). */
export class KageProblemError extends Error {
  constructor(
    readonly status: number,
    readonly problem: Problem,
  ) {
    super(`${problem.code}: ${problem.detail}`);
    this.name = 'KageProblemError';
  }
}

/** A typed client that sends the key with every request. */
export function createKageClient(options: KageClientOptions): KageClient {
  return createClient<paths>({
    baseUrl: options.baseUrl,
    headers: { Authorization: `Bearer ${options.token}` },
    ...(options.fetch === undefined ? {} : { fetch: options.fetch }),
  });
}

/**
 * Fetches one page (`POST /v1/fetch`). The result of a fetch that was made
 * comes back whatever the site did: a block is its `outcome`. A fetch that
 * outlasted the server's wall time comes back as accepted, with the place
 * its result will be.
 *
 * @throws KageProblemError when the request is refused.
 */
export async function fetchPage(
  client: KageClient,
  request: FetchRequest,
): Promise<FetchResult | FetchAccepted> {
  const { data, error, response } = await client.POST('/v1/fetch', { body: request });
  if (data === undefined) {
    throw new KageProblemError(response.status, error);
  }
  return data;
}

/**
 * The result of a fetch again, by the request's id (`GET /v1/results/{id}`).
 *
 * @throws KageProblemError when there is no such result of this key.
 */
export async function readResult(
  client: KageClient,
  id: string,
): Promise<FetchResult | FetchAccepted> {
  const { data, error, response } = await client.GET('/v1/results/{id}', {
    params: { path: { id } },
  });
  if (data === undefined) {
    throw new KageProblemError(response.status, error);
  }
  return data;
}

/**
 * What was kept of a page, as bytes (`GET /v1/artifacts/{id}`).
 *
 * @throws KageProblemError when there is no such artifact of this key.
 */
export async function readArtifact(
  client: KageClient,
  id: string,
): Promise<{ contentType: string; bytes: Uint8Array }> {
  const { data, error, response } = await client.GET('/v1/artifacts/{id}', {
    params: { path: { id } },
    parseAs: 'arrayBuffer',
  });
  if (data === undefined) {
    throw new KageProblemError(response.status, error);
  }
  return {
    contentType: response.headers.get('content-type') ?? 'application/octet-stream',
    bytes: new Uint8Array(data),
  };
}
