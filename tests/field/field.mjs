// The field campaign's runner (docs/runbooks/field-campaign.md, "A session
// runs it"): drives one site's batch of one day through the API of a
// running stack, then records what came of it from the store.
//
//   node field.mjs assign                    deal each country's sites to its personas (once)
//   node field.mjs policies                  write the round's domains.yaml and reload it
//   node field.mjs check <index>             the preflight alone: contacts no site
//   node field.mjs run <index> [--pass 2 --change <text>] [--dom] [--persona <id>]
//                                            the site's batch of today, watched and recorded
//                                            (--persona: the reader of a control)
//   node field.mjs record <index> --job <id> [--pass 2 --change <text>]
//                                            the record of a batch again, from the store
//   node field.mjs warmup                    the personas' warm-up, recorded (follow-up F85)
//   node field.mjs report <file> [--title <text>]
//                                            the round's anonymised report from the records
//
// Everything private comes from the environment and stays in the private
// directory; the token is never printed or written:
//
//   KAGE_FIELD=1            the runner's guard: it never runs without it
//   KAGE_FIELD_SITES        the private sites file (JSON, tests/field/sites.example.json)
//   KAGE_FIELD_DIR          the private directory: records, sections, raw evidence
//   KAGE_API_URL            where the stack's API listens (loopback)
//   KAGE_FIELD_KEY          a key with the fetch, jobs, crawl, sessions and admin scopes
//   KAGE_FIELD_POSTGRES     the stack's Postgres container, read with psql
//   KAGE_FIELD_POLICIES     the private policies directory the stack reads (policies)
//   KAGE_FIELD_SHIPPED      the shipped policies to copy from (default policies/)
//   KAGE_FIELD_POLL_S       how often a batch is looked at (default 60)
//   KAGE_FIELD_KEEPALIVE_S  how often its session is used again (default 240)
//   KAGE_FIELD_FLUSH_S      how long to wait for the last bytes to be counted (default 40)
import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  afterBlock,
  attemptsFromMetrics,
  BLOCKS,
  consentIn,
  cookieNames,
  crawlRequest,
  dealPersonas,
  domainsFile,
  inWindow,
  leaks,
  localTime,
  megabytes,
  pagesAllowed,
  parseSites,
  policyDifferences,
  policyEntry,
  renderSection,
  renderWarmup,
  scrubPrivate,
  sessionIds,
  sessionRequest,
  siteOf,
  stopReason,
  vendorHints,
  windowEnd,
} from './lib.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, '..', '..');

