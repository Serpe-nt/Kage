import assert from 'node:assert/strict';
import { createServer, type IncomingMessage, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { after, before, describe, it } from 'node:test';

import {
  cancelJob,
  createJob,
  createKageClient,
  fetchPage,
  hasEnded,
  KageProblemError,
  readArtifact,
  readJob,
  readJobItems,
  readResult,
  waitForJob,
  type FetchResult,
  type Job,
} from './client.js';

const TOKEN = 'kage_0190a0b000007000800000000000000a_dGVzdC1vbmx5';
const ID = '0190a0b0-0000-7000-8000-000000000001';
const JOB = '0190a0b0-0000-7000-8000-00000000000b';

/** A result as the API returns one, with what a test names. */
function result(over: Partial<FetchResult>): FetchResult {
  return {
    id: ID,
    state: 'finished',
    outcome: 'ok',
    status: 200,
    redirects: [],
    artifacts: {},
    side_effects: [],
    timing: { total_ms: 12 },
    attempts: [],
    cached: false,
    escalated: false,
    created_at: '2026-10-11T12:00:00Z',
    ...over,
  };
}

/** A job as the API returns one, with what a test names. */
function job(over: Partial<Job>): Job {
  return {
    id: JOB,
    type: 'batch',
    state: 'queued',
    origin: 'api',
    cancel_requested: false,
    items: { total: 3, succeeded: 0, failed: 0, cancelled: 0, pending: 3 },
    items_url: `/v1/jobs/${JOB}/items`,
    created_at: '2026-10-11T12:00:00Z',
    ...over,
  };
}

interface Seen {
  method: string;
  url: string;
  authorization: string | undefined;
  contentType: string | undefined;
  idempotencyKey: string | undefined;
  body: string;
}

async function read(request: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) {
    chunks.push(chunk as Buffer);
  }
  return Buffer.concat(chunks).toString('utf8');
}

