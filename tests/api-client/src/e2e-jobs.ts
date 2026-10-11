/**
 * What the compose end-to-end test of jobs runs (tests/e2e/jobs.sh), with a
 * client that was generated from the API's document and is not Rust:
 *
 * - a batch of 50 pages of the fixture site, read by three visitors, to
 *   completion, with a result per item, submitted twice under one
 *   `Idempotency-Key`;
 * - a flow of several steps in the browser of a persona of each engine,
 *   with what each capturing step brought back;
 * - a batch that is cancelled while it is read.
 *
 *   KAGE_API_URL          where the API listens
 *   KAGE_API_KEY          a key with the `jobs` and `fetch` scopes
 *   KAGE_E2E_SITE         the fixture site, for example http://fixture.kage.test:8088
 *   KAGE_E2E_HOOK         where the batch's events are posted
 *   KAGE_E2E_HOOK_SECRET  what they are signed with
 *
 * It prints one JSON document and exits 0 when everything held.
 */
import assert from 'node:assert/strict';

import {
  cancelJob,
  createJob,
  createKageClient,
  KageProblemError,
  readArtifact,
  readJob,
  readJobItems,
  readResult,
  waitForJob,
  type JobItem,
  type JobRequest,
  type KageClient,
} from './client.js';

function env(name: string): string {
  const value = process.env[name];
  if (value === undefined || value === '') {
    throw new Error(`${name} is not set`);
  }
  return value;
}

const BATCH_PAGES = 50;
const ENGINES = ['chromium', 'firefox'] as const;

/** The result of an item, which every item that was fetched has. */
function resultOf(item: JobItem) {
  const result = item.result;
  assert.ok(result !== undefined && result !== null, `item ${item.seq} has no result`);
  return result;
}

async function batch(client: KageClient, site: string) {
  const urls = Array.from(
    { length: BATCH_PAGES },
    (_, index) => `${site}/listing?page=${101 + index}`,
  );
  const request: JobRequest = {
    type: 'batch',
    urls,
    // All three personas of the stack read the site side by side: that is
    // what the caller pays for the batch to end within the test.
    visitors: 3,
    defaults: {
      include: ['dom', 'headers'],
      behavior: { scroll_depth: 0, dwell_ms: 600 },
      timeout_ms: 30_000,
    },
    webhook: {
      url: env('KAGE_E2E_HOOK'),
      secret: env('KAGE_E2E_HOOK_SECRET'),
      progress_every: 10,
    },
  };
  const key = `e2e-batch-${Date.now()}`;
  const started = Date.now();

  const first = await createJob(client, request, key);
  assert.equal(first.replayed, false);
  assert.equal(first.job.type, 'batch');
  assert.equal(first.job.items.total, BATCH_PAGES);
  assert.equal(first.job.visitors, 3);
  assert.equal(first.job.webhook?.host, new URL(env('KAGE_E2E_HOOK')).hostname);

  // The same request under the same key is the same job; another request
  // under it is a mistake.
  const again = await createJob(client, request, key);
  assert.equal(again.replayed, true);
  assert.equal(again.job.id, first.job.id);
  const reused = await createJob(client, { type: 'batch', urls: [urls[0] ?? ''] }, key).then(
    () => null,
    (error: unknown) => error,
  );
  assert.ok(reused instanceof KageProblemError, 'a reused key was accepted');
  assert.equal(reused.status, 409);
  assert.equal(reused.problem.code, 'idempotency_key_reused');
  assert.equal(reused.problem['job_id'], first.job.id);

  const done = await waitForJob(client, first.job.id, { timeoutMs: 600_000 });
  assert.equal(done.state, 'succeeded', JSON.stringify(done));
  assert.deepEqual(done.items, {
    total: BATCH_PAGES,
    succeeded: BATCH_PAGES,
    failed: 0,
    cancelled: 0,
    pending: 0,
  });

  // Every item, in the order given, with its own result.
  const items = await readJobItems(client, first.job.id, { pageSize: 20 });
  assert.equal(items.length, BATCH_PAGES);
  const engines = new Map<string, number>();
  const personas = new Set<string>();
  for (const [index, item] of items.entries()) {
    assert.equal(item.seq, index);
    assert.equal(item.state, 'succeeded', `item ${index}: ${item.error ?? ''}`);
    assert.equal(item.url, urls[index]);
    assert.equal(item.outcome, 'ok');
    const result = resultOf(item);
    assert.equal(result.outcome, 'ok');
    assert.equal(result.status, 200);
    assert.equal(result.final_url, urls[index]);
    assert.equal(result.id, item.request_id);
    assert.equal(result.attempts.at(-1)?.render, 'browser');
    const persona = result.persona;
    assert.ok(persona !== undefined && persona !== null, `item ${index} names no persona`);
    assert.match(persona.id, /^p_[0-9a-f]{16}$/);
    engines.set(persona.engine, (engines.get(persona.engine) ?? 0) + 1);
    personas.add(persona.id);
    // The document is an artifact of the item's request.
    assert.ok(typeof result.artifacts['dom_html'] === 'string', `item ${index}: no document`);
  }
  // Real personas of both engines read it.
  for (const engine of ENGINES) {
    assert.ok((engines.get(engine) ?? 0) > 0, `no item was read in ${engine}`);
  }
  assert.ok(personas.size <= 3, `more visitors than asked for: ${personas.size}`);

  // One item's document, as it was kept and as its result returns it.
  const sample = items[BATCH_PAGES - 1];
  assert.ok(sample !== undefined);
  const stored = await readResult(client, sample.request_id ?? '');
  assert.ok('outcome' in stored);
  assert.ok(stored.dom_html?.includes(`Listing, page ${100 + BATCH_PAGES}`), 'the wrong document');
  const kept = await readArtifact(client, resultOf(sample).artifacts['dom_html'] ?? '');
  assert.equal(Buffer.from(kept.bytes).toString('utf8'), stored.dom_html);

  // Without results the page of items is the states alone.
  const bare = await readJobItems(client, first.job.id, { results: false, pageSize: 200 });
  assert.equal(bare.length, BATCH_PAGES);
  assert.ok(bare.every((item) => item.result === undefined || item.result === null));

  return {
    id: first.job.id,
    state: done.state,
    items: done.items,
    engines: Object.fromEntries(engines),
    visitors: personas.size,
    seconds: Math.round((Date.now() - started) / 1000),
  };
}

