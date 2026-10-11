// The client and the judge of the live check of jobs (tests/live/jobs.sh).
//
//   node check-jobs.mjs run <workdir>             the fetches, the batch, the flows, their webhooks
//   node check-jobs.mjs overflow-begin <workdir>  a fetch that outlasts the wall time
//   node check-jobs.mjs overflow-end <workdir>    its result, after the server was restarted
//   node check-jobs.mjs snapshot <workdir> <name> /metrics and the pool, as an admin sees them
//   node check-jobs.mjs judge <workdir> [evidence directory]
//
// Everything it asks the API for is one of two third-party detection pages
// (spec §21: a live check never touches a target site), unless
// KAGE_LIVE_JOBS_REHEARSAL=1 names two pages of a world of the rehearsal's
// own. Keys and the webhook's secret come from the environment and are
// never written anywhere:
//
//   KAGE_API_URL            where the API listens
//   KAGE_API_KEY            a key with the fetch, jobs and sessions scopes
//   KAGE_ADMIN_KEY          a key with the admin scope (snapshot)
//   KAGE_LIVE_HOOK_URL      the receiver the events are posted to
//   KAGE_LIVE_HOOK_SECRET   what they are signed with
//   KAGE_LIVE_JOBS_ONLY     `flows`: only the two flows and the event of one
//                           (four page loads), to look at a flow again
//
// `judge` reads what the other modes saved, prints what was measured and a
// verdict per step, and with an evidence directory writes there what the
// key saw of its jobs: states, counters, times per page and per step,
// persona handles and engines. No address, credential, seed or fingerprint
// is written; the judge refuses to write a file that holds something that
// looks like one.
import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const [mode, workdir, extra] = process.argv.slice(2);
if (!mode || !workdir) {
  console.error("usage: check-jobs.mjs run|overflow-begin|overflow-end|snapshot|judge <workdir> [...]");
  process.exit(2);
}

const ENGINES = ["chromium", "firefox"];
const rehearsal = process.env.KAGE_LIVE_JOBS_REHEARSAL === "1";
// The two pages of the check. Nothing else is ever asked for.
const PAGE_A = rehearsal ? process.env.KAGE_LIVE_JOBS_PAGE_A : "https://bot.sannysoft.com/";
const PAGE_B = rehearsal ? process.env.KAGE_LIVE_JOBS_PAGE_B : "https://abrahamjuliot.github.io/creepjs/";
// What each page needs before it has painted its verdicts (the self-test's own figures).
const SETTLE_A_MS = rehearsal ? 500 : 4000;
const SETTLE_B_MS = rehearsal ? 500 : 12_000;
// The policy's floor under a persona's stay on a page.
const MIN_DWELL_MS = 2000;
// Only the flows: the fetches, the batch and the restart are left out.
const ONLY_FLOWS = process.env.KAGE_LIVE_JOBS_ONLY === "flows";
// A rehearsal may give the scroll a time it cannot make, to see it end
// inside it. The check itself gives a step the API's own default.
const SCROLL_TIMEOUT_MS = rehearsal && process.env.KAGE_LIVE_JOBS_SCROLL_TIMEOUT_MS
  ? { timeout_ms: Number(process.env.KAGE_LIVE_JOBS_SCROLL_TIMEOUT_MS) }
  : {};