describe('the generated client', () => {
  const seen: Seen[] = [];
  let server: Server;
  let baseUrl = '';
  /** How often the job was read: it ends at the third reading. */
  let readings = 0;

  before(async () => {
    // What the API answers, as far as these tests ask.
    server = createServer((request, response) => {
      void read(request).then((body) => {
        seen.push({
          method: request.method ?? '',
          url: request.url ?? '',
          authorization: request.headers.authorization,
          contentType: request.headers['content-type'],
          idempotencyKey: request.headers['idempotency-key'] as string | undefined,
          body,
        });
        const json = (status: number, document: unknown, type = 'application/json') => {
          response.writeHead(status, { 'content-type': type, 'x-request-id': ID });
          response.end(JSON.stringify(document));
        };
        if (request.headers.authorization !== `Bearer ${TOKEN}`) {
          json(
            401,
            {
              type: 'urn:kage:problem:invalid_key',
              title: 'The API key is not valid',
              status: 401,
              detail: 'the API key is not valid',
              code: 'invalid_key',
              request_id: ID,
            },
            'application/problem+json',
          );
        } else if (request.method === 'POST' && request.url === '/v1/fetch') {
          const asked = JSON.parse(body) as { url: string };
          if (asked.url.startsWith('http://127.')) {
            json(
              422,
              {
                type: 'urn:kage:problem:ssrf_denied',
                title: 'The address is not one Kage fetches from',
                status: 422,
                detail: 'address 127.0.0.1 is in a denied range',
                code: 'ssrf_denied',
                request_id: ID,
                field: 'url',
              },
              'application/problem+json',
            );
          } else if (asked.url.endsWith('/slow')) {
            json(202, { id: ID, state: 'running', job_id: null, result: `/v1/results/${ID}` });
          } else {
            json(200, result({ final_url: asked.url, artifacts: { dom_html: 'art_1' } }));
          }
        } else if (request.method === 'POST' && request.url === '/v1/jobs') {
          const asked = JSON.parse(body) as { type: string };
          if (asked.type === 'crawl') {
            json(
              422,
              {
                type: 'urn:kage:problem:not_available_yet',
                title: 'This part of the API does not exist yet',
                status: 422,
                detail: 'crawl jobs arrive with milestone M10',
                code: 'not_available_yet',
                request_id: ID,
              },
              'application/problem+json',
            );
          } else if (request.headers['idempotency-key'] === 'again') {
            response.writeHead(200, {
              'content-type': 'application/json',
              'idempotency-replayed': 'true',
            });
            response.end(JSON.stringify(job({ state: 'running' })));
          } else {
            response.writeHead(201, {
              'content-type': 'application/json',
              location: `/v1/jobs/${JOB}`,
            });
            response.end(JSON.stringify(job({})));
          }
        } else if (request.method === 'GET' && request.url === `/v1/jobs/${JOB}`) {
          readings += 1;
          json(
            200,
            readings < 3
              ? job({ state: 'running' })
              : job({
                  state: 'succeeded',
                  items: { total: 3, succeeded: 3, failed: 0, cancelled: 0, pending: 0 },
                }),
          );
        } else if (
          request.method === 'GET' &&
          (request.url ?? '').startsWith(`/v1/jobs/${JOB}/items`)
        ) {
          const query = new URL(request.url ?? '', 'http://api.invalid').searchParams;
          const item = (seq: number) => ({
            seq,
            state: 'succeeded',
            url: `https://example.org/${seq}`,
            outcome: 'ok',
            request_id: ID,
            ...(query.get('results') === 'false' ? {} : { result: result({}) }),
          });
          json(
            200,
            query.get('cursor') === null
              ? { items: [item(0), item(1)], next_cursor: '1' }
              : { items: [item(2)], next_cursor: null },
          );
        } else if (request.method === 'POST' && request.url === `/v1/jobs/${JOB}/cancel`) {
          json(200, job({ state: 'running', cancel_requested: true }));
        } else if (request.method === 'GET' && request.url === `/v1/results/${ID}`) {
          json(200, result({ outcome: 'forbidden', status: 403 }));
        } else if (request.method === 'GET' && request.url === '/v1/artifacts/art_1') {
          response.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
          response.end('<h1>kept</h1>');
        } else {
          json(
            404,
            {
              type: 'urn:kage:problem:artifact_not_found',
              title: 'There is no such artifact',
              status: 404,
              detail: 'there is no such thing among what this key fetched',
              code: 'artifact_not_found',
            },
            'application/problem+json',
          );
        }
      });
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    baseUrl = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
  });

  after(async () => {
    await new Promise((resolve) => server.close(resolve));
  });

  it('fetches a page with the key and the body the document describes', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    const fetched = await fetchPage(client, {
      url: 'https://example.org/listing',
      render: 'browser',
      include: ['dom', 'headers'],
      persona: { engines: ['firefox'] },
      behavior: { scroll_depth: 0.3, dwell_ms: 3000 },
      timeout_ms: 60_000,
    });

    assert.equal(fetched.state, 'finished');
    assert.ok('outcome' in fetched);
    assert.equal(fetched.outcome, 'ok');
    assert.equal(fetched.final_url, 'https://example.org/listing');
    const sent = seen.at(-1);
    assert.equal(sent?.method, 'POST');
    assert.equal(sent?.url, '/v1/fetch');
    assert.equal(sent?.authorization, `Bearer ${TOKEN}`);
    assert.equal(sent?.contentType, 'application/json');
    assert.deepEqual(JSON.parse(sent?.body ?? ''), {
      url: 'https://example.org/listing',
      render: 'browser',
      include: ['dom', 'headers'],
      persona: { engines: ['firefox'] },
      behavior: { scroll_depth: 0.3, dwell_ms: 3000 },
      timeout_ms: 60_000,
    });
  });

  it('returns a fetch that runs on as accepted, and a block as a result', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });

    const accepted = await fetchPage(client, { url: 'https://example.org/slow' });
    assert.equal(accepted.state, 'running');
    assert.ok('result' in accepted);
    assert.equal(accepted.result, `/v1/results/${ID}`);

    const blocked = await readResult(client, ID);
    assert.ok('outcome' in blocked);
    assert.equal(blocked.outcome, 'forbidden');
  });

  it('reads an artifact as bytes', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    const kept = await readArtifact(client, 'art_1');
    assert.equal(kept.contentType, 'text/html; charset=utf-8');
    assert.equal(Buffer.from(kept.bytes).toString('utf8'), '<h1>kept</h1>');
  });

  it('submits a job under a key that names it, and knows the job a key made before', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    const request = {
      type: 'batch',
      urls: ['https://example.org/0', 'https://example.org/1'],
      items: [{ url: 'https://example.org/2', include: ['dom'] }],
      defaults: { behavior: { scroll_depth: 0.3, dwell_ms: 3000 } },
      visitors: 1,
      webhook: {
        url: 'https://hooks.example.org/kage',
        secret: 'a secret of sixteen bytes or more',
        progress_every: 2,
      },
    } as const;

    const first = await createJob(
      client,
      {
        ...request,
        urls: [...request.urls],
        items: [{ url: 'https://example.org/2', include: ['dom'] }],
      },
      'nightly-1',
    );
    assert.equal(first.replayed, false);
    assert.equal(first.job.id, JOB);
    assert.equal(first.job.state, 'queued');
    const sent = seen.at(-1);
    assert.equal(sent?.method, 'POST');
    assert.equal(sent?.url, '/v1/jobs');
    assert.equal(sent?.idempotencyKey, 'nightly-1');
    assert.deepEqual(JSON.parse(sent?.body ?? ''), request);

    const again = await createJob(
      client,
      { type: 'flow', actions: [{ action: 'goto', url: 'https://example.org/' }] },
      'again',
    );
    assert.equal(again.replayed, true);
    assert.equal(again.job.id, JOB);
    // Without a key no header is sent.
    await createJob(client, { type: 'batch', urls: ['https://example.org/0'] });
    assert.equal(seen.at(-1)?.idempotencyKey, undefined);
  });

  it('reads the items of a job page by page, with or without their results', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });

    const items = await readJobItems(client, JOB, { pageSize: 2 });
    assert.deepEqual(
      items.map((item) => item.seq),
      [0, 1, 2],
    );
    assert.equal(items[2]?.result?.outcome, 'ok');
    const asked = seen.slice(-2).map((request) => request.url);
    assert.deepEqual(asked, [
      `/v1/jobs/${JOB}/items?limit=2`,
      `/v1/jobs/${JOB}/items?limit=2&cursor=1`,
    ]);

    const bare = await readJobItems(client, JOB, { results: false });
    assert.equal(bare.length, 3);
    assert.ok(bare.every((item) => item.result === undefined));
    assert.match(seen.at(-1)?.url ?? '', /limit=50&results=false&cursor=1$/);
  });

  it('waits for a job to end, and asks one to stop', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    readings = 0;

    assert.equal(hasEnded(await readJob(client, JOB)), false);
    const done = await waitForJob(client, JOB, { timeoutMs: 5000, everyMs: 5 });
    assert.equal(done.state, 'succeeded');
    assert.equal(done.items.pending, 0);
    assert.equal(readings, 3);
    // What it waits for may be something else than the end.
    const seenAgain = await waitForJob(client, JOB, {
      timeoutMs: 5000,
      done: (seen) => seen.items.succeeded >= 2,
    });
    assert.equal(seenAgain.items.succeeded, 3);
    // And it does not wait for ever.
    await assert.rejects(
      waitForJob(client, JOB, { timeoutMs: 20, everyMs: 5, done: () => false }),
      /is succeeded after 20 ms/,
    );

    const stopping = await cancelJob(client, JOB);
    assert.equal(stopping.cancel_requested, true);
    assert.equal(seen.at(-1)?.url, `/v1/jobs/${JOB}/cancel`);
    assert.equal(seen.at(-1)?.method, 'POST');
  });

  it('throws the problem document of a job that is refused or is not there', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    // The document describes only what the API takes: a crawl is not in it yet.
    const crawl = { type: 'crawl' } as unknown as Parameters<typeof createJob>[1];
    await assert.rejects(createJob(client, crawl), (error: unknown) => {
      assert.ok(error instanceof KageProblemError);
      assert.equal(error.status, 422);
      assert.equal(error.problem.code, 'not_available_yet');
      return true;
    });
    const other = '0190a0b0-0000-7000-8000-00000000000c';
    for (const call of [
      () => readJob(client, other),
      () => readJobItems(client, other),
      () => cancelJob(client, other),
    ]) {
      await assert.rejects(call(), (error: unknown) => {
        assert.ok(error instanceof KageProblemError);
        assert.equal(error.status, 404);
        return true;
      });
    }
  });

  it('throws the problem document of a refusal', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    await assert.rejects(fetchPage(client, { url: 'http://127.0.0.1/admin' }), (error: unknown) => {
      assert.ok(error instanceof KageProblemError);
      assert.equal(error.status, 422);
      assert.equal(error.problem.code, 'ssrf_denied');
      assert.equal(error.problem['field'], 'url');
      assert.match(error.message, /^ssrf_denied: /);
      return true;
    });
    await assert.rejects(readArtifact(client, 'art_2'), (error: unknown) => {
      assert.ok(error instanceof KageProblemError);
      assert.equal(error.problem.code, 'artifact_not_found');
      return true;
    });

    const stranger = createKageClient({ baseUrl, token: 'kage_wrong', fetch: globalThis.fetch });
    await assert.rejects(readResult(stranger, ID), (error: unknown) => {
      assert.ok(error instanceof KageProblemError);
      assert.equal(error.status, 401);
      assert.equal(error.problem.code, 'invalid_key');
      return true;
    });
  });
});
