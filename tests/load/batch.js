// The load test of the release (spec §21, M12): one batch of 200 pages of
// the fixture site, queued in Kage as one job, read to its end. Run by
// tests/e2e/load.sh in k6's own container on the stack's default network;
// see that script for the two sets of numbers it is run with.
//
// What it shows through the API and the fixture site's own log:
//   - every answer of the API is a 2xx;
//   - the job ends `succeeded`, never `paused`, within the time it is given;
//   - every item is `ok` with status 200 at its first attempt, in a browser,
//     not from the response cache;
//   - the site was asked for every page exactly once, by at most as many
//     addresses as the batch has visitors, never with a header that names
//     a forwarder, and never twice within MIN_GAP_MS by one address.
//
// The numbers it measured are k6 gauges (`load_*`) in the JSON summary.

import { check, fail, sleep } from 'k6';
import { Gauge } from 'k6/metrics';
import http from 'k6/http';

const env = (name, fallback) => {
  const value = __ENV[name] ?? fallback;
  if (value === undefined) fail(`${name} is not set`);
  return value;
};

const KAGE = env('KAGE_URL');
const TOKEN = env('KAGE_TOKEN');
const SITE = env('SITE');
const SITE_HELPER = env('SITE_HELPER');
const PAGES = Number(env('PAGES', '200'));
const VISITORS = Number(env('VISITORS', '3'));
const FIRST_PAGE = Number(env('FIRST_PAGE', '1001'));
const MIN_SECONDS = Number(env('MIN_SECONDS'));
const MAX_SECONDS = Number(env('MAX_SECONDS'));
const MIN_GAP_MS = Number(env('MIN_GAP_MS', '300'));

const ENDED = ['succeeded', 'partial', 'failed', 'cancelled'];
const FORWARDING = ['forwarded', 'via', 'x-forwarded-for', 'x-forwarded-host', 'x-real-ip'];

const totalSeconds = new Gauge('load_total_seconds');
const addresses = new Gauge('load_addresses');
const pagesSeen = new Gauge('load_pages_seen');
const leastGapMs = new Gauge('load_least_gap_ms');
const mostInAMinute = new Gauge('load_site_most_in_60s');
const leastSpanOf21 = new Gauge('load_site_least_span_of_21_ms');

http.setResponseCallback(http.expectedStatuses({ min: 200, max: 299 }));

export const options = {
  scenarios: {
    batch: {
      executor: 'shared-iterations',
      vus: 1,
      iterations: 1,
      maxDuration: `${MAX_SECONDS + 300}s`,
    },
  },
  thresholds: {
    checks: ['rate==1.0'],
    http_req_failed: ['rate==0'],
  },
};

const asKey = { headers: { Authorization: `Bearer ${TOKEN}`, 'Content-Type': 'application/json' } };

function items(id) {
  const all = [];
  let cursor = null;
  for (;;) {
    const query = cursor === null ? '' : `&cursor=${encodeURIComponent(cursor)}`;
    const page = http.get(`${KAGE}/v1/jobs/${id}/items?limit=100${query}`, {
      ...asKey,
      tags: { name: 'items' },
    });
    all.push(...page.json('items'));
    cursor = page.json('next_cursor') ?? null;
    if (cursor === null) return all;
  }
}

export default function () {
  http.get(`${SITE_HELPER}/__kage/reset`, { tags: { name: 'site reset' } });
  const paths = Array.from({ length: PAGES }, (_, i) => `/listing?page=${FIRST_PAGE + i}`);
  const urls = paths.map((path) => `${SITE}${path}`);

  const created = http.post(
    `${KAGE}/v1/jobs`,
    // The pages as the end-to-end tests read them: no scrolling, a dwell of
    // 600 ms over the policy's floor of 500 ms.
    JSON.stringify({
      type: 'batch',
      urls,
      visitors: VISITORS,
      defaults: { render: 'browser', behavior: { scroll_depth: 0, dwell_ms: 600 } },
    }),
    { ...asKey, tags: { name: 'create' } },
  );
  if (!check(created, { 'the batch is accepted': (r) => r.status === 201 })) {
    fail(`the batch was not accepted: ${created.status} ${created.body}`);
  }
  const id = created.json('id');

  // Polled once a second until it ends; every state it was in is kept.
  const states = new Set();
  const deadline = Date.now() + (MAX_SECONDS + 240) * 1000;
  let job = created.json();
  while (!ENDED.includes(job.state) && Date.now() < deadline) {
    sleep(1);
    job = http.get(`${KAGE}/v1/jobs/${id}`, { ...asKey, tags: { name: 'job' } }).json();
    states.add(job.state);
  }
  const seconds = (Date.parse(job.finished_at) - Date.parse(job.created_at)) / 1000;
  totalSeconds.add(seconds);
  check(job, {
    'the job succeeded': (j) => j.state === 'succeeded' && j.items.succeeded === PAGES,
    'the job was never paused': () => !states.has('paused'),
    [`the job took between ${MIN_SECONDS} s and ${MAX_SECONDS} s`]: () =>
      seconds >= MIN_SECONDS && seconds <= MAX_SECONDS,
  });

  const read = items(id);
  check(read, {
    'every item is there': (all) => all.length === PAGES,
    'every item is ok with 200 at its first attempt, in a browser, not cached': (all) =>
      all.every((item, index) => {
        const result = item.result;
        return (
          result !== null &&
          result !== undefined &&
          item.url === urls[index] &&
          result.outcome === 'ok' &&
          result.status === 200 &&
          result.cached === false &&
          result.attempts.length === 1 &&
          result.attempts[0].render === 'browser'
        );
      }),
  });

  // What the site saw of the batch's pages: the documents, not what they
  // load.
  const asked = new Set(paths);
  const seen = http
    .get(`${SITE_HELPER}/__kage/requests`, { tags: { name: 'site log' } })
    .json()
    .filter((request) => asked.has(request.path));
  pagesSeen.add(seen.length);
  const byAddress = new Map();
  for (const request of seen) {
    const list = byAddress.get(request.from) ?? [];
    list.push(request.at);
    byAddress.set(request.from, list);
  }
  addresses.add(byAddress.size);
  let least = Number.MAX_SAFE_INTEGER;
  for (const times of byAddress.values()) {
    times.sort((a, b) => a - b);
    for (let i = 1; i < times.length; i += 1) least = Math.min(least, times[i] - times[i - 1]);
  }
  leastGapMs.add(least === Number.MAX_SAFE_INTEGER ? -1 : least);
  const times = seen.map((request) => request.at).sort((a, b) => a - b);
  let most = 0;
  for (let i = 0, j = 0; i < times.length; i += 1) {
    while (times[i] - times[j] >= 60000) j += 1;
    most = Math.max(most, i - j + 1);
  }
  mostInAMinute.add(most);
  let span = Number.MAX_SAFE_INTEGER;
  for (let i = 20; i < times.length; i += 1) span = Math.min(span, times[i] - times[i - 20]);
  leastSpanOf21.add(span === Number.MAX_SAFE_INTEGER ? -1 : span);
  check(seen, {
    'the site saw every page exactly once': (all) =>
      all.length === PAGES && new Set(all.map((request) => request.path)).size === PAGES,
    [`the site saw at most ${VISITORS} addresses`]: () => byAddress.size >= 1 && byAddress.size <= VISITORS,
    'no page came with a header that names a forwarder': (all) =>
      all.every((request) =>
        request.headers.every(([name]) => !FORWARDING.includes(String(name).toLowerCase())),
      ),
    [`no address asked twice within ${MIN_GAP_MS} ms`]: () => least >= MIN_GAP_MS,
  });
}