const env = (name) => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is not set`);
  return value;
};
const save = (name, value) =>
  writeFileSync(join(workdir, name), typeof value === "string" ? value : JSON.stringify(value, null, 2));
const load = (name) => JSON.parse(readFileSync(join(workdir, name), "utf8"));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const seconds = (from, to) => Math.round((Date.parse(to) - Date.parse(from)) / 100) / 10;

/** One call to the API. */
async function api(method, path, { body, key = env("KAGE_API_KEY"), headers = {}, raw = false } = {}) {
  const response = await fetch(`${env("KAGE_API_URL")}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${key}`,
      ...(body === undefined ? {} : { "content-type": "application/json" }),
      ...headers,
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(330_000),
  });
  if (raw) return response;
  const text = await response.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    // Left as null: the caller reports the status.
  }
  return { status: response.status, json, headers: response.headers };
}

function expectStatus(what, answer, status) {
  if (answer.status !== status) {
    const said = answer.json ? `${answer.json.code ?? ""} ${answer.json.detail ?? ""}` : "";
    throw new Error(`${what}: status ${answer.status}, expected ${status}. ${said}`.trim());
  }
}

const ended = (job) => ["succeeded", "partial", "failed", "cancelled"].includes(job.state);

/** Reads a job until it has ended. A job that waits says for what. */
async function waitForJob(what, id, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let last = "";
  for (;;) {
    const answer = await api("GET", `/v1/jobs/${id}`);
    expectStatus(what, answer, 200);
    const job = answer.json;
    const now = `${job.state} ${job.items.succeeded + job.items.failed + job.items.cancelled}/${job.items.total}`;
    if (now !== last) {
      const waits = job.paused ? ` (${job.paused.reason} until ${job.paused.until})` : "";
      console.log(`    ${what}: ${now}${waits}`);
      last = now;
    }
    if (ended(job)) return job;
    if (job.paused?.until && Date.parse(job.paused.until) > deadline) {
      throw new Error(
        `${what} waits for ${job.paused.reason} until ${job.paused.until}, past this check's patience. ` +
          "A job never wakes a persona: at night run with KAGE_LIVE_JOBS_ACTIVE_HOURS=ignore.",
      );
    }
    if (Date.now() > deadline) throw new Error(`${what} is ${now} after ${Math.round(timeoutMs / 1000)} s`);
    await sleep(3000);
  }
}

async function itemsOf(id) {
  const items = [];
  let cursor;
  for (;;) {
    const answer = await api("GET", `/v1/jobs/${id}/items?limit=50${cursor ? `&cursor=${cursor}` : ""}`);
    expectStatus("the items of a job", answer, 200);
    items.push(...answer.json.items);
    if (!answer.json.next_cursor) return items;
    cursor = answer.json.next_cursor;
  }
}

async function createJob(what, body) {
  const answer = await api("POST", "/v1/jobs", {
    body,
    headers: { "idempotency-key": `live-${what}-${Date.now()}` },
  });
  expectStatus(`submitting ${what}`, answer, 201);
  return answer.json;
}

const webhook = () => ({ url: env("KAGE_LIVE_HOOK_URL"), secret: env("KAGE_LIVE_HOOK_SECRET") });

function flowOf(engine, withWebhook) {
  return {
    type: "flow",
    actions: [
      { action: "goto", url: PAGE_A },
      { action: "wait_for", ms: SETTLE_A_MS },
      { action: "scroll", depth: 0.6, ...SCROLL_TIMEOUT_MS },
      { action: "screenshot" },
      { action: "goto", url: PAGE_B },
      { action: "wait_for", ms: SETTLE_B_MS },
      // The keyboard's way back, by the persona's own hand.
      { action: "back" },
      { action: "wait_for", url: PAGE_A },
      { action: "screenshot" },
    ],
    include: ["dom", "headers"],
    persona: { engines: [engine] },
    timeout_ms: 270_000,
    ...(withWebhook ? { webhook: webhook() } : {}),
  };
}