const env = (name) => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set`);
  return value;
};
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const say = (text) => console.log(`    ${text}`);
const writeJson = (path, value) => {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
};

/** The flags after the positional arguments: `--name value` and `--flag`. */
function flags(args) {
  const out = {};
  for (let i = 0; i < args.length; i += 1) {
    if (!args[i].startsWith('--')) continue;
    const name = args[i].slice(2);
    const next = args[i + 1];
    if (next === undefined || next.startsWith('--')) out[name] = true;
    else {
      out[name] = next;
      i += 1;
    }
  }
  return out;
}

/** One call to the API with the round's key. Throws on a status the caller did not expect. */
async function api(method, path, { body, expect = [200], headers = {} } = {}) {
  const response = await fetch(`${env('KAGE_API_URL')}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${env('KAGE_FIELD_KEY')}`,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
      ...headers,
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(60_000),
  });
  const text = await response.text();
  if (!expect.includes(response.status)) {
    throw new Error(`${method} ${path}: ${response.status} ${text.slice(0, 600)}`);
  }
  const type = response.headers.get('content-type') ?? '';
  return { status: response.status, body: type.includes('json') && text ? JSON.parse(text) : text };
}

/** The round's key id (the UUID inside `kage_<id>_<secret>`), never the secret. */
function keyId() {
  const match = /^kage_([0-9a-f]{32})_/.exec(env('KAGE_FIELD_KEY'));
  if (!match) throw new Error('KAGE_FIELD_KEY is not a Kage token');
  const hex = match[1];
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/**
 * One read-only query of the stack's store, its rows as JSON. Values reach
 * psql as variables (`:'name'`), quoted by psql, never spliced into the
 * text.
 */
function sql(query, vars) {
  const args = ['exec', '-i', env('KAGE_FIELD_POSTGRES'), 'psql', '--username', 'kage', '--dbname', 'kage'];
  args.push('--no-psqlrc', '--quiet', '--tuples-only', '--no-align', '--set', 'ON_ERROR_STOP=1');
  for (const [name, value] of Object.entries(vars)) args.push('--set', `${name}=${value}`);
  // jsonb prints on one line, where json_agg's own text breaks between rows.
  const script = `BEGIN TRANSACTION READ ONLY;\nSELECT coalesce(jsonb_agg(q), '[]'::jsonb) FROM (\n${query}\n) q;\nCOMMIT;\n`;
  const out = execFileSync('docker', args, { input: script, encoding: 'utf8', stdio: ['pipe', 'pipe', 'pipe'] });
  return JSON.parse(out.trim().split('\n').find((line) => line.startsWith('[')) ?? '[]');
}

const SITE_MATCH = `(request.domain = :'domain' OR right(request.domain, length(:'domain') + 1) = '.' || :'domain')`;

// The runbook's two queries (docs/runbooks/field-campaign.md, "What to
// record"), for one batch: its job, in place of a domain and a moment.
const RATES = `
SELECT request.level, attempt.engine, attempt.outcome,
       count(*) AS attempts,
       round(100.0 * count(*) / sum(count(*)) OVER (PARTITION BY request.level, attempt.engine), 1) AS percent
FROM attempts attempt
JOIN requests request ON request.id = attempt.request_id
WHERE request.job_id = :'job'
GROUP BY 1, 2, 3
ORDER BY 1, 2, 4 DESC`;
const WHO_BLOCKED = `
SELECT attempt.block ->> 'vendor' AS vendor, attempt.block ->> 'type' AS kind, count(*)
FROM attempts attempt
JOIN requests request ON request.id = attempt.request_id
WHERE request.job_id = :'job' AND attempt.block IS NOT NULL
GROUP BY 1, 2`;
const SITE_COUNTS = `
SELECT count(*) FILTER (WHERE (request.created_at AT TIME ZONE :'tz')::date = :'today'::date) AS today,
       count(*) AS site,
       count(DISTINCT (request.created_at AT TIME ZONE :'tz')::date) AS days
FROM requests request
WHERE request.api_key_id = :'key' AND request.created_at >= :'since' AND ${SITE_MATCH}`;
const ROUND_COUNT = `
SELECT count(*) AS round FROM requests request
WHERE request.api_key_id = :'key' AND request.created_at >= :'since'`;
const SITE_BYTES = `
SELECT coalesce(sum(usage.bytes_up + usage.bytes_down), 0)::bigint AS bytes,
       coalesce(sum(usage.bytes_down), 0)::bigint AS down
