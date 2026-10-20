import assert from 'node:assert/strict';
import { createServer, type IncomingMessage, type Server } from 'node:http';
import type { AddressInfo } from 'node:net';
import { after, before, describe, it } from 'node:test';

import {
  cachedAt,
  cancelJob,
  crawlProgress,
  createCrawl,
  createJob,
  createKageClient,
  extractOf,
  fetchPage,
  hasEnded,
  isResult,
  KageProblemError,
  readArtifact,
  readCrawlPages,
  readJob,
  readJobItems,
  readResult,
  robotsDenial,
  waitForJob,
  type CrawlProgress,
  type Extract,
  type FetchRequest,
  type FetchResult,
  type Job,
} from './client.js';

const TOKEN = 'kage_0190a0b000007000800000000000000a_dGVzdC1vbmx5';
const ID = '0190a0b0-0000-7000-8000-000000000001';
const JOB = '0190a0b0-0000-7000-8000-00000000000b';
const CRAWL = '0190a0b0-0000-7000-8000-00000000000d';

/** How far a crawl has come, with what a test names. */
function progress(over: Partial<CrawlProgress>): CrawlProgress {
  return {
    discovered: 0,
    waiting: 0,
    fetching: 0,
    fetched: 0,
    failed: 0,
    skipped: {},
    dropped: 0,
    duplicates: 0,
    near_duplicates: 0,
    depth_reached: 0,
    pages: 0,
    max_pages: 50,
    max_depth: 2,
    reached: { typed: 0, click: 0, back_click: 0, referer: 0 },
    sitemaps: [],
    ...over,
  };
}

/** A result as the API returns one, with what a test names. */
function result(over: Partial<FetchResult>): FetchResult {
  return {
    id: ID,
    state: 'finished',
    outcome: 'ok',
    level: 3,
    status: 200,
    redirects: [],
    artifacts: {},
    side_effects: [],
    timing: { total_ms: 12 },
    attempts: [],
    cached: false,
    created_at: '2026-10-11T12:00:00Z',
    ...over,
  };
}