async function run() {
  // (a) one page in the browser of a persona of each engine, while the caller waits.
  for (const engine of ONLY_FLOWS ? [] : ENGINES) {
    console.log(`==> a synchronous fetch of ${PAGE_A} in ${engine}`);
    const began = Date.now();
    const answer = await api("POST", "/v1/fetch", {
      body: {
        url: PAGE_A,
        render: "browser",
        include: ["dom", "headers", "screenshot"],
        persona: { engines: [engine] },
        timeout_ms: 150_000,
      },
    });
    expectStatus(`the fetch in ${engine}`, answer, 200);
    save(`fetch-${engine}.json`, answer.json);
    const result = answer.json;
    console.log(
      `    ${result.outcome}, HTTP ${result.status}, persona ${result.persona?.id}, ` +
        `dwelt ${result.behavior?.dwell_ms} ms, ${Math.round((Date.now() - began) / 1000)} s in all`,
    );
  }

  // (b) a batch of the two pages, each twice, with the default one visitor per site.
  let batch = null;
  if (!ONLY_FLOWS) {
    console.log("==> a batch of the two pages, each twice, one visitor per site");
    batch = await createJob("batch", {
      type: "batch",
      urls: [PAGE_A, PAGE_B, PAGE_A, PAGE_B],
      defaults: { include: ["dom", "headers"], timeout_ms: 150_000 },
      webhook: webhook(),
    });
    save("batch-accepted.json", batch);
    await waitForJob("the batch", batch.id, 45 * 60_000);
    save("batch-items.json", await itemsOf(batch.id));
  }

  // (c) a flow in the browser of a persona of each engine.
  const flows = {};
  for (const engine of ENGINES) {
    console.log(`==> a flow of nine steps in ${engine}`);
    const flow = await createJob(`flow-${engine}`, flowOf(engine, engine === "chromium"));
    flows[engine] = flow.id;
    await waitForJob(`the ${engine} flow`, flow.id, 20 * 60_000);
    save(`flow-${engine}-items.json`, await itemsOf(flow.id));
  }

  // (d) the events of the batch and of one flow, at the receiver.
  console.log("==> the events at the receiver");
  const wanted = [...(batch ? [batch.id] : []), flows.chromium];
  const deadline = Date.now() + 5 * 60_000;
  let deliveries = [];
  for (;;) {
    deliveries = await (await fetch(`${new URL(env("KAGE_LIVE_HOOK_URL")).origin}/__deliveries`)).json();
    const delivered = (id) =>
      deliveries.some((d) => d.answered === 204 && d.body?.job?.id === id && d.event !== "job.progress");
    if (wanted.every(delivered)) break;
    if (Date.now() > deadline) break;
    await sleep(3000);
  }
  save("deliveries.json", deliveries);
  console.log(`    ${deliveries.map((d) => `${d.event} ${d.answered}`).join(", ") || "none"}`);
  // The jobs as their key sees them at the end, with their delivery logs.
  if (batch) save("batch.json", (await api("GET", `/v1/jobs/${batch.id}`)).json);
  for (const engine of ENGINES) {
    save(`flow-${engine}.json`, (await api("GET", `/v1/jobs/${flows[engine]}`)).json);
  }

  // The pictures that were kept: their sizes and types, not the pictures.
  const sizes = {};
  const measure = async (name, artifact) => {
    if (!artifact) return;
    const response = await api("GET", `/v1/artifacts/${artifact}`, { raw: true });
    const bytes = (await response.arrayBuffer()).byteLength;
    sizes[name] = { status: response.status, bytes, content_type: response.headers.get("content-type") };
  };
  for (const engine of ENGINES) {
    if (!ONLY_FLOWS) await measure(`fetch-${engine}`, load(`fetch-${engine}.json`).artifacts?.screenshot);
    for (const item of load(`flow-${engine}-items.json`)) {
      await measure(`flow-${engine}-step-${item.seq}`, item.result?.artifacts?.screenshot);
    }
  }
  save("sizes.json", sizes);
}

async function overflowBegin() {
  console.log(`==> a synchronous fetch of ${PAGE_A} against a wall time it outlasts`);
  const answer = await api("POST", "/v1/fetch", {
    body: { url: PAGE_A, render: "browser", include: ["dom", "headers"], timeout_ms: 150_000 },
  });
  expectStatus("the fetch that overflows", answer, 202);
  save("overflow-accepted.json", answer.json);
  const job = await api("GET", `/v1/jobs/${answer.json.job_id}`);
  expectStatus("the job of the fetch", job, 200);
  save("overflow-job-before.json", job.json);
  console.log(`    202: request ${answer.json.id}, carried on by job ${answer.json.job_id} (${job.json.state})`);
}

