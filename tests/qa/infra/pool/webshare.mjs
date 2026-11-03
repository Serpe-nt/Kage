#!/usr/bin/env node
// A fake Webshare API (spec §7.2, crates/kage-proxy/src/provider/webshare.rs)
// for the pool's QA, round two. It lists proxies of the world (world.mjs
// proxy containers, by the address their name resolves to), replaces one on
// POST v3/proxy/replace/ with the next reserve, and keeps the replaced log.
//
// Kage refuses a plain-http `webshare.base_url` unless it is this machine
// (keyed_url in crates/kage-core/src/config/proxies.rs), and its client has no
// setting for a private CA; so this server runs in kage's own network
// namespace (`network_mode: service:kage`) and kage is given
// `KAGE__WEBSHARE__BASE_URL=http://127.0.0.1:9480/api/`.
//
//   node webshare.mjs --port 9480 --active 'proxy-a:3128:provider-a:e2e-only-a,...' \
//        --reserve 'proxy-d:3128:provider-d:e2e-only-d,...' [--complete-after-ms 3000]
//
// GET  /__state                  what it lists, the reserve, the replacements, the log of calls
// POST /__drop {"name":"proxy-a","reason":"auto_invalidated","replace":true}
//                                the provider replaces (or, replace false, drops) a proxy by itself
// POST /__fail {"code":"active_replacement"}   the next replace answers 400 with that code
// Every call must carry `Authorization: Token <non-empty>`; the token itself is
// never recorded, only whether one came.
import dns from 'node:dns';
import http from 'node:http';

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 && args[index + 1] !== undefined ? args[index + 1] : fallback;
};
const port = Number(option('port', '9480'));
const completeAfter = Number(option('complete-after-ms', '3000'));
const parse = (text) =>
  text
    .split(',')
    .filter(Boolean)
    .map((spec, index) => {
      const [name, p, username, password, country] = spec.split(':');
      return { name, port: Number(p), username, password, country: country || 'DE' };
    });
let nextId = 1000;
const withId = (entry) => ({ ...entry, id: String(nextId++) });
const active = parse(option('active', '')).map(withId);
const reserve = parse(option('reserve', '')).map(withId);
const replacements = new Map();
const replaced = [];
const calls = [];
let failNext = null;

// A proxy's address as first resolved, kept: a stopped container's name no
// longer resolves, and a provider still lists (and replaces) the address.
const known = new Map();
const addressOf = async (name) => {
  try {
    const { address } = await dns.promises.lookup(name, { family: 4 });
    known.set(name, address);
    return address;
  } catch {
    return known.get(name) ?? null;
  }
};
const listed = async (entry) => ({
  id: entry.id,
  username: entry.username,
  password: entry.password,
  proxy_address: await addressOf(entry.name),
  port: entry.port,
  valid: true,
  last_verification: new Date().toISOString(),
  country_code: entry.country,
  city_name: 'Berlin',
  asn_name: 'QA Net',
  asn_number: 64500,
  high_country_confidence: true,
  created_at: '2026-10-01T00:00:00Z',
});

const swap = async (entry, reason) => {
  const index = active.indexOf(entry);
  if (index < 0) return null;
  active.splice(index, 1);
  const next = reserve.shift() ?? null;
  if (next) active.push(next);
  replaced.unshift({
    id: nextId++,
    reason,
    proxy: await addressOf(entry.name),
    proxy_port: entry.port,
    proxy_country_code: entry.country,
    replaced_with: next ? await addressOf(next.name) : '',
    replaced_with_port: next ? next.port : null,
    replaced_with_country_code: next ? next.country : null,
    created_at: new Date().toISOString(),
  });
  return next;
};

const json = (res, status, value) => {
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify(value));
};

