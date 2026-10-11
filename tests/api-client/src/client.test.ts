import assert from 'node:assert/strict';
import { createServer, type IncomingMessage, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { after, before, describe, it } from 'node:test';

import {
  createKageClient,
  fetchPage,
  KageProblemError,
  readArtifact,
  readResult,
  type FetchResult,
} from './client.js';

const TOKEN = 'kage_0190a0b000007000800000000000000a_dGVzdC1vbmx5';
const ID = '0190a0b0-0000-7000-8000-000000000001';

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

interface Seen {
  method: string;
  url: string;
  authorization: string | undefined;
  contentType: string | undefined;
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

  before(async () => {
    // What the API answers, as far as these tests ask.
    server = createServer((request, response) => {
      void read(request).then((body) => {
        seen.push({
          method: request.method ?? '',
          url: request.url ?? '',
          authorization: request.headers.authorization,
          contentType: request.headers['content-type'],
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