async function overflowEnd() {
  const accepted = load("overflow-accepted.json");
  const deadline = Date.now() + 10 * 60_000;
  let answer;
  let last = "";
  for (;;) {
    answer = await api("GET", `/v1/results/${accepted.id}`);
    const job = await api("GET", `/v1/jobs/${accepted.job_id}`);
    const now = `result ${answer.status}, job ${job.json?.state}${job.json?.paused ? ` (${job.json.paused.reason})` : ""}`;
    if (now !== last) {
      console.log(`    after the restart: ${now}`);
      last = now;
    }
    if (answer.status === 200) break;
    if (Date.now() > deadline) throw new Error(`the carried fetch has no result after ten minutes: ${now}`);
    await sleep(3000);
  }
  save("overflow-result.json", answer.json);
  save("overflow-job.json", (await api("GET", `/v1/jobs/${accepted.job_id}`)).json);
  save("overflow-items.json", await itemsOf(accepted.job_id));
}

async function snapshot(name) {
  const key = env("KAGE_ADMIN_KEY");
  const metrics = await api("GET", "/metrics", { key, raw: true });
  save(`${name}.metrics.txt`, await metrics.text());
  const pool = await api("GET", "/v1/admin/pool", { key });
  expectStatus("the pool", pool, 200);
  save(`${name}.pool.json`, pool.json);
}

// --- the judge ---------------------------------------------------------------

/** The series of a scrape that say nothing of a persona or an address. */
const FAMILIES = [
  "kage_requests_total",
  "kage_attempts_total",
  "kage_bandwidth_bytes_total",
  "kage_bandwidth_budget_ratio",
  "kage_jobs",
  "kage_job_items",
  "kage_jobs_accepted_total",
  "kage_jobs_finished_total",
  "kage_job_items_total",
  "kage_webhook_deliveries_total",
  "kage_webhook_deliveries_pending",
  "kage_queue_depth",
  "kage_scheduler_refusals_total",
];

function seriesOf(name) {
  const path = join(workdir, `${name}.metrics.txt`);
  if (!existsSync(path)) return null;
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((line) => FAMILIES.some((family) => line.startsWith(`${family}{`) || line.startsWith(`${family} `)))
    .sort();
}

function bytesOf(lines, direction) {
  const line = (lines ?? []).find(
    (l) => l.startsWith("kage_bandwidth_bytes_total{") && l.includes(`direction="${direction}"`),
  );
  return line ? Number(line.trim().split(" ").pop()) : null;
}

function poolOf(name) {
  const path = join(workdir, `${name}.pool.json`);
  if (!existsSync(path)) return null;
  const pool = JSON.parse(readFileSync(path, "utf8"));
  // Counts and the budget: the forwarder's own count and the provider's figure.
  return { personas: pool.personas, bandwidth: pool.bandwidth };
}

const scrubbedJob = (job) => ({
  id: job.id,
  type: job.type,
  state: job.state,
  origin: job.origin,
  items: job.items,
  visitors: job.visitors ?? null,
  paused: job.paused ?? null,
  cancel_requested: job.cancel_requested,
  created_at: job.created_at,
  started_at: job.started_at ?? null,
  finished_at: job.finished_at ?? null,
  webhook: job.webhook
    ? {
        receiver: "the loopback receiver of this check",
        deliveries: (job.webhook.deliveries ?? []).map((d) => ({
          id: d.id,
          type: d.type,
          state: d.state,
          attempts: d.attempts,
          last_status: d.last_status ?? null,
          created_at: d.created_at,
        })),
      }
    : null,
});

const scrubbedItem = (item, sizes, prefix) => ({
  seq: item.seq,
  state: item.state,
  url: item.url ?? null,
  step: item.step ?? null,
  outcome: item.outcome ?? null,
  error: item.error ?? null,
  started_at: item.started_at ?? null,
  finished_at: item.finished_at ?? null,
  seconds: item.started_at && item.finished_at ? seconds(item.started_at, item.finished_at) : null,
  request_id: item.request_id ?? null,
  result: item.result
    ? {
        outcome: item.result.outcome,
        status: item.result.status ?? null,
        final_url: item.result.final_url ?? null,
        persona: item.result.persona?.id ?? null,
        engine: item.result.persona?.engine ?? null,
        attempts: (item.result.attempts ?? []).length,
        total_ms: item.result.timing?.total_ms ?? null,
        screenshot_bytes: sizes[`${prefix}-step-${item.seq}`]?.bytes ?? null,
      }
    : null,
});

