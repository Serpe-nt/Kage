// Helpers of the extract QA agent. Run inside the container kage-qa.
import { execFileSync, spawnSync } from 'node:child_process';
export const stack = process.env.STACK ?? 'extract';
export const sh = (...a) => execFileSync('bash', ['/scripts/qa/stack.sh', ...a], { encoding: 'utf8' }).trim();
export const base = sh('url', stack);
export const admin = sh('admin', stack);
export const key = (name, scopes = 'fetch,jobs,crawl', extra = '"requests_per_minute":6000,"max_concurrent":32') =>
  sh('key', stack, `${name}-${Date.now()}`, scopes, extra);
export async function api(method, path, token, body, headers = {}) {
  const r = await fetch(base + path, {
    method,
    headers: { ...(token ? { authorization: `Bearer ${token}` } : {}), ...(body !== undefined ? { 'content-type': 'application/json' } : {}), ...headers },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await r.text();
  let json = null;
  try { json = JSON.parse(text); } catch {}
  return { status: r.status, headers: Object.fromEntries(r.headers), json, text };
}
export const f = (token, body) => api('POST', '/v1/fetch', token, body);
// Programs many routes in one go.
export function program(routes) {
  const code = `let t='';process.stdin.on('data',(c)=>t+=c).on('end',async()=>{for(const r of JSON.parse(t)){await fetch('http://qa.kage.test:8091/__set',{method:'POST',body:JSON.stringify(r)})}console.log('ok')})`;
  const out = spawnSync('bash', ['/scripts/qa/stack.sh', 'dc', stack, 'exec', '-T', 'world', 'node', '-e', code], { input: JSON.stringify(routes), encoding: 'utf8' });
  if (out.status !== 0) throw new Error(out.stderr);
}
export function reqlog() {
  const out = sh('get', stack, 'http://qa.kage.test:8091/__log');
  return JSON.parse(out);
}
export const QA = 'http://qa.kage.test:8091';
export const OTHER = 'http://other.qa.test:8091';
export const html = { 'content-type': 'text/html; charset=utf-8' };
export const words = (n, w = 'word') => Array.from({ length: n }, (_, i) => `${w}${i}`).join(' ');
export const article = (n = 200, title = 'A title') => `<!doctype html><html lang="en"><head><title>${title}</title></head><body><article><h1>${title}</h1><p>${words(n)}</p></article></body></html>`;
export const uid = () => Math.random().toString(36).slice(2, 10);
export const brief = (x, n = 600) => { const s = typeof x === 'string' ? x : JSON.stringify(x); return s && s.length > n ? s.slice(0, n) + `...(${s.length})` : s; };