FROM bandwidth_usage usage
WHERE usage.hour >= date_trunc('hour', :'from'::timestamptz) AND usage.hour <= :'to'::timestamptz
  AND (usage.domain = :'domain' OR right(usage.domain, length(:'domain') + 1) = '.' || :'domain')`;
const ROUND_BYTES = `
SELECT coalesce(sum(usage.bytes_up + usage.bytes_down), 0)::bigint AS bytes
FROM bandwidth_usage usage
WHERE usage.egress = 'pool' AND usage.hour >= date_trunc('hour', :'since'::timestamptz)`;
const BREAKERS = `
SELECT breaker.domain, breaker.state, breaker.block_count_1h, breaker.paused_until
FROM domain_breakers breaker
WHERE breaker.domain = :'domain' OR right(breaker.domain, length(:'domain') + 1) = '.' || :'domain'`;
// A scout is the registrable domain's; a round's policy key is that domain
// or one of its hosts.
const SCOUT = `
SELECT scout.persona_id, scout.since FROM site_scouts scout
WHERE scout.site = :'domain' OR right(:'domain', length(scout.site) + 1) = '.' || scout.site`;
// Every live persona's warm-up: the visits made and skipped, and when it
// was warmed (follow-up F85: Kage's own verdicts on them mean nothing).
const WARMUP = `
SELECT persona.id, persona.engine, persona.country, persona.warmup_visits, persona.warmed_at,
       count(event.id) FILTER (WHERE event.details ->> 'outcome' <> 'skipped') AS made,
       count(event.id) FILTER (WHERE event.details ->> 'outcome' = 'skipped') AS skipped,
       min(event.at) AS first_visit, max(event.at) AS last_visit
FROM personas persona
LEFT JOIN health_events event
  ON event.subject_id::text = persona.id::text AND event.kind = 'persona_warmup_visit'
WHERE persona.retired_at IS NULL
GROUP BY 1, 2, 3, 4, 5
ORDER BY 1`;
const ROBOTS = `
SELECT cache.origin, cache.status, cache.fetched_at
FROM robots_cache cache
WHERE cache.origin ~ ('^https?://([a-z0-9-]+\\.)*' || replace(:'domain', '.', '\\.') || '(:[0-9]+)?$')`;

function loadRound() {
  return parseSites(readFileSync(env('KAGE_FIELD_SITES'), 'utf8'));
}

/** The pages the round's key asked of a site and of the round, for the budget. */
function counts(round, site, now) {
  const today = localTime(now, site.timezone).date;
  const vars = { key: keyId(), since: round.since, domain: site.domain, tz: site.timezone, today };
  const [mine] = sql(SITE_COUNTS, vars);
  const [all] = sql(ROUND_COUNT, vars);
  return { today: Number(mine.today), site: Number(mine.site), days: Number(mine.days), round: Number(all.round) };
}

function roundBytes(round) {
  return Number(sql(ROUND_BYTES, { since: round.since })[0].bytes);
}

/**
 * The preflight: everything that must hold before a batch begins. Contacts
 * no site; returns what the batch may do, or throws with the reason.
 */
async function preflight(round, site, now = new Date()) {
  const problems = [];
  const ready = await api('GET', '/readyz', { expect: [200, 503] });
  if (ready.status !== 200) problems.push(`the stack is not ready: ${JSON.stringify(ready.body).slice(0, 300)}`);
  const pool = (await api('GET', '/v1/admin/pool')).body;
  const bytes = roundBytes(round);
  const stop = stopReason(pool, bytes, round.stops);
  if (stop) problems.push(`stop: ${stop}`);
  if (!site.persona) problems.push(`site ${site.index} has no persona: run \`assign\` first`);
  let persona = null;
  if (site.persona) {
    persona = (await api('GET', `/v1/admin/personas/${site.persona}`)).body;
    if (persona.state !== 'active' || !persona.ready) {
      problems.push(`the site's persona is ${persona.state}${persona.cooling_reason ? ` (${persona.cooling_reason})` : ''}, ready ${persona.ready}`);
    }
    if (!site.control && (persona.engine !== site.engine || persona.country !== site.country)) {
      problems.push(`the site's persona is ${persona.engine} in ${persona.country}, the sites file says ${site.engine} in ${site.country}`);
    }
  }
  const host = new URL(site.seed).hostname;
  const view = (await api('GET', `/v1/policies/domains/${encodeURIComponent(host)}`)).body;
  for (const [field, want, found] of policyDifferences(site, view)) {
    problems.push(`the policy of ${host} has ${field} ${JSON.stringify(found)}, the round's entry ${JSON.stringify(want)}`);
  }
  if (view.paused_until && Date.parse(view.paused_until) > now.getTime()) problems.push(`the site is paused until ${view.paused_until}`);
  for (const breaker of sql(BREAKERS, { domain: site.domain })) {
    if (breaker.state !== 'closed') problems.push(`the breaker of ${breaker.domain} is ${breaker.state}`);
  }
  if (!inWindow(site, now)) {
    problems.push(`it is ${localTime(now, site.timezone).clock} in ${site.timezone}, outside ${site.window.from} to ${site.window.until}`);
  }
  const counted = counts(round, site, now);
  const allowed = pagesAllowed(site, round.budget, counted);
  if (allowed.reason) problems.push(`budget: ${allowed.reason}`);
  return { problems, pool, persona, counted, pages: allowed.pages, bytes };
}