async function flow(client: KageClient, site: string, engine: (typeof ENGINES)[number]) {
  const listing = '/listing?page=7';
  const search = `${site}/search?q=lamps&r=${encodeURIComponent(listing)}&r=${encodeURIComponent('/landing')}`;
  const started = Date.now();
  const { job } = await createJob(client, {
    type: 'flow',
    actions: [
      { action: 'goto', url: search },
      // The persona's own hand clicks the first result.
      { action: 'click', selector: 'a.result' },
      { action: 'wait_for', selector: '#referer' },
      { action: 'extract', include: ['dom', 'headers'] },
      { action: 'back' },
      { action: 'wait_for', url: '**/search?**' },
      { action: 'extract', include: ['dom'] },
      { action: 'goto', url: `${site}/landing` },
      { action: 'assert', selector: '#landing' },
      { action: 'screenshot' },
    ],
    include: ['dom', 'headers'],
    persona: { engines: [engine] },
    behavior: { dwell_ms: 800 },
    timeout_ms: 150_000,
  });
  assert.equal(job.type, 'flow');
  // One item per step, and one for the page the flow ends on.
  assert.equal(job.items.total, 11);

  const done = await waitForJob(client, job.id, { timeoutMs: 400_000 });
  const items = await readJobItems(client, job.id);
  assert.equal(done.state, 'succeeded', JSON.stringify(items.map((item) => item.error)));
  assert.deepEqual(
    items.map((item) => item.step?.action),
    [
      'goto',
      'click',
      'wait_for',
      'extract',
      'back',
      'wait_for',
      'extract',
      'goto',
      'assert',
      'screenshot',
      'final',
    ],
  );
  for (const item of items.slice(0, 10)) {
    assert.equal(item.state, 'succeeded', `step ${item.seq}: ${item.error ?? ''}`);
    assert.equal(item.step?.ok, true);
    assert.ok(typeof item.step.duration_ms === 'number');
    assert.ok(typeof item.step.paced_ms === 'number', `step ${item.seq} was not paced`);
  }
  // It stayed on every page it arrived on, never below the policy's floor.
  for (const arrival of [0, 1, 4, 7]) {
    assert.ok(
      (items[arrival]?.step?.paced_ms ?? 0) >= 500,
      `step ${arrival}: ${JSON.stringify(items[arrival]?.step)}`,
    );
  }
  // A step that only reads is not delayed.
  assert.equal(items[2]?.step?.paced_ms, 0);

  // What each capturing step brought back is a result of its own.
  const clicked = items[3];
  assert.ok(clicked !== undefined);
  const page = resultOf(clicked);
  assert.equal(page.final_url, `${site}${listing}`);
  assert.equal(page.status, 200);
  assert.equal(page.persona?.engine, engine);
  const document = await readResult(client, clicked.request_id ?? '');
  assert.ok('outcome' in document);
  // The page was reached by a click on the results page, as its own
  // server saw it.
  const html = document.dom_html ?? '';
  assert.ok(html.includes('Listing, page 7'), 'the click did not arrive');
  assert.ok(html.includes(`<p id="referer">${site}/search?`), html);

  const wentBack = items[6];
  assert.ok(wentBack !== undefined);
  assert.ok(resultOf(wentBack).final_url?.startsWith(`${site}/search?q=lamps`));
  const results = await readResult(client, wentBack.request_id ?? '');
  assert.ok('outcome' in results);
  assert.ok(results.dom_html?.includes('id="results"'), 'the page before did not come back');

  const picture = items[9];
  assert.ok(picture !== undefined);
  const pictureId = resultOf(picture).artifacts['screenshot'];
  assert.ok(pictureId !== undefined, 'the screenshot step kept no picture');
  const shot = await readArtifact(client, pictureId);
  assert.ok(shot.bytes.length > 1000, `a picture of ${shot.bytes.length} bytes`);

  // The last item is the page the flow ended on: the flow's own request.
  const last = items[10];
  assert.ok(last !== undefined);
  assert.equal(last.outcome, 'ok');
  const ended = resultOf(last);
  assert.equal(ended.final_url, `${site}/landing`);
  assert.equal(ended.attempts.length, 1);
  assert.equal(ended.persona?.engine, engine);

  return {
    engine,
    id: job.id,
    persona: ended.persona.id,
    steps: items.slice(0, 10).map((item) => ({
      action: item.step?.action,
      ms: item.step?.duration_ms,
      paced_ms: item.step?.paced_ms,
    })),
    captures: items.filter((item) => item.step?.action !== 'final' && item.request_id != null)
      .length,
    screenshot_bytes: shot.bytes.length,
    seconds: Math.round((Date.now() - started) / 1000),
  };
}