/** What a page says, with what a test names. */
function extract(over: Partial<Extract>): Extract {
  return {
    title: 'The budget',
    meta: {},
    opengraph: {},
    twitter: {},
    jsonld: [],
    jsonld_skipped: 0,
    links: [],
    links_truncated: false,
    headings: [{ level: 1, text: 'The budget' }],
    text: 'The council met.',
    markdown: '# The budget\n\nThe council met.',
    word_count: 3,
    thin_content: true,
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
          const asked = JSON.parse(body) as { url: string; level?: number };
          if (asked.level === undefined) {
            // Every request names its level: nothing is decided for the caller.
            json(
              422,
              {
                type: 'urn:kage:problem:level_required',
                title: 'The request names no level',
                status: 422,
                detail: 'level is required: 0, 1, 2 or 3',
                code: 'level_required',
                request_id: ID,
                field: 'level',
              },
              'application/problem+json',
            );
          } else if (asked.level === 1) {
            json(
              422,
              {
                type: 'urn:kage:problem:level_unavailable',
                title: 'No persona of this installation can be fetched at this level',
                status: 422,
                detail: 'no browser version of this installation has a verified http profile',
                code: 'level_unavailable',
                request_id: ID,
                field: 'level',
              },
              'application/problem+json',
            );
          } else if (asked.url.startsWith('http://127.')) {
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
          } else if (asked.url.endsWith('/plain')) {
            // A plain client's: no persona, and the exit it left from.
            json(
              200,
              result({
                level: 0,
                final_url: asked.url,
                raw_body: '<h1>plain</h1>',
                persona: null,
                attempts: [
                  {
                    n: 1,
                    render: 'http',
                    outcome: 'ok',
                    status: 200,
                    total_ms: 9,
                    persona: null,
                    engine: null,
                    exit: 'pool',
                  },
                ],
              }),
            );
          } else if (asked.url.endsWith('/private/ledger')) {
            // The site's robots.txt is enforced and disallows the address.
            json(
              200,
              result({
                outcome: 'robots_denied',
                status: null,
                robots: {
                  fetched: true,
                  allowed: false,
                  matched_rule: 'Disallow: /private',
                  crawl_delay: 2.5,
                },
              }),
            );
          } else if (asked.url.endsWith('/unknown')) {
            json(
              200,
              result({
                outcome: 'robots_denied',
                status: null,
                reason: 'robots_unavailable',
                retry_at: '2026-10-11T13:00:00Z',
                robots: { fetched: false, allowed: true },
              }),
            );
          } else if (asked.url.endsWith('/too-long')) {
            json(
              422,
              {
                type: 'urn:kage:problem:cache_ttl_too_long',
                title: 'Results are not served from the cache for that long',
                status: 422,
                detail: 'cache_ttl_s is at most 86400 for example.org',
                code: 'cache_ttl_too_long',
                request_id: ID,
                field: 'cache_ttl_s',
                maximum_s: 86_400,
              },
              'application/problem+json',
            );
          } else if (asked.url.endsWith('/budget')) {
            // The second request for the page is answered from the cache.
            const again = seen.filter((one) => one.body.includes('/budget')).length > 1;
            json(
              200,
              result({
                final_url: asked.url,
                extract: extract({ canonical: asked.url, thin_content: false, word_count: 300 }),
                robots: { fetched: false, allowed: true },
                ...(again ? { cached: true, cached_at: '2026-10-11T11:58:00Z' } : {}),
              }),
            );
          } else {
            json(200, result({ final_url: asked.url, artifacts: { dom_html: 'art_1' } }));
          }
        } else if (request.method === 'POST' && request.url === '/v1/jobs') {
          const asked = JSON.parse(body) as { type: string; seeds?: string[] };
          if (asked.type === 'crawl' && (asked.seeds ?? []).length === 0) {
            json(
              422,
              {
                type: 'urn:kage:problem:validation_failed',
                title: 'A field of the request is not acceptable',
                status: 422,
                detail: 'a crawl starts somewhere: at least one seed',
                code: 'validation_failed',
                request_id: ID,
                field: 'seeds',
              },
              'application/problem+json',
            );
          } else if (asked.type === 'crawl') {
            response.writeHead(201, {
              'content-type': 'application/json',
              location: `/v1/jobs/${CRAWL}`,
            });
            response.end(
              JSON.stringify(
                job({
                  id: CRAWL,
                  type: 'crawl',
                  items: { total: 0, succeeded: 0, failed: 0, cancelled: 0, pending: 0 },
                  items_url: `/v1/jobs/${CRAWL}/items`,
                  visitors: 1,
                  crawl: progress({}),
                }),
              ),
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
        } else if (request.method === 'GET' && request.url === `/v1/jobs/${CRAWL}`) {
          json(
            200,
            job({
              id: CRAWL,
              type: 'crawl',
              state: 'succeeded',
              items: { total: 3, succeeded: 3, failed: 0, cancelled: 0, pending: 0 },
              crawl: progress({
                discovered: 6,
                fetched: 3,
                skipped: { too_deep: 2, hidden: 1 },
                duplicates: 1,
                depth_reached: 2,
                pages: 3,
                reached: { typed: 1, click: 1, back_click: 1, referer: 0 },
                ended: 'completed',
              }),
            }),
          );
        } else if (
          request.method === 'GET' &&
          (request.url ?? '').startsWith(`/v1/jobs/${CRAWL}/items`)
        ) {
          const page = (seq: number, path: string, crawl: object) => ({
            seq,
            state: 'succeeded',
            url: `https://example.org${path}`,
            outcome: 'ok',
            request_id: ID,
            crawl: { depth: seq === 0 ? 0 : 1, found_by: seq === 0 ? 'seed' : 'link', ...crawl },
          });
          json(200, {
            items: [
              page(0, '/', { reached: 'typed', links: { found: 2, new: 2 } }),
              page(1, '/a', {
                found_on: 'https://example.org/',
                reached: 'click',
                backs: 0,
                content_hash: 'ab'.repeat(32),
              }),
              page(2, '/b', {
                found_on: 'https://example.org/',
                reached: 'back_click',
                backs: 1,
                duplicate_of: 'https://example.org/a',
              }),
              // A page that is being read has nothing to say yet.
              { seq: 3, state: 'running', url: 'https://example.org/c' },
            ],
            next_cursor: null,
          });
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
      level: 3,
      include: ['dom', 'headers'],
      persona: { engines: ['firefox'] },
      behavior: { scroll_depth: 0.3, dwell_ms: 3000 },
      timeout_ms: 60_000,
    });

    assert.equal(fetched.state, 'finished');
    assert.ok('outcome' in fetched);
    assert.equal(fetched.outcome, 'ok');
    assert.equal(fetched.level, 3);
    assert.equal(fetched.final_url, 'https://example.org/listing');
    const sent = seen.at(-1);
    assert.equal(sent?.method, 'POST');
    assert.equal(sent?.url, '/v1/fetch');
    assert.equal(sent?.authorization, `Bearer ${TOKEN}`);
    assert.equal(sent?.contentType, 'application/json');
    assert.deepEqual(JSON.parse(sent?.body ?? ''), {
      url: 'https://example.org/listing',
      level: 3,
      include: ['dom', 'headers'],
      persona: { engines: ['firefox'] },
      behavior: { scroll_depth: 0.3, dwell_ms: 3000 },
      timeout_ms: 60_000,
    });
  });

  it('returns a fetch that runs on as accepted, and a block as a result', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });

    const accepted = await fetchPage(client, { url: 'https://example.org/slow', level: 3 });
    assert.equal(accepted.state, 'running');
    assert.ok('result' in accepted);
    assert.equal(accepted.result, `/v1/results/${ID}`);

    const blocked = await readResult(client, ID);
    assert.ok('outcome' in blocked);
    assert.equal(blocked.outcome, 'forbidden');
  });

  it('asks for what a page says, for the cache and about robots.txt, as the document types them', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    const asking = {
      url: 'https://example.org/budget',
      level: 3,
      include: ['headers', 'extract'],
      extract: { markdown: true, links: false },
      cache_ttl_s: 600,
      robots: 'report',
    } as const;

    const first = await fetchPage(client, { ...asking, include: [...asking.include] });
    assert.ok(isResult(first));
    assert.deepEqual(JSON.parse(seen.at(-1)?.body ?? ''), asking);
    const said = extractOf(first);
    assert.equal(said.title, 'The budget');
    assert.equal(said.canonical, 'https://example.org/budget');
    assert.equal(said.headings[0]?.level, 1);
    assert.equal(said.thin_content, false);
    assert.equal(first.robots?.fetched, false);
    assert.equal(first.robots?.allowed, true);
    assert.equal(cachedAt(first), undefined);
    assert.equal(robotsDenial(first), undefined);

    // The same again is answered from the cache, and says since when.
    const second = await fetchPage(client, { ...asking, include: [...asking.include] });
    assert.ok(isResult(second));
    assert.equal(second.cached, true);
    assert.equal(cachedAt(second)?.toISOString(), '2026-10-11T11:58:00.000Z');
    assert.deepEqual(second.attempts, []);
    assert.equal(extractOf(second).word_count, 300);

    // A result without an extraction says so when it is asked for one, and
    // a fetch that runs on is no result yet.
    const plain = await fetchPage(client, { url: 'https://example.org/listing', level: 3 });
    assert.ok(isResult(plain));
    assert.throws(() => extractOf(plain), /has no extract/);
    assert.throws(() => cachedAt({ ...plain, cached: true }), /does not say since when/);
    assert.equal(
      isResult(await fetchPage(client, { url: 'https://example.org/slow', level: 3 })),
      false,
    );

    // More than a site's results may be served for is refused, with the most.
    await assert.rejects(
      fetchPage(client, { url: 'https://example.org/too-long', level: 3, cache_ttl_s: 86_401 }),
      (error: unknown) => {
        assert.ok(error instanceof KageProblemError);
        assert.equal(error.status, 422);
        assert.equal(error.problem.code, 'cache_ttl_too_long');
        assert.equal(error.problem['maximum_s'], 86_400);
        return true;
      },
    );
  });

  it('says why a page was not fetched for the sake of robots.txt', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });

    // A rule of the file: the result is a result, and names the rule.
    const denied = await fetchPage(client, {
      url: 'https://example.org/private/ledger',
      level: 3,
      robots: 'enforce',
    });
    assert.ok(isResult(denied));
    assert.equal(denied.outcome, 'robots_denied');
    assert.deepEqual(robotsDenial(denied), { rule: 'Disallow: /private' });
    assert.equal(denied.robots?.crawl_delay, 2.5);

    // A file that could not be had: why, and when to ask again.
    const unknown = await fetchPage(client, { url: 'https://example.org/unknown', level: 3 });
    assert.ok(isResult(unknown));
    const why = robotsDenial(unknown);
    assert.equal(why?.rule, undefined);
    assert.equal(why?.reason, 'robots_unavailable');
    assert.equal(why?.retryAt?.toISOString(), '2026-10-11T13:00:00.000Z');
  });

  it('fetches at level 0 with no persona, and is told what a level does not allow', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });

    // A plain client: no persona, and its attempt names the exit it left from.
    const plain = await fetchPage(client, {
      url: 'https://example.org/plain',
      level: 0,
      include: ['raw', 'headers'],
    });
    assert.ok(isResult(plain));
    assert.equal(plain.level, 0);
    assert.equal(plain.persona, null);
    assert.equal(plain.raw_body, '<h1>plain</h1>');
    assert.deepEqual(
      plain.attempts.map((attempt) => [
        attempt.render,
        attempt.persona,
        attempt.engine,
        attempt.exit,
      ]),
      [['http', null, null, 'pool']],
    );
    assert.deepEqual(JSON.parse(seen.at(-1)?.body ?? ''), {
      url: 'https://example.org/plain',
      level: 0,
      include: ['raw', 'headers'],
    });

    // Level 1 while no browser version has a verified http profile.
    await assert.rejects(
      fetchPage(client, { url: 'https://example.org/listing', level: 1 }),
      (error: unknown) => {
        assert.ok(error instanceof KageProblemError);
        assert.equal(error.status, 422);
        assert.equal(error.problem.code, 'level_unavailable');
        assert.equal(error.problem['field'], 'level');
        return true;
      },
    );

    // The document makes the level required: a request without one does
    // not compile, and the API refuses one that is sent all the same.
    // @ts-expect-error `level` is missing.
    const unleveled: FetchRequest = { url: 'https://example.org/listing' };
    await assert.rejects(fetchPage(client, unleveled), (error: unknown) => {
      assert.ok(error instanceof KageProblemError);
      assert.equal(error.problem.code, 'level_required');
      assert.equal(error.problem['field'], 'level');
      return true;
    });
  });

  it('reads an artifact as bytes', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    const kept = await readArtifact(client, 'art_1');
    assert.equal(kept.contentType, 'text/html; charset=utf-8');
    assert.equal(Buffer.from(kept.bytes).toString('utf8'), '<h1>kept</h1>');
  });

  it('submits a job under a key that names it, and knows the job a key made before', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    // The batch's level, and an item with a level of its own.
    const request = {
      type: 'batch',
      urls: ['https://example.org/0', 'https://example.org/1'],
      items: [{ url: 'https://example.org/2', level: 2, include: ['dom'] }],
      defaults: { level: 3, behavior: { scroll_depth: 0.3, dwell_ms: 3000 } },
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
        items: [{ url: 'https://example.org/2', level: 2, include: ['dom'] }],
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
      { type: 'flow', level: 3, actions: [{ action: 'goto', url: 'https://example.org/' }] },
      'again',
    );
    assert.equal(again.replayed, true);
    assert.equal(again.job.id, JOB);
    // Without a key no header is sent.
    await createJob(client, {
      type: 'batch',
      urls: ['https://example.org/0'],
      defaults: { level: 3 },
    });
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

  it('submits a crawl, reads how far it came and what each page was to it', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    const request = {
      seeds: ['https://example.org/'],
      deny: ['/login'],
      max_depth: 2,
      max_pages: 50,
      use_sitemaps: false,
      defaults: { level: 3, include: ['dom'] },
    };

    const submitted = await createCrawl(client, request, 'crawl-1');

    assert.equal(submitted.replayed, false);
    assert.equal(submitted.job.type, 'crawl');
    // A crawl has no items when it is accepted.
    assert.equal(submitted.job.items.total, 0);
    const sent = seen.at(-1);
    assert.equal(sent?.url, '/v1/jobs');
    assert.equal(sent?.idempotencyKey, 'crawl-1');
    assert.deepEqual(JSON.parse(sent?.body ?? ''), { type: 'crawl', ...request });
    assert.equal(crawlProgress(submitted.job).max_pages, 50);
    assert.equal(crawlProgress(submitted.job).ended ?? null, null);

    const done = await waitForJob(client, CRAWL, { timeoutMs: 2000, everyMs: 5 });
    const crawl = crawlProgress(done);
    assert.equal(crawl.ended, 'completed');
    assert.deepEqual(crawl.skipped, { too_deep: 2, hidden: 1 });
    assert.deepEqual(crawl.reached, { typed: 1, click: 1, back_click: 1, referer: 0 });
    assert.equal(crawl.fetched, done.items.succeeded);

    const pages = await readCrawlPages(client, CRAWL, { results: false });
    assert.deepEqual(
      pages.map((page) => [page.url, page.page.depth, page.page.reached, page.page.backs ?? null]),
      [
        ['https://example.org/', 0, 'typed', null],
        ['https://example.org/a', 1, 'click', 0],
        ['https://example.org/b', 1, 'back_click', 1],
      ],
    );
    assert.equal(pages[2]?.page.duplicate_of, 'https://example.org/a');
    assert.equal(pages[1]?.page.found_on, 'https://example.org/');
    assert.equal(seen.at(-1)?.url, `/v1/jobs/${CRAWL}/items?limit=50&results=false`);
    // Another kind of job has no progress of a crawl.
    assert.throws(() => crawlProgress(job({})), /is a batch, not a crawl/);
  });

  it('throws the problem document of a job that is refused or is not there', async () => {
    const client = createKageClient({ baseUrl, token: TOKEN });
    await assert.rejects(
      createCrawl(client, { seeds: [], defaults: { level: 3 } }),
      (error: unknown) => {
        assert.ok(error instanceof KageProblemError);
        assert.equal(error.status, 422);
        assert.equal(error.problem.code, 'validation_failed');
        assert.equal(error.problem['field'], 'seeds');
        return true;
      },
    );
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
    await assert.rejects(
      fetchPage(client, { url: 'http://127.0.0.1/admin', level: 3 }),
      (error: unknown) => {
        assert.ok(error instanceof KageProblemError);
        assert.equal(error.status, 422);
        assert.equal(error.problem.code, 'ssrf_denied');
        assert.equal(error.problem['field'], 'url');
        assert.match(error.message, /^ssrf_denied: /);
        return true;
      },
    );
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