/**
 * A session pinned to the site's persona: the next id until one lands on
 * it (a new session goes to the ready persona with the fewest open ones).
 * Throws when none of the ids does.
 */
async function pin(round, site, date) {
  const tried = [];
  for (const id of sessionIds(round, site, date)) {
    const opened = await api('POST', '/v1/sessions', { body: sessionRequest(site, id), expect: [200, 201, 409] });
    if (opened.status === 409) throw new Error(`no persona may hold a session for site ${site.index}: ${opened.body.code}`);
    if (opened.body.persona.id === site.persona) return opened.body;
    tried.push(id);
    await api('DELETE', `/v1/sessions/${encodeURIComponent(id)}`, { expect: [200, 204, 404] });
  }
  throw new Error(`no session of ${tried.length} landed on site ${site.index}'s persona`);
}

/** Every item of a job with its result, page by page. */
async function itemsOf(job) {
  const items = [];
  let cursor = null;
  do {
    const page = (
      await api('GET', `/v1/jobs/${job}/items?limit=200&results=true${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ''}`)
    ).body;
    items.push(...page.items);
    cursor = page.next_cursor;
  } while (cursor);
  return items;
}

const ENDED = ['succeeded', 'partial', 'failed', 'cancelled'];

/**
 * A control is read by a persona of the day that reads a site of the round
 * too (a control is no site of the round): the one `--persona` names.
 */
function withReader(site, options) {
  if (!options.persona) return site;
  if (!site.control) throw new Error(`site ${site.index} is read by its own persona; --persona is for a control`);
  return { ...site, persona: options.persona };
}

