// QA helpers of the crawl topic: API calls, the responder, a page generator.
import { execSync } from 'node:child_process';
import crypto from 'node:crypto';

export const stack = process.env.S || 'crawl';
const sh = (c) => execSync(c, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
export const st = (args) => sh(`bash /scripts/qa/stack.sh ${args}`);
export const base = st(`url ${stack}`);
export const admin = st(`admin ${stack}`);
const responderIp = sh(
  `docker inspect qa-${stack}-responder-1 --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}'`,
);
const R = `http://${responderIp}:8091`;

export async function api(method, path, body, token = admin, headers = {}) {
  const once = () => fetch(base + path, {
    method,
    headers: {
      ...(token ? { authorization: `Bearer ${token}` } : {}),
      ...(body !== undefined ? { 'content-type': 'application/json' } : {}),
      ...headers,
    },
    body: body !== undefined ? (typeof body === 'string' ? body : JSON.stringify(body)) : undefined,
  });
  let res;
  for (let tries = 0; ; tries++) {
    try {
      res = await once();
      break;
    } catch (error) {
      // The server was restarted under a kept connection, or it is on its way up.
      if (tries >= 20) throw error;
      await new Promise((r) => setTimeout(r, 500));
    }
  }
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {}
  return { status: res.status, json, text, headers: res.headers };
}

let cachedKey = null;
export async function key(extra = {}) {
  if (cachedKey && !Object.keys(extra).length) return cachedKey;
  const r = await api('POST', '/v1/admin/keys', {
    name: 'qa-crawl-' + crypto.randomBytes(5).toString('hex'),
    scopes: ['fetch', 'jobs', 'crawl'],
    requests_per_minute: 6000,
    max_concurrent: 64,
    ...extra,
  });
  if (r.status !== 201) throw new Error('key: ' + r.text);
  if (!Object.keys(extra).length) cachedKey = r.json.token;
  return r.json.token;
}

export const uid = () => crypto.randomBytes(4).toString('hex');

export async function set(route) {
  const r = await fetch(R + '/__set', { method: 'POST', body: JSON.stringify(route) });
  if (!r.ok) throw new Error('set failed');
}
export async function log() {
  return (await fetch(R + '/__log')).json();
}
export async function reset() {
  await fetch(R + '/__reset', { method: 'POST' });
}

// Unique text of a page: enough distinct words that SimHash and hash differ.
export function words(seed, n = 120) {
  const out = [];
  let h = crypto.createHash('sha256').update(String(seed)).digest();
  while (out.length < n) {
    for (let i = 0; i + 3 < h.length && out.length < n; i += 3) {
      out.push(h.subarray(i, i + 3).toString('hex').replace(/[0-9]/g, (d) => 'abcdefghij'[d]));
    }
    h = crypto.createHash('sha256').update(h).digest();
  }
  return out.join(' ');
}

export function html(seed, inner = '', head = '') {
  return `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Page ${seed}</title>${head}</head><body><main><h1>Page ${seed}</h1><p>${words(seed)}</p>${inner}</main></body></html>`;
}
const a = (href, text = 'link') => `<p><a href="${href}">${text} ${href.replace(/[<>"&]/g, '')}</a></p>`;
export const links = (...hrefs) => hrefs.map((h) => a(h)).join('');

// site({ '/x/': ['/x/a', '/x/b'], '/x/a': '<raw inner html>' }, host?)
export async function site(pages, host) {
  for (const [path, v] of Object.entries(pages)) {
    const inner = Array.isArray(v) ? links(...v) : v;
    await set({ path, ...(host ? { host } : {}), status: 200, headers: { 'content-type': 'text/html; charset=utf-8' }, body: html(path, inner) });
  }
}

export const Q = 'http://qa.kage.test:8091';
export const O = 'http://other.qa.test:8091';

export async function start(spec, token) {
  const r = await api('POST', '/v1/jobs', { type: 'crawl', defaults: { level: 0 }, ...spec }, token ?? (await key()));
  return r;
}
export async function job(id, token) {
  return (await api('GET', `/v1/jobs/${id}`, undefined, token ?? (await key()))).json;
}
export async function wait(id, ms = 240000, token) {
  const t0 = Date.now();
  for (;;) {
    const j = await job(id, token);
    if (j.finished_at || ['succeeded', 'partial', 'failed', 'cancelled'].includes(j.state)) return j;
    if (Date.now() - t0 > ms) return { ...j, TIMED_OUT: true };
    await new Promise((r) => setTimeout(r, 500));
  }
}
export async function items(id, token) {
  const out = [];
  let cursor = null;
  for (;;) {
    const r = await api('GET', `/v1/jobs/${id}/items?limit=100${cursor ? `&cursor=${encodeURIComponent(cursor)}` : ''}`, undefined, token ?? (await key()));
    if (r.status !== 200) throw new Error('items: ' + r.text);
    out.push(...r.json.items);
    cursor = r.json.next_cursor;
    if (!cursor) break;
  }
  return out;
}
export const brief = (i) => ({
  url: i.url,
  st: i.state,
  d: i.crawl?.depth,
  by: i.crawl?.found_by,
  on: i.crawl?.found_on,
  r: i.crawl?.reached,
  final: i.crawl?.final_url,
  dup: i.crawl?.duplicate_of,
  near: i.crawl?.near_duplicate_of,
  l: i.crawl?.links,
  err: i.error,
  oc: i.outcome,
  http: i.result?.status,
  why: i.result?.reason,
  rf: i.result?.final_url !== i.url ? i.result?.final_url : undefined,
  rd: i.result?.redirects?.length || undefined,
  nb: i.not_before ?? undefined,
});
export async function run(spec, opt = {}) {
  const r = await start(spec, opt.token);
  if (r.status !== 202 && r.status !== 201) return { refused: r.status, code: r.json?.code, detail: r.json?.detail, text: r.text };
  const j = await wait(r.json.id, opt.ms, opt.token);
  const it = await items(r.json.id, opt.token);
  return { job: j, items: it };
}
export function show(name, x) {
  if (x.refused) {
    console.log(`== ${name}: REFUSED ${x.refused} ${x.code} ${x.detail}`);
    return;
  }
  const { job: j, items: it } = x;
  console.log(`== ${name}: state=${j.state} timedout=${!!j.TIMED_OUT} paused=${JSON.stringify(j.paused)} error=${JSON.stringify(j.error)} items=${JSON.stringify(j.items)}`);
  console.log('   crawl=' + JSON.stringify(j.crawl));
  for (const i of it) console.log('   ' + JSON.stringify(brief(i)));
}
export const sql = (q) => {
  const f = `/tmp/sql-${uid()}.txt`;
  return execSync(`bash /scripts/qa/stack.sh sql ${stack} "$(cat <<'EOF'\n${q}\nEOF\n)"`, { encoding: 'utf8', shell: '/bin/bash', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
};
