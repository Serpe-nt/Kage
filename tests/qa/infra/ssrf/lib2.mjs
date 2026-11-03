// Helpers of the ssrf topic, round two: the DNS world of overlay-dns.yaml.
// Run in the container, `STACK=name SSRF_NET=11.66.1 node x.mjs`.
export * from '/scripts/qa/ssrf/lib.mjs';
import { ip } from '/scripts/qa/ssrf/lib.mjs';

export const net = process.env.SSRF_NET ?? '11.66.1';
export const PUB = `${net}.10`;
const dnsBase = `http://${net}.53:8053`;
const pubBase = `http://${PUB}:8091`;
const post = async (url, body) => {
  const r = await fetch(url, { method: 'POST', body: body === undefined ? undefined : JSON.stringify(body) });
  if (!r.ok) throw new Error(`${url} ${r.status} ${await r.text()}`);
};
/** Programs one name: {name, a, aaaa, times, by, ttl, rcode, cname}. */
export const dns = (entry) => post(`${dnsBase}/__set`, entry);
export const dnsLog = async (name) => {
  const all = await (await fetch(`${dnsBase}/__log`)).json();
  return name ? all.filter((e) => e.name.toLowerCase().replace(/\.$/, '') === name) : all;
};
export const pubSet = (route) => post(`${pubBase}/__set`, route);
export const pubLog = async () => (await fetch(`${pubBase}/__log`)).json();
export const udpLog = async () => (await fetch(`http://${ip('udpcatch')}:8054/`)).json();
export const udpReset = () => post(`http://${ip('udpcatch')}:8054/__reset`);
/** The private address of a service on the stack's default network. */
export const priv = (service) => ip(service);
export const short = (r) => ({
  status: r.status,
  outcome: r.json.outcome,
  code: r.json.code,
  reason: r.json.reason,
  detail: r.json.detail,
  final_url: r.json.final_url,
  http_status: r.json.status,
  attempts: (r.json.attempts ?? []).length,
});