/** Runs a site's batch of today and records it. */
async function run(round, site, options) {
  const started = new Date();
  const date = localTime(started, site.timezone).date;
  const check = await preflight(round, site, started);
  if (check.problems.length > 0) {
    for (const problem of check.problems) say(`refused: ${problem}`);
    throw new Error(`site ${site.index} cannot be read now`);
  }
  say(`site ${site.index}: ${check.pages} pages allowed; ${check.counted.site} asked of it before, ${check.counted.round} in the round, ${megabytes(check.bytes)} MB used by the round`);
  // A control's session asks for its reader of the day by that persona's
  // country and engine, as a site's does for its own.
  const pinned = site.control ? { ...site, country: check.persona.country, engine: check.persona.engine } : site;
  const session = await pin(round, pinned, date);
  say(`session ${session.id} pinned to the site's persona (${session.persona.engine}, ${session.persona.country})`);
  const include = options.dom ? ['headers', 'extract', 'dom'] : ['headers', 'extract'];
  const request = crawlRequest(round, site, { sessionId: session.id, pages: check.pages, include });
  const created = await api('POST', '/v1/jobs', {
    body: request,
    expect: [200, 201],
    headers: { 'idempotency-key': `${round.round}-${site.index}-${date}-p${options.pass}` },
  });
  const job = created.body.id;
  say(`crawl ${job} accepted (${created.status})`);
  const dir = join(env('KAGE_FIELD_DIR'), date, `site-${site.index}`);
  writeJson(join(dir, 'request.json'), scrubPrivate({ ...request, session: session.id, job, started_at: started.toISOString() }));

  const end = windowEnd(site, started);
  const pollMs = Number(process.env.KAGE_FIELD_POLL_S ?? 60) * 1000;
  const keepaliveMs = Number(process.env.KAGE_FIELD_KEEPALIVE_S ?? 240) * 1000;
  let lastKeepalive = Date.now();
  let ended = null;
  let view = created.body;
  const seen = new Set();
  while (!ended) {
    await sleep(pollMs);
    view = (await api('GET', `/v1/jobs/${job}`)).body;
    if (ENDED.includes(view.state)) {
      ended = `${view.state}${view.crawl?.ended ? ` (${view.crawl.ended})` : ''}`;
      break;
    }
    const now = new Date();
    let cancel = null;
    for (const item of await itemsOf(job)) {
      if (seen.has(item.seq) || !item.result || item.result.state !== 'finished') continue;
      seen.add(item.seq);
      const result = item.result;
      say(`page ${item.seq}: ${result.outcome} ${result.status ?? '-'}${result.block ? ` ${result.block.vendor} ${result.block.type}` : ''}`);
      if (result.persona && result.persona.id !== site.persona) cancel = `a page was read by another persona (${result.persona.id})`;
      if (BLOCKS.includes(result.outcome)) {
        const after = afterBlock(result, now, end);
        if (after.ends_day) cancel = `a block (${result.outcome}) cools the persona until ${after.until.toISOString()}, past the site's day`;
        else say(`a block (${result.outcome}) cools the persona until ${after.until.toISOString()}; the day goes on`);
      }
    }
    if (!cancel && now >= end) cancel = `the site's day ended at ${site.window.until} ${site.timezone}`;
    // Under `enforce` a robots.txt that could not be had holds the batch,
    // and the scout would ask the site again every `robots.error_ttl`: a
    // refusal is the site's answer for the day.
    if (!cancel && view.paused?.reason === 'robots_unavailable') {
      cancel = 'its robots.txt could not be had by the scout (robots_unavailable), so nothing may be read under enforce';
    }
    if (!cancel && view.paused?.until && Date.parse(view.paused.until) >= end.getTime()) {
      cancel = `the batch waits until ${view.paused.until} (${view.paused.reason}), past the site's day`;
    }
    if (!cancel) {
      const stop = stopReason((await api('GET', '/v1/admin/pool')).body, roundBytes(round), round.stops);
      if (stop) cancel = `stop: ${stop}`;
    }
    if (cancel) {
      say(`cancelling: ${cancel}`);
      await api('POST', `/v1/jobs/${job}/cancel`, { expect: [200, 202] });
      for (let n = 0; n < 60 && !ENDED.includes(view.state); n += 1) {
        await sleep(5000);
        view = (await api('GET', `/v1/jobs/${job}`)).body;
      }
      ended = `cancelled by the runner: ${cancel}`;
      break;
    }
    if (Date.now() - lastKeepalive >= keepaliveMs) {
      await api('POST', '/v1/sessions', { body: sessionRequest(pinned, session.id), expect: [200, 201] });
      lastKeepalive = Date.now();
    }
  }
  await api('DELETE', `/v1/sessions/${encodeURIComponent(session.id)}`, { expect: [200, 204, 404] });
  say(`the batch ended: ${ended}`);
  // The forwarder's counts reach the store every `bandwidth.flush_interval`
  // (30 s): the last page's bytes are there after one more.
  await sleep(Number(process.env.KAGE_FIELD_FLUSH_S ?? 40) * 1000);
  return record(round, site, { job, session: session.id, started, ended, pass: options.pass, change: options.change, persona: check.persona });
}

