// Helpers of the ssrf topic: run in the container, `node x.mjs STACK`.
import { execFileSync } from 'node:child_process';

const s = '/scripts/qa/stack.sh';
export const stack = process.env.STACK ?? process.argv[2];
const sh = (...a) => execFileSync('bash', [s, ...a], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 1 << 28 }).trim();
export const base = sh('url', stack);
export const admin = sh('admin', stack);
export const token = (name, scopes = 'fetch,jobs,crawl,sessions', extra = '"requests_per_minute":6000,"max_concurrent":50') => sh('key', stack, name, scopes, extra);
export const sql = (q) => sh('sql', stack, q);
export const dcexec = (service, ...args) => sh('dc', stack, 'exec', '-T', service, ...args);
export const logs = (service, tail = '2000') => sh('dc', stack, 'logs', '--no-color', '--tail', tail, service);
const inspect = (name) => execFileSync('docker', ['inspect', '-f', '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}', name], { encoding: 'utf8' }).trim().split(/\s+/).filter((a) => !a.startsWith("11."))[0];
const project = process.env.PROJECT ?? `qa-${stack}`;
export const ip = (service) => inspect(`${project}-${service}-1`);
const responder = `http://${ip('responder')}:8091`;
export const fixture = `http://${ip('fixture')}:8088`;
export const set = async (route) => {
  const r = await fetch(`${responder}/__set`, { method: 'POST', body: JSON.stringify(route) });
  if (!r.ok) throw new Error(`set ${r.status}`);
};
export const rlog = async () => (await fetch(`${responder}/__log`)).json();
export const reset = async () => { await fetch(`${responder}/__reset`, { method: 'POST' }); };
export const rlogFor = async (needle) => (await rlog()).filter((e) => e.url.includes(needle));
export const fixtureLog = async () => (await fetch(`${fixture}/__kage/requests`)).json();
export const api = async (method, path, tok, body, timeoutMs = 300000) => {
  const r = await fetch(`${base}${path}`, {
    method,
    headers: { authorization: `Bearer ${tok}`, ...(body === undefined ? {} : { 'content-type': 'application/json' }) },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  });
  const text = await r.text();
  let json;
  try { json = JSON.parse(text); } catch { json = { raw: text }; }
  return { status: r.status, json, text };
};
export const proxies = async () => {
  const o = {};
  for (const p of ['proxy-a', 'proxy-b', 'proxy-c']) {
    const r = await fetch(`http://${ip(p)}:3128/`);
    o[p] = await r.json();
  }
  return o;
};
export const proxyAddresses = () => ['proxy-a', 'proxy-b', 'proxy-c'].map(ip);
export const kageAddresses = () =>
  execFileSync('docker', ['inspect', '-f', '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}', `${project}-kage-1`], { encoding: 'utf8' }).trim().split(/\s+/);
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const uniq = () => Math.random().toString(36).slice(2, 9);