/** Refuses anything that looks like an address, a token or a long secret. */
function assertClean(name, text) {
  const patterns = [
    [/\b(?:\d{1,3}\.){3}\d{1,3}\b/, "an IPv4 address"],
    [/\b(?:[0-9a-f]{1,4}:){4,}[0-9a-f]{1,4}\b/i, "an IPv6 address"],
    [/kage_[0-9a-f]{32}_/, "an API token"],
    [/\b[0-9a-f]{64}\b/i, "a seed or a key"],
  ];
  for (const [pattern, what] of patterns) {
    const found = text.match(pattern);
    if (found) throw new Error(`${name} would hold ${what}; nothing was written`);
  }
}

function judge(evidenceDir) {
  const problems = [];
  const verdicts = [];
  const measured = {};
  const step = (name, check, needs = []) => {
    if (needs.some((file) => !existsSync(join(workdir, file)))) {
      verdicts.push([name, "NOT RUN"]);
      return;
    }
    const before = problems.length;
    try {
      check((condition, message) => {
        if (!condition) problems.push(`${name}: ${message}`);
      });
    } catch (error) {
      problems.push(`${name}: ${error.message}`);
    }
    verdicts.push([name, problems.length === before ? "PASSED" : "FAILED"]);
  };
  const sizes = existsSync(join(workdir, "sizes.json")) ? load("sizes.json") : {};
  const fine = (outcome) => ["ok", "challenge_passed"].includes(outcome);

  // (a) the synchronous fetches.
  measured.fetches = {};
  step("a. a synchronous fetch in each engine", (check) => {
    for (const engine of ENGINES) {
      const result = load(`fetch-${engine}.json`);
      const shot = sizes[`fetch-${engine}`];
      check(result.state === "finished" && fine(result.outcome), `${engine}: ${result.outcome} (${result.reason ?? ""})`);
      check(result.status === 200, `${engine}: HTTP ${result.status}`);
      check(result.persona?.engine === engine, `${engine}: read by a ${result.persona?.engine} persona`);
      check(/^p_[0-9a-f]{16}$/.test(result.persona?.id ?? ""), `${engine}: the persona is not a handle`);
      check((result.attempts ?? []).at(-1)?.render === "browser", `${engine}: not a browser attempt`);
      check(result.behavior?.applied === "policy", `${engine}: behaviour applied by ${result.behavior?.applied}`);
      check((result.behavior?.dwell_ms ?? 0) >= MIN_DWELL_MS, `${engine}: dwelt ${result.behavior?.dwell_ms} ms`);
      check((result.behavior?.moves ?? 0) > 0, `${engine}: the pointer did not move`);
      check(shot?.status === 200 && shot.bytes > 1000, `${engine}: no screenshot (${JSON.stringify(shot)})`);
      measured.fetches[engine] = {
        persona: result.persona?.id,
        outcome: result.outcome,
        status: result.status,
        attempts: (result.attempts ?? []).length,
        total_ms: result.timing?.total_ms,
        behavior: {
          applied: result.behavior?.applied,
          dwell_ms: result.behavior?.dwell_ms,
          scroll_depth: result.behavior?.scroll_depth,
          pointer_moves: result.behavior?.moves,
          wheel_notches: result.behavior?.notches,
        },
        pacing: result.pacing?.applied ?? null,
        screenshot: shot ? { bytes: shot.bytes, content_type: shot.content_type } : null,
      };
    }
  }, ["fetch-chromium.json"]);

  // (b) the batch.
  step("b. a batch of four items with one visitor per site", (check) => {
    const job = load("batch.json");
    const items = load("batch-items.json");
    check(job.state === "succeeded", `the batch is ${job.state}: ${JSON.stringify(job.items)}`);
    check(job.visitors === 1, `visitors ${job.visitors}`);
    check(items.length === 4, `${items.length} items`);
    const bySite = new Map();
    for (const item of items) {
      check(item.state === "succeeded" && fine(item.result?.outcome), `item ${item.seq}: ${item.state} ${item.error ?? ""}`);
      check(item.url === [PAGE_A, PAGE_B][item.seq % 2], `item ${item.seq}: ${item.url}`);
      check(item.result?.final_url?.startsWith(item.url), `item ${item.seq} ended on ${item.result?.final_url}`);
      const site = new URL(item.url).host;
      bySite.set(site, [...(bySite.get(site) ?? []), item]);
    }
    measured.batch = { seconds: seconds(job.created_at, job.finished_at), sites: {} };
    for (const [site, pages] of bySite) {
      const visitors = new Set(pages.map((item) => item.result?.persona?.id));
      check(visitors.size === 1, `${site} was read by ${visitors.size} personas`);
      const [first, second] = pages;
      // An item is handed to the scheduler when its turn comes and waits
      // there for the visitor's pace: its page began when its attempts did.
      const spent = (item) => (item.result?.attempts ?? []).reduce((sum, attempt) => sum + (attempt.total_ms ?? 0), 0);
      const began = (item) => new Date(Date.parse(item.finished_at) - spent(item)).toISOString();
      const startToStart = second ? seconds(began(first), began(second)) : null;
      // The profile's gap is from one page's start to the next one's, and
      // never shorter than 1.5 s.
      check(startToStart === null || startToStart >= 1.5, `${site}: the second page began ${startToStart} s after the first began`);
      measured.batch.sites[site] = {
        visitor: [...visitors][0],
        engine: first.result?.persona?.engine,
        pages: pages.map((item) => ({
          seq: item.seq,
          waited_for_its_turn_s: seconds(item.started_at, began(item)),
          began_after_accept_s: seconds(job.created_at, began(item)),
          page_took_ms: spent(item),
          attempts: (item.result?.attempts ?? []).length,
        })),
        second_page_began_after_the_first_began_s: startToStart,
        pause_between_the_two_pages_s: second ? seconds(first.finished_at, began(second)) : null,
      };
    }
  }, ["batch.json"]);

  // (c) the flows.
  measured.flows = {};
  step("c. a flow in each engine", (check) => {
    for (const engine of ENGINES) {
      const job = load(`flow-${engine}.json`);
      const items = load(`flow-${engine}-items.json`);
      check(job.state === "succeeded", `${engine}: the flow is ${job.state}: ${items.map((i) => i.error).filter(Boolean).join("; ")}`);
      const actions = items.map((item) => item.step?.action);
      check(
        actions.join(" ") === "goto wait_for scroll screenshot goto wait_for back wait_for screenshot final",
        `${engine}: steps ${actions.join(" ")}`,
      );
      for (const item of items.slice(0, 9)) {
        check(item.step?.ok === true, `${engine}: step ${item.seq} (${item.step?.action}): ${item.error ?? "not ok"}`);
      }
      // It stayed on every page it arrived on, never below the policy's floor,
      // and no step that only reads was delayed.
      for (const arrival of [0, 4, 6]) {
        check((items[arrival]?.step?.paced_ms ?? 0) >= MIN_DWELL_MS, `${engine}: step ${arrival} paced ${items[arrival]?.step?.paced_ms} ms`);
      }
      for (const reading of [1, 3, 5, 7, 8]) {
        check(items[reading]?.step?.paced_ms === 0, `${engine}: the reading step ${reading} was delayed`);
      }
      for (const picture of [3, 8]) {
        const shot = sizes[`flow-${engine}-step-${picture}`];
        check(shot?.status === 200 && shot.bytes > 1000, `${engine}: step ${picture} kept no picture`);
        check(items[picture]?.result?.final_url?.startsWith(PAGE_A), `${engine}: step ${picture} pictured ${items[picture]?.result?.final_url}`);
      }
      const last = items[9];
      check(fine(last?.outcome), `${engine}: the flow ended ${last?.outcome}`);
      check(last?.result?.final_url?.startsWith(PAGE_A), `${engine}: it ended on ${last?.result?.final_url}`);
      check(last?.result?.persona?.engine === engine, `${engine}: run by a ${last?.result?.persona?.engine} persona`);
      measured.flows[engine] = {
        persona: last?.result?.persona?.id,
        seconds: seconds(job.created_at, job.finished_at),
        attempts: (last?.result?.attempts ?? []).length,
        steps: items.slice(0, 9).map((item) => ({
          action: item.step?.action,
          ms: item.step?.duration_ms,
          paced_ms: item.step?.paced_ms,
        })),
        screenshots_bytes: [3, 8].map((seq) => sizes[`flow-${engine}-step-${seq}`]?.bytes ?? null),
      };
    }
  });

  // (d) the webhooks.
  step("d. the events of the batch and of one flow, signed, at the receiver", (check) => {
    const deliveries = load("deliveries.json");
    const batch = existsSync(join(workdir, "batch.json")) ? load("batch.json") : null;
    const flow = load("flow-chromium.json");
    check(deliveries.length >= (batch ? 3 : 2), `${deliveries.length} deliveries`);
    for (const d of deliveries) {
      check(d.verified, `a delivery of ${d.event} did not verify`);
      check(d.from_loopback && d.forwarding_headers.length === 0, "a delivery did not come straight from this host");
      check(d.method === "POST" && d.content_type === "application/json", `${d.method} ${d.content_type}`);
      check(/^kage\//.test(d.user_agent ?? ""), `User-Agent ${d.user_agent}`);
      check(d.body?.id === d.delivery && d.body?.type === d.event, "the body does not name its event");
      const text = JSON.stringify(d.body);
      check(!text.includes("persona") && !text.includes(new URL(PAGE_A).host), "the event says what was fetched, or by whom");
    }
    const first = deliveries[0];
    const again = deliveries.filter((d) => d.delivery === first?.delivery);
    check(first?.answered === 503, `the first delivery was answered ${first?.answered}`);
    check(again.length === 2 && again[1].answered === 204, `the refused event came ${again.length} times`);
    const wait = again.length === 2 ? seconds(again[0].at, again[1].at) : null;
    // The first retry: 30 s, moved by up to a fifth.
    check(wait !== null && wait >= 23 && wait <= 40, `the retry came after ${wait} s`);
    for (const [name, job] of [...(batch ? [["batch", batch]] : []), ["chromium flow", flow]]) {
      const completed = deliveries.filter((d) => d.event === "job.completed" && d.body?.job?.id === job.id && d.answered === 204);
      check(completed.length === 1, `${completed.length} delivered job.completed for the ${name}`);
      check(completed[0]?.body?.job?.state === "succeeded", `the ${name}'s event says ${completed[0]?.body?.job?.state}`);
      const log = job.webhook?.deliveries ?? [];
      check(log.some((d) => d.type === "job.completed" && d.state === "delivered"), `the ${name}'s delivery log: ${JSON.stringify(log)}`);
    }
    check((load("flow-firefox.json").webhook ?? null) === null, "the flow without a webhook has one");
    measured.webhooks = {
      deliveries: deliveries.map((d) => ({
        at: d.at,
        event: d.event,
        delivery: d.delivery,
        job: d.body?.job?.id,
        verified: d.verified,
        age_s: d.age_s,
        answered: d.answered,
        from_this_host: d.from_loopback,
      })),
      first_retry_after_s: wait,
    };
  });

  // (e) the overflow and the restart.
  step("e. a fetch that outlasts the wall time, across a restart of the server", (check) => {
    const accepted = load("overflow-accepted.json");
    const result = load("overflow-result.json");
    const job = load("overflow-job.json");
    const items = load("overflow-items.json");
    check(accepted.state === "running" && typeof accepted.job_id === "string", "the 202 names no job");
    check(result.id === accepted.id, "the result is another request's");
    check(result.state === "finished" && fine(result.outcome), `the result is ${result.outcome} (${result.reason ?? ""})`);
    check(result.final_url?.startsWith(PAGE_A), `it ended on ${result.final_url}`);
    check(job.origin === "sync_overflow" && job.type === "batch" && job.items.total === 1, "not a job of one item made by the overflow");
    check(job.state === "succeeded", `the job is ${job.state}`);
    check(items.length === 1 && items[0].request_id === accepted.id, "the item is not the request of the fetch");
    measured.overflow = {
      request: accepted.id,
      job: accepted.job_id,
      state_before_restart: load("overflow-job-before.json").state,
      outcome: result.outcome,
      status: result.status,
      attempts: (result.attempts ?? []).length,
      persona: result.persona?.id,
      engine: result.persona?.engine,
      job_seconds: seconds(job.created_at, job.finished_at),
    };
  }, ["overflow-accepted.json"]);

  // What was counted, for the record: no assertion (the provider's counter lags).
  const snapshots = ["before", "after", "end"].map((name) => [name, seriesOf(name), poolOf(name)]);
  measured.bandwidth = Object.fromEntries(
    snapshots
      .filter(([, series]) => series)
      .map(([name, series, pool]) => [
        name,
        {
          forwarder_bytes_up: bytesOf(series, "up"),
          forwarder_bytes_down: bytesOf(series, "down"),
          budget: pool?.bandwidth ?? null,
        },
      ]),
  );

  console.log("==> what was measured");
  console.log(JSON.stringify(measured, null, 2));
  console.log("==> the verdict, step by step");
  for (const [name, verdict] of verdicts) console.log(`    ${verdict}  ${name}`);
  for (const problem of problems) console.error(`error: ${problem}`);
  if (verdicts.every(([, verdict]) => verdict !== "PASSED")) problems.push("no step ran");

  if (evidenceDir) {
    mkdirSync(evidenceDir, { recursive: true });
    const files = {
      "measured.json": JSON.stringify(measured, null, 2),
      "verdict.txt": [...verdicts.map(([n, v]) => `${v}  ${n}`), ...problems.map((p) => `error: ${p}`)].join("\n") + "\n",
    };
    const optional = (name, make) => {
      try {
        files[name] = JSON.stringify(make(), null, 2);
      } catch {
        // A step that did not run left nothing to keep.
      }
    };
    optional("batch.json", () => ({
      job: scrubbedJob(load("batch.json")),
      items: load("batch-items.json").map((item) => scrubbedItem(item, sizes, "batch")),
    }));
    for (const engine of ENGINES) {
      optional(`flow-${engine}.json`, () => ({
        job: scrubbedJob(load(`flow-${engine}.json`)),
        items: load(`flow-${engine}-items.json`).map((item) => scrubbedItem(item, sizes, `flow-${engine}`)),
      }));
    }
    optional("overflow.json", () => ({
      accepted: { id: load("overflow-accepted.json").id, job_id: load("overflow-accepted.json").job_id, state: "running" },
      job_before_restart: scrubbedJob(load("overflow-job-before.json")),
      job: scrubbedJob(load("overflow-job.json")),
      items: load("overflow-items.json").map((item) => scrubbedItem(item, sizes, "overflow")),
    }));
    for (const [name, series, pool] of snapshots) {
      if (series) files[`metrics-${name}.txt`] = `${series.join("\n")}\n`;
      if (pool) files[`pool-${name}.json`] = JSON.stringify(pool, null, 2);
    }
    for (const [name, text] of Object.entries(files)) {
      assertClean(name, text);
      writeFileSync(join(evidenceDir, name), text.endsWith("\n") ? text : `${text}\n`);
    }
  }
  if (problems.length > 0) process.exit(1);
  console.log("    every step passed");
}

try {
  if (mode === "run") await run();
  else if (mode === "overflow-begin") await overflowBegin();
  else if (mode === "overflow-end") await overflowEnd();
  else if (mode === "snapshot") await snapshot(extra ?? "snapshot");
  else if (mode === "judge") judge(extra);
  else throw new Error(`unknown mode ${mode}`);
} catch (error) {
  console.error(`error: ${error instanceof Error ? error.message : String(error)}`);
  process.exit(1);
}