/** What a batch came to, from the API and the store: the private record and the section. */
async function record(round, site, { job, session = null, started = null, ended = null, pass = 1, change = null, persona = null }) {
  const view = (await api('GET', `/v1/jobs/${job}`)).body;
  const reader = persona ?? (await api('GET', `/v1/admin/personas/${site.persona}`)).body;
  const items = await itemsOf(job);
  const from = started ?? new Date(view.started_at ?? view.created_at);
  const to = new Date(view.finished_at ?? Date.now());
  const date = localTime(from, site.timezone).date;
  // A page the site was asked for has an attempt; one the batch's end left
  // unread (a cancel, a stop) has none and is counted apart.
  const unread = items
    .filter((item) => !item.result || item.result.attempts.length === 0)
    .map((item) => ({ seq: item.seq, url: item.url, reason: item.result?.reason ?? item.error ?? item.state }));
  const pages = items
    .filter((item) => item.result && item.result.attempts.length > 0)
    .map((item) => {
      const result = item.result;
      const last = result.attempts.at(-1) ?? {};
      return {
        seq: item.seq,
        url: item.url,
        depth: item.crawl?.depth ?? null,
        reached: item.crawl?.reached ?? null,
        backs: item.crawl?.backs ?? null,
        outcome: result.outcome,
        status: result.status ?? null,
        reason: result.reason ?? null,
        attempts: result.attempts.length,
        persona: result.persona?.id ?? null,
        block: result.block ? { vendor: result.block.vendor, type: result.block.type, confidence: result.block.confidence, evidence: result.block.evidence } : null,
        total_ms: result.timing?.total_ms ?? last.total_ms ?? null,
        patience_ms: result.behavior?.patience_ms ?? null,
        scroll_ended_by: result.behavior?.scroll_ended_by ?? null,
        scroll_reached: result.behavior?.scroll_reached ?? null,
        dwell_ms: result.behavior?.dwell_ms ?? null,
        waited_ms: result.pacing?.waited_ms ?? null,
        word_count: result.extract?.word_count ?? null,
        thin_content: result.extract?.thin_content ?? null,
        lang: result.extract?.lang ?? null,
        title: result.extract?.title ?? null,
        robots: result.robots ?? null,
        vendors: vendorHints(result.headers),
        cookies: cookieNames(result.headers),
        redirects: result.redirects?.length ?? 0,
        final_url: result.final_url ?? null,
      };
    });
  // A batch that kept its documents (`--dom`): the consent dialog each
  // carries, by name only; the document stays in the stack.
  for (const page of pages) {
    const item = items.find((one) => one.seq === page.seq);
    const artifact = item?.result?.artifacts?.dom_html;
    if (artifact) page.consent = consentIn((await api('GET', `/v1/artifacts/${artifact}`)).body);
  }
  const metrics = attemptsFromMetrics((await api('GET', '/metrics')).body, site.domain);
  const bytes = sql(SITE_BYTES, { from: from.toISOString(), to: to.toISOString(), domain: site.domain })[0];
  const robotsRows = sql(ROBOTS, { domain: site.domain });
  const scout = sql(SCOUT, { domain: site.domain })[0] ?? null;
  const fetched = pages.filter((page) => page.robots?.fetched).length;
  const disallowed = pages.filter((page) => page.robots && page.robots.allowed === false).length;
  const blocks = pages.filter((page) => page.block).map((page) => ({ ...page.block, outcome: page.outcome }));
  const readers = [...new Set(pages.map((page) => page.persona).filter(Boolean))];
  const hard = blocks.length > 0;
  const kept = {
    round: round.round,
    index: site.index,
    pass,
    change,
    date,
    job,
    session,
    started_at: from.toISOString(),
    finished_at: to.toISOString(),
    persona: {
      id: reader.id,
      engine: reader.engine,
      country: reader.country,
      claimed_os: reader.claimed_os,
    },
    readers,
    rates: sql(RATES, { job }),
    who_blocked: sql(WHO_BLOCKED, { job }),
    pages,
    unread,
    blocks,
    metrics,
    crawl: view.crawl ?? null,
    skipped: view.crawl?.skipped ?? {},
    robots: `${robotsRows.length} origin(s) asked by the scout, ${fetched} of ${pages.length} results with a verdict, ${disallowed} disallowed`,
    scout: scout ? { persona: scout.persona_id, since: scout.since, is_reader: scout.persona_id === site.persona } : null,
    ended: ended ?? `${view.state}${view.crawl?.ended ? ` (${view.crawl.ended})` : ''}`,
    pacing: `${site.entry.pacing} (locked)`,
    bytes: Number(bytes.bytes),
    decision: decide(site, pass, hard, pages),
  };
  const dir = join(env('KAGE_FIELD_DIR'), date, `site-${site.index}`);
  writeJson(join(dir, `record-pass${pass}.json`), scrubPrivate(kept));
  writeJson(join(env('KAGE_FIELD_DIR'), 'records', `site-${site.index}-pass${pass}.json`), scrubPrivate(kept));
  say(`site ${site.index}: ${pages.length} pages, ${blocks.length} blocked, ${megabytes(kept.bytes)} MB; readers ${readers.length}; ${kept.decision}`);
  if (readers.length > 1 || (readers[0] && readers[0] !== site.persona)) say(`WARNING: the site was read by ${readers.length} personas`);
  return kept;
}