http
  .createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', async () => {
      const body = Buffer.concat(chunks).toString('utf8');
      const url = new URL(req.url ?? '/', 'http://webshare.invalid');
      const path = url.pathname;
      if (path === '/__state') {
        json(res, 200, {
          active: await Promise.all(active.map(async (e) => ({ id: e.id, name: e.name, address: await addressOf(e.name) }))),
          reserve: reserve.map((e) => ({ id: e.id, name: e.name })),
          replacements: Object.fromEntries(replacements),
          replaced,
          calls: calls.slice(-300),
        });
        return;
      }
      if (path === '/__drop' && req.method === 'POST') {
        const change = JSON.parse(body || '{}');
        const entry = active.find((e) => e.name === change.name);
        if (!entry) return json(res, 404, { error: 'not listed' });
        if (change.replace === false) {
          active.splice(active.indexOf(entry), 1);
          return json(res, 200, { dropped: entry.name });
        }
        const next = await swap(entry, change.reason || 'auto_invalidated');
        return json(res, 200, { dropped: entry.name, added: next?.name ?? null });
      }
      if (path === '/__fail' && req.method === 'POST') {
        failNext = JSON.parse(body || '{}').code ?? null;
        return json(res, 200, { fail_next: failNext });
      }
      const auth = String(req.headers.authorization ?? '');
      calls.push({ at: new Date().toISOString(), method: req.method, path, query: url.search, token: /^Token \S+$/.test(auth), user_agent: req.headers['user-agent'] ?? null, body: req.method === 'POST' ? body.slice(0, 2000) : undefined });
      if (!/^Token \S+$/.test(auth)) return json(res, 401, { detail: 'Invalid token.' });
      const api = path.replace(/^\/api\//, '');
      if (api === 'v2/proxy/list/') {
        const page = Number(url.searchParams.get('page') ?? '1');
        const size = Number(url.searchParams.get('page_size') ?? '100');
        const slice = active.slice((page - 1) * size, page * size);
        const more = page * size < active.length;
        return json(res, 200, { count: active.length, next: more ? `x?page=${page + 1}` : null, previous: null, results: await Promise.all(slice.map(listed)) });
      }
      if (api === 'v2/proxy/list/replaced/') {
        return json(res, 200, { count: replaced.length, next: null, previous: null, results: replaced });
      }
      if (api === 'v3/proxy/replace/' && req.method === 'POST') {
        if (failNext) {
          const code = failNext;
          failNext = null;
          return json(res, 400, { type: 'validation_error', errors: [{ code, detail: code, attr: null }] });
        }
        const request = JSON.parse(body || '{}');
        const ip = request?.to_replace?.ip_addresses?.[0];
        let entry = null;
        for (const e of active) if ((await addressOf(e.name)) === ip || e.name === ip) entry = e;
        const id = nextId++;
        const record = { id, state: entry ? 'validating' : 'failed', ip, name: entry?.name ?? null, created_at: new Date().toISOString(), error: entry ? null : 'no such proxy', error_code: entry ? null : 'invalid' };
        replacements.set(String(id), record);
        if (entry) {
          setTimeout(async () => {
            const next = await swap(entry, 'proxy_replaced');
            record.state = 'completed';
            record.added = next?.name ?? null;
          }, completeAfter);
        }
        return json(res, 201, { id, state: record.state, proxies_removed: 0, proxies_added: 0, dry_run: false, created_at: record.created_at });
      }
      const status = api.match(/^v3\/proxy\/replace\/(\d+)\/$/);
      if (status) {
        const record = replacements.get(status[1]);
        if (!record) return json(res, 404, { detail: 'Not found.' });
        return json(res, 200, { id: record.id, state: record.state, proxies_removed: record.state === 'completed' ? 1 : 0, proxies_added: record.state === 'completed' && record.added ? 1 : 0, error: record.error, error_code: record.error_code });
      }
      if (api === 'v2/subscription/') {
        const start = new Date(Date.now() - 5 * 86400000).toISOString();
        const end = new Date(Date.now() + 25 * 86400000).toISOString();
        return json(res, 200, { id: 1, plan: 77, start_date: start, end_date: end, renewals_enabled: true, paused: false, throttled: false });
      }
      if (api === 'v2/subscription/plan/77/') {
        const used = [...replacements.values()].filter((r) => r.state !== 'failed').length;
        return json(res, 200, { id: 77, status: 'active', proxy_type: 'static', proxy_subtype: 'residential', proxy_count: active.length, proxy_countries: { DE: active.length }, bandwidth_limit: 250, proxy_replacements_total: 10, proxy_replacements_used: used, proxy_replacements_available: 10 - used });
      }
      if (api === 'v3/proxy/config') {
        return json(res, 200, { auto_replace_invalid_proxies: true, auto_replace_low_country_confidence_proxies: false, auto_replace_out_of_rotation_proxies: false, auto_replace_failed_site_check_proxies: false });
      }
      if (api === 'v3/proxy/list/stats') {
        return json(res, 200, { available_countries: { DE: reserve.length } });
      }
      if (api === 'v2/stats/') return json(res, 200, []);
      return json(res, 404, { detail: 'Not found.' });
    });
  })
  .listen(port, '127.0.0.1');

for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(0));
