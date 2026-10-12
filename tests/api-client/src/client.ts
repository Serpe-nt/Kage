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
export type JobRequest = components['schemas']['JobRequest'];
export type Job = components['schemas']['JobView'];
export type JobItem = components['schemas']['JobItemView'];
/** A crawl as `POST /v1/jobs` takes it, without its `type`. */
export type CrawlRequest = components['schemas']['CrawlJob'];
/** How far a crawl has come: the `crawl` of its job. */
export type CrawlProgress = components['schemas']['CrawlView'];
/** What a page was to its crawl: the `crawl` of its item. */
export type CrawlPage = components['schemas']['CrawlItemView'];
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

/** A job that was submitted, and whether it is the one an earlier request made. */
export interface SubmittedJob {
  readonly job: Job;
  /** True when the `Idempotency-Key` named a job that existed already. */
  readonly replayed: boolean;
}

/**
 * Submits a job (`POST /v1/jobs`). With an idempotency key the same
 * request within a day is answered with the job it made the first time.
 *
 * @throws KageProblemError when the job is refused; nothing of it is stored then.
 */
export async function createJob(
  client: KageClient,
  request: JobRequest,
  idempotencyKey?: string,
): Promise<SubmittedJob> {
  const { data, error, response } = await client.POST('/v1/jobs', {
    body: request,
    params: { header: idempotencyKey === undefined ? {} : { 'Idempotency-Key': idempotencyKey } },
  });
  if (data === undefined) {
    throw new KageProblemError(response.status, error);
  }
  return { job: data, replayed: response.headers.get('idempotency-replayed') === 'true' };
}

/**
 * Submits a crawl (`POST /v1/jobs` with `type: "crawl"`): a site read
 * from its seeds along its links, the way a reader goes through it. The
 * key needs the `crawl` scope besides `jobs`. The job has no items yet:
 * one is added for every page the crawl asks for.
 *
 * @throws KageProblemError when the crawl is refused; nothing of it is stored then.
 */
export async function createCrawl(
  client: KageClient,
  crawl: CrawlRequest,
  idempotencyKey?: string,
): Promise<SubmittedJob> {
  return createJob(client, { type: 'crawl', ...crawl }, idempotencyKey);
}

/**
 * How far a crawl has come: what it discovered, fetched and skipped, how
 * its pages were reached, and why it stopped reading once it has.
 *
 * @throws Error when the job is no crawl.
 */
export function crawlProgress(job: Job): CrawlProgress {
  if (job.crawl === undefined || job.crawl === null) {
    throw new Error(`job ${job.id} is a ${job.type}, not a crawl`);
  }
  return job.crawl;
}

/** A page a crawl asked for: its item, its address and what it was to the crawl. */
export interface CrawledPage {
  readonly item: JobItem;
  readonly url: string;
  readonly page: CrawlPage;
}

/**
 * The pages of a crawl in the order it asked for them, each with its depth,
 * where its link was found, how it was reached and what it repeats
 * (`GET /v1/jobs/{id}/items`). A page that is still being read is not listed.
 *
 * @throws KageProblemError when there is no such job of this key.
 */
export async function readCrawlPages(
  client: KageClient,
  id: string,
  options: { readonly results?: boolean; readonly pageSize?: number } = {},
): Promise<CrawledPage[]> {
  const pages: CrawledPage[] = [];
  for (const item of await readJobItems(client, id, options)) {
    const { url, crawl } = item;
    if (typeof url === 'string' && crawl !== undefined && crawl !== null) {
      pages.push({ item, url, page: crawl });
    }
  }
  return pages;
}

/**
 * A job as it stands (`GET /v1/jobs/{id}`).
 *
 * @throws KageProblemError when there is no such job of this key.
 */
export async function readJob(client: KageClient, id: string): Promise<Job> {
  const { data, error, response } = await client.GET('/v1/jobs/{id}', {
    params: { path: { id } },
  });
  if (data === undefined) {
    throw new KageProblemError(response.status, error);
  }
  return data;
}

/**
 * Every item of a job, in the job's order, read page by page
 * (`GET /v1/jobs/{id}/items`).
 *
 * @throws KageProblemError when there is no such job of this key.
 */
export async function readJobItems(
  client: KageClient,
  id: string,
  options: { readonly results?: boolean; readonly pageSize?: number } = {},
): Promise<JobItem[]> {
  const items: JobItem[] = [];
  let cursor: string | undefined;
  for (;;) {
    const { data, error, response } = await client.GET('/v1/jobs/{id}/items', {
      params: {
        path: { id },
        query: {
          limit: options.pageSize ?? 50,
          ...(options.results === undefined ? {} : { results: options.results }),
          ...(cursor === undefined ? {} : { cursor }),
        },
      },
    });
    if (data === undefined) {
      throw new KageProblemError(response.status, error);
    }
    items.push(...data.items);
    if (data.next_cursor === undefined || data.next_cursor === null) {
      return items;
    }
    cursor = data.next_cursor;
  }
}

/**
 * Asks a job to stop (`POST /v1/jobs/{id}/cancel`); the answer is the job
 * as it stands, which is `cancelled` once what was under way is over.
 *
 * @throws KageProblemError when there is no such job of this key.
 */
export async function cancelJob(client: KageClient, id: string): Promise<Job> {
  const { data, error, response } = await client.POST('/v1/jobs/{id}/cancel', {
    params: { path: { id } },
  });
  if (data === undefined) {
    throw new KageProblemError(response.status, error);
  }
  return data;
}

/** Whether a job has ended. */
export function hasEnded(job: Job): boolean {
  return ['succeeded', 'partial', 'failed', 'cancelled'].includes(job.state);
}

/**
 * Reads a job until `done` says so, and returns it as it was then.
 *
 * @throws Error when it did not get there within the time.
 */
export async function waitForJob(
  client: KageClient,
  id: string,
  options: {
    readonly timeoutMs: number;
    readonly everyMs?: number;
    readonly done?: (job: Job) => boolean;
  },
): Promise<Job> {
  const done = options.done ?? hasEnded;
  const deadline = Date.now() + options.timeoutMs;
  for (;;) {
    const job = await readJob(client, id);
    if (done(job)) {
      return job;
    }
    if (Date.now() >= deadline) {
      throw new Error(
        `job ${id} is ${job.state} after ${options.timeoutMs} ms: ${JSON.stringify(job.items)}`,
      );
    }
    await new Promise((resolve) => setTimeout(resolve, options.everyMs ?? 1000));
  }
}