function decide(site, pass, blocked, pages) {
  if (site.control) return blocked ? 'the control was blocked: look at the stack before anything else' : 'control read as expected';
  if (pages.length === 0) return 'nothing read: the batch ended before its first page';
  if (!blocked) {
    const passed = pages.filter((page) => page.outcome === 'challenge_passed').length;
    return passed > 0
      ? `keep; ${passed} challenge(s) passed: pass 2 may try one change`
      : `keep: no block in ${pages.length} pages`;
  }
  return pass === 1 ? 'pass 2: one change, to be approved' : 'refuses a gentle visitor: left alone for the round';
}

/** The records of a site, pass by pass. */
function recordsOf(index) {
  const dir = join(env('KAGE_FIELD_DIR'), 'records');
  return [1, 2]
    .map((pass) => join(dir, `site-${index}-pass${pass}.json`))
    .filter((path) => existsSync(path))
    .map((path) => JSON.parse(readFileSync(path, 'utf8')));
}

/** The private record of the personas' warm-up, for the report's section on it. */
function warmup() {
  const rows = sql(WARMUP, {});
  writeJson(join(env('KAGE_FIELD_DIR'), 'records', 'warmup.json'), { at: new Date().toISOString(), personas: rows });
  const made = rows.reduce((sum, row) => sum + Number(row.made), 0);
  const skipped = rows.reduce((sum, row) => sum + Number(row.skipped), 0);
  say(`${rows.length} personas: ${made} warm-up visits made, ${skipped} skipped, ${rows.filter((row) => row.warmed_at).length} warmed`);
}

/** The anonymised report of the round: one section per site that has a record. */
function report(round, file, title) {
  const sections = [];
  const firstBatch = {};
  for (const site of round.sites) {
    const passes = recordsOf(site.index);
    if (passes.length === 0) continue;
    firstBatch[site.index] = passes[0].started_at;
    sections.push(renderSection(site, passes));
  }
  const warmed = join(env('KAGE_FIELD_DIR'), 'records', 'warmup.json');
  if (existsSync(warmed)) sections.push(renderWarmup(round, JSON.parse(readFileSync(warmed, 'utf8')).personas, firstBatch));
  const text = [`# ${title}`, '', ...sections].join('\n');
  const found = leaks(text, round);
  if (found.length > 0) throw new Error(`the report would carry ${found.join(', ')}: nothing written`);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, text);
  say(`${sections.length} sections written to ${file}`);
}