async function cancelled(client: KageClient, site: string) {
  const pages = 30;
  const { job } = await createJob(client, {
    type: 'batch',
    urls: Array.from({ length: pages }, (_, index) => `${site}/slow?ms=1500&n=${index + 1}`),
    defaults: { behavior: { scroll_depth: 0, dwell_ms: 600 }, timeout_ms: 30_000 },
  });
  await waitForJob(client, job.id, {
    timeoutMs: 180_000,
    everyMs: 250,
    done: (seen) => seen.items.succeeded >= 2,
  });
  const atCancel = await cancelJob(client, job.id);
  assert.equal(atCancel.cancel_requested, true);
  const done = await waitForJob(client, job.id, { timeoutMs: 120_000, everyMs: 250 });
  assert.equal(done.state, 'cancelled', JSON.stringify(done));
  // One visitor: the page that was being read is finished, and not one
  // more is begun.
  assert.ok(
    done.items.succeeded <= atCancel.items.succeeded + 1,
    `${done.items.succeeded} ended, ${atCancel.items.succeeded} had when the cancel came`,
  );
  assert.equal(done.items.cancelled, pages - done.items.succeeded);
  assert.equal(done.items.pending, 0);
  const items = await readJobItems(client, job.id, { results: false, pageSize: 200 });
  for (const item of items) {
    assert.equal(item.state, item.seq < done.items.succeeded ? 'succeeded' : 'cancelled');
  }
  // A job that has ended is answered as it ended.
  assert.equal((await cancelJob(client, job.id)).state, 'cancelled');
  assert.equal((await readJob(client, job.id)).state, 'cancelled');

  return {
    id: job.id,
    succeeded_at_cancel: atCancel.items.succeeded,
    succeeded: done.items.succeeded,
    cancelled: done.items.cancelled,
  };
}

async function main(): Promise<unknown> {
  const client = createKageClient({ baseUrl: env('KAGE_API_URL'), token: env('KAGE_API_KEY') });
  const site = env('KAGE_E2E_SITE');

  const read = await batch(client, site);
  const flows = [];
  for (const engine of ENGINES) {
    flows.push(await flow(client, site, engine));
  }
  const stopped = await cancelled(client, site);

  // A job that cannot be accepted whole is refused whole.
  const refusal = await createJob(client, {
    type: 'flow',
    actions: [
      { action: 'goto', url: `${site}/landing` },
      { action: 'goto', url: 'http://169.254.169.254/latest/meta-data/' },
    ],
  }).then(
    () => null,
    (error: unknown) => error,
  );
  assert.ok(refusal instanceof KageProblemError, 'a flow to a metadata address was accepted');
  assert.equal(refusal.problem.code, 'ssrf_denied');
  assert.equal(refusal.problem['step'], 1);

  return { batch: read, flows, cancelled: stopped, refused: refusal.problem.code };
}

try {
  process.stdout.write(`${JSON.stringify(await main())}\n`);
} catch (error) {
  process.stderr.write(
    `${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`,
  );
  process.exitCode = 1;
}
