// Lists the hosts an idle Chrome asked for by itself, from its NetLog
// (`--log-net-log`, written by the measurement image of the live check).
//
//   node netlog-hosts.mjs <netlog file> <sidecar log>
//
// The window is the one background-traffic.mjs measures: from the moment
// the sidecar opened the context to the moment the self-test asked for its
// first page. Only host names are printed, with how many requests went to
// each; never a path, a query or an address, and the proxy's own address
// is not a request's host. A NetLog Chrome had no time to finish (it ends
// with an event and a comma) is read up to its last whole event. This is a
// measurement, not a check: it always exits 0 and says when it could not
// measure.
import { readFileSync } from 'node:fs';

const [netlogFile, logFile] = process.argv.slice(2);
if (!netlogFile || !logFile) {
  console.error('usage: netlog-hosts.mjs <netlog file> <sidecar log>');
  process.exit(2);
}

function parseNetlog(text) {
  try {
    return JSON.parse(text);
  } catch {
    const end = text.lastIndexOf('},');
    if (end < 0) return null;
    try {
      return JSON.parse(`${text.slice(0, end + 1)}]}`);
    } catch {
      return null;
    }
  }
}

let opened = null;
let asked = null;
for (const line of readFileSync(logFile, 'utf8').split('\n')) {
  let entry;
  try {
    entry = JSON.parse(line);
  } catch {
    continue;
  }
  if (typeof entry?.time !== 'number') continue;
  if (opened === null && entry.msg === 'context opened') opened = entry.time;
  else if (opened !== null && asked === null && String(entry.req?.url ?? '').endsWith('/selftest')) asked = entry.time;
}

let netlog = null;
try {
  netlog = parseNetlog(readFileSync(netlogFile, 'utf8'));
} catch {
  netlog = null;
}
if (netlog === null || !Array.isArray(netlog.events) || opened === null || asked === null) {
  console.log('    hosts: not measured (no NetLog, or no window in the sidecar log)');
  process.exit(0);
}

const offset = Number(netlog.constants?.timeTickOffset ?? 0);
const address = /^(\d{1,3}(\.\d{1,3}){3}|\[[0-9a-f:.]+\])$/i;
const inside = new Map();
let addresses = 0;
let requests = 0;
for (const event of netlog.events) {
  const url = event?.params?.url;
  if (typeof url !== 'string' || !/^(https?|wss?):\/\//.test(url)) continue;
  const at = Number(event.time) + offset;
  if (at < opened || at > asked) continue;
  let host;
  try {
    host = new URL(url).hostname;
  } catch {
    continue;
  }
  // One request is logged by several events with its URL; a source id counts once.
  const key = `${host} ${event.source?.id ?? ''}`;
  if (inside.has(key)) continue;
  inside.set(key, host);
  requests += 1;
  if (address.test(host)) addresses += 1;
}
const counts = new Map();
for (const host of inside.values()) {
  if (address.test(host)) continue;
  counts.set(host, (counts.get(host) ?? 0) + 1);
}
const listed = [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
console.log(
  `    hosts asked for in the idle window (${Math.round((asked - opened) / 1000)} s): ${requests} requests, ${listed.length} hosts${addresses > 0 ? `, ${addresses} to an address (not shown)` : ''}`,
);
for (const [host, count] of listed) console.log(`      ${host} ${count}`);