/** Deals the personas, once, and writes them into the sites file. */
async function assign(round) {
  if (round.sites.some((site) => !site.control && site.persona)) throw new Error('the personas are dealt already');
  const listed = (await api('GET', '/v1/admin/personas')).body;
  const personas = (listed.personas ?? listed).map((persona) => ({
    id: persona.id,
    country: persona.country,
    engine: persona.engine,
    claimed_os: persona.claimed_os,
    state: persona.state,
  }));
  const dealt = dealPersonas(round, personas);
  const path = env('KAGE_FIELD_SITES');
  const file = JSON.parse(readFileSync(path, 'utf8'));
  for (const one of dealt) {
    const site = file.sites.find((entry) => entry.index === one.index);
    Object.assign(site, { persona: one.persona, engine: one.engine, claimed_os: one.claimed_os });
  }
  file.draw = { at: new Date().toISOString(), seed: file.draw_seed, personas: personas.length, dealt };
  writeFileSync(path, `${JSON.stringify(file, null, 2)}\n`);
  parseSites(readFileSync(path, 'utf8'));
  for (const one of dealt) say(`site ${one.index}: a ${one.engine} persona (${one.claimed_os}) in ${one.country}`);
}

/** Writes the round's policies into the private directory and has the stack reload them. */
async function policies(round) {
  const dir = env('KAGE_FIELD_POLICIES');
  const shipped = process.env.KAGE_FIELD_SHIPPED ?? join(repo, 'policies');
  mkdirSync(dir, { recursive: true });
  // The other files are the shipped ones until the round changes one (a
  // signature, a pacing profile): a change of the round's is kept.
  for (const name of readdirSync(shipped).filter((entry) => entry.endsWith('.yaml'))) {
    if (name !== 'domains.yaml' && !existsSync(join(dir, name))) copyFileSync(join(shipped, name), join(dir, name));
  }
  writeFileSync(join(dir, 'domains.yaml'), domainsFile(readFileSync(join(shipped, 'domains.yaml'), 'utf8'), round));
  const reloaded = await api('POST', '/v1/admin/policies/reload', { expect: [200, 422] });
  if (reloaded.status !== 200) throw new Error(`the stack refused the round's policies: ${JSON.stringify(reloaded.body).slice(0, 600)}`);
  say(`the round's ${round.sites.length} entries are in force`);
}

async function main() {
  if (process.env.KAGE_FIELD !== '1') {
    console.error('error: the field runner reads real sites; it runs only with KAGE_FIELD=1, on the owner\'s approval');
    process.exit(2);
  }
  const [command, index, ...rest] = process.argv.slice(2);
  const options = flags(rest);
  const round = loadRound();
  switch (command) {
    case 'assign':
      return assign(round);
    case 'policies':
      return policies(round);
    case 'warmup':
      return warmup();
    case 'entry':
      return process.stdout.write(policyEntry(siteOf(round, index), { key: `site-${index}.example`, comment: `site ${index}` }));
    case 'check': {
      const check = await preflight(round, withReader(siteOf(round, index), options));
      for (const problem of check.problems) say(`refused: ${problem}`);
      say(check.problems.length ? 'the batch may not begin' : `the batch may begin: ${check.pages} pages`);
      return undefined;
    }
    case 'run':
      return run(round, withReader(siteOf(round, index), options), {
        pass: Number(options.pass ?? 1),
        change: options.change ?? null,
        dom: Boolean(options.dom),
      });
    case 'record':
      return record(round, withReader(siteOf(round, index), options), {
        job: options.job,
        pass: Number(options.pass ?? 1),
        change: options.change ?? null,
      });
    case 'report':
      return report(round, index, options.title ?? 'Field report');
    default:
      console.error('usage: field.mjs assign | policies | entry <index> | check <index> | run <index> | record <index> --job <id> | report <file>');
      process.exit(2);
  }
  return undefined;
}

main().catch((error) => {
  console.error(`error: ${error.message}`);
  process.exit(1);
});
