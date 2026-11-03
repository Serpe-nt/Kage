#!/usr/bin/env node
// A programmable DNS server of the QA world (node, no packages).
//
//   node dns.mjs --port 53 --control 8053 [--forward 127.0.0.11]
//
// UDP on --port answers A, AAAA and CNAME questions for the names it was
// told about; every other question under a programmed zone is NXDOMAIN.
// A name it was not told about is forwarded to --forward (Docker's own
// resolver of this container) only when it ends in `.test`, `.invalid` or
// has one label, and is REFUSED otherwise: nothing of a real name ever
// leaves the closed world from here.
//
// HTTP on --control:
//   POST /__set   {"name":"r1.rebind.test", "a":["1.2.3.4"], "aaaa":["::1"],
//                  "cname":"other.name", "ttl":0, "rcode":"NXDOMAIN",
//                  "times":[{"a":[..]},{"a":[..]}],     answers in turn, per
//                                                     (name, type); the last repeats
//                  "by":{"11.66.1.20":{"a":[..]}}}    an answer for one querier
//                 A name of "*.zone" answers every name under the zone that
//                 has no entry of its own.
//   POST /__reset forgets every name and the log
//   GET  /__log   every question: {at, from, name, type, answer, turn}
import dgram from 'node:dgram';
import http from 'node:http';
import net from 'node:net';

const arg = (name, fallback) => {
  const at = process.argv.indexOf(`--${name}`);
  return at > 0 ? process.argv[at + 1] : fallback;
};
const port = Number(arg('port', '53'));
const control = Number(arg('control', '8053'));
const forward = arg('forward', '');

let names = new Map();
let log = [];
const counts = new Map();
const TYPES = { 1: 'A', 5: 'CNAME', 28: 'AAAA', 255: 'ANY' };
const RCODES = { NOERROR: 0, SERVFAIL: 2, NXDOMAIN: 3, REFUSED: 5 };

const readName = (buf, offset) => {
  const labels = [];
  let at = offset;
  let end = -1;
  for (let jumps = 0; jumps < 32; jumps += 1) {
    const length = buf[at];
    if (length === 0) {
      at += 1;
      break;
    }
    if ((length & 0xc0) === 0xc0) {
      if (end < 0) end = at + 2;
      at = ((length & 0x3f) << 8) | buf[at + 1];
      continue;
    }
    labels.push(buf.subarray(at + 1, at + 1 + length).toString('latin1'));
    at += 1 + length;
  }
  return { name: labels.join('.'), next: end < 0 ? at : end };
};
const encodeName = (name) => {
  const parts = name.replace(/\.$/, '').split('.').filter(Boolean);
  const out = [];
  for (const part of parts) {
    const bytes = Buffer.from(part, 'latin1');
    out.push(Buffer.from([bytes.length]), bytes);
  }
  out.push(Buffer.from([0]));
  return Buffer.concat(out);
};
const v6bytes = (address) => {
  // Node's own parser by way of a SocketAddress, which accepts every form.
  const sa = new net.SocketAddress({ address, family: 'ipv6' });
  const full = sa.address;
  const [head, tail] = full.includes('::') ? full.split('::') : [full, ''];
  let h = head ? head.split(':') : [];
  let t = tail ? tail.split(':') : [];
  const expand = (groups) => groups.flatMap((g) => {
    if (g.includes('.')) {
      const o = g.split('.').map(Number);
      return [((o[0] << 8) | o[1]).toString(16), ((o[2] << 8) | o[3]).toString(16)];
    }
    return [g];
  });
  h = expand(h);
  t = expand(t);
  const groups = [...h, ...Array(8 - h.length - t.length).fill('0'), ...t];
  const b = Buffer.alloc(16);
  groups.forEach((g, i) => b.writeUInt16BE(parseInt(g, 16), i * 2));
  return b;
};
const record = (type, ttl, rdata, nameBuf) => {
  const head = Buffer.alloc(10);
  head.writeUInt16BE(type, 0);
  head.writeUInt16BE(1, 2);
  head.writeUInt32BE(ttl, 4);
  head.writeUInt16BE(rdata.length, 8);
  return Buffer.concat([nameBuf, head, rdata]);
};
const lookup = (name) => {
  const key = name.toLowerCase().replace(/\.$/, '');
  if (names.has(key)) return { key, entry: names.get(key) };
  const labels = key.split('.');
  for (let i = 1; i < labels.length; i += 1) {
    const wild = `*.${labels.slice(i).join('.')}`;
    if (names.has(wild)) return { key: wild, entry: names.get(wild) };
  }
  return null;
};
const inZone = (name) => {
  const key = name.toLowerCase().replace(/\.$/, '');
  for (const k of names.keys()) {
    const zone = k.replace(/^\*\./, '');
    if (key === zone || key.endsWith(`.${zone}`)) return true;
  }
  return false;
};
const answerFor = (entry, key, typeName, from) => {
  let chosen = entry;
  let turn = null;
  if (entry.by && entry.by[from]) chosen = { ...entry, ...entry.by[from], times: entry.by[from].times };
  if (chosen.times && chosen.times.length > 0) {
    const counter = `${key}|${typeName}|${entry.by && entry.by[from] ? from : ''}`;
    const seen = counts.get(counter) ?? 0;
    counts.set(counter, seen + 1);
    turn = seen;
    chosen = { ...chosen, ...chosen.times[Math.min(seen, chosen.times.length - 1)] };
  }
  return { chosen, turn };
};

const server = dgram.createSocket('udp4');
server.on('message', (msg, rinfo) => {
  if (msg.length < 12) return;
  const id = msg.readUInt16BE(0);
  const flags = msg.readUInt16BE(2);
  const { name, next } = readName(msg, 12);
  const qtype = msg.readUInt16BE(next);
  const question = msg.subarray(12, next + 4);
  const typeName = TYPES[qtype] ?? String(qtype);
  const from = rinfo.address;
  const found = lookup(name);
  const entryLog = { at: new Date().toISOString(), from, name, type: typeName };
  if (!found) {
    const lower = name.toLowerCase().replace(/\.$/, '');
    const forwardable = forward && !inZone(lower) && (/\.(test|invalid)$/.test(lower) || !lower.includes('.'));
    if (forwardable) {
      const out = dgram.createSocket('udp4');
      const timer = setTimeout(() => out.close(), 3000);
      out.on('message', (reply) => {
        clearTimeout(timer);
        server.send(reply, rinfo.port, rinfo.address);
        out.close();
      });
      out.send(msg, 53, forward);
      log.push({ ...entryLog, answer: 'forwarded' });
      return;
    }
    const rcode = inZone(lower) ? RCODES.NXDOMAIN : RCODES.REFUSED;
    const head = Buffer.alloc(12);
    head.writeUInt16BE(id, 0);
    head.writeUInt16BE(0x8000 | 0x0400 | (flags & 0x0100) | 0x0080 | rcode, 2);
    head.writeUInt16BE(1, 4);
    server.send(Buffer.concat([head, question]), rinfo.port, rinfo.address);
    log.push({ ...entryLog, answer: rcode === RCODES.NXDOMAIN ? 'NXDOMAIN' : 'REFUSED' });
    return;
  }
  const { chosen, turn } = answerFor(found.entry, found.key, typeName, from);
  const ttl = Number(chosen.ttl ?? 0);
  const rcode = RCODES[chosen.rcode ?? 'NOERROR'] ?? 0;
  const answers = [];
  const pointer = Buffer.from([0xc0, 0x0c]);
  let owner = pointer;
  const shown = [];
  if (chosen.cname && qtype !== 5) {
    answers.push(record(5, ttl, encodeName(chosen.cname), pointer));
    shown.push(`CNAME ${chosen.cname}`);
    owner = encodeName(chosen.cname);
  }
  if (qtype === 1 || qtype === 255) {
    for (const a of chosen.a ?? []) {
      answers.push(record(1, ttl, Buffer.from(a.split('.').map(Number)), owner));
      shown.push(a);
    }
  }
  if (qtype === 28 || qtype === 255) {
    for (const a of chosen.aaaa ?? []) {
      answers.push(record(28, ttl, v6bytes(a), owner));
      shown.push(a);
    }
  }
  if (qtype === 5 && chosen.cname) {
    answers.push(record(5, ttl, encodeName(chosen.cname), pointer));
    shown.push(`CNAME ${chosen.cname}`);
  }
  const head = Buffer.alloc(12);
  head.writeUInt16BE(id, 0);
  head.writeUInt16BE(0x8000 | 0x0400 | (flags & 0x0100) | 0x0080 | rcode, 2);
  head.writeUInt16BE(1, 4);
  head.writeUInt16BE(rcode === 0 ? answers.length : 0, 6);
  const reply = Buffer.concat([head, question, ...(rcode === 0 ? answers : [])]);
  server.send(reply, rinfo.port, rinfo.address);
  log.push({ ...entryLog, answer: rcode === 0 ? shown : chosen.rcode, turn });
  if (log.length > 20000) log = log.slice(-10000);
});
server.bind(port, '0.0.0.0');

http
  .createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const url = new URL(req.url ?? '/', 'http://dns.invalid');
      if (url.pathname === '/__set' && req.method === 'POST') {
        try {
          const entry = JSON.parse(Buffer.concat(chunks).toString('utf8'));
          const key = String(entry.name).toLowerCase().replace(/\.$/, '');
          names.set(key, entry);
          for (const k of [...counts.keys()]) if (k.startsWith(`${key}|`)) counts.delete(k);
          res.writeHead(200).end('ok');
        } catch (error) {
          res.writeHead(400).end(String(error));
        }
        return;
      }
      if (url.pathname === '/__reset' && req.method === 'POST') {
        names = new Map();
        log = [];
        counts.clear();
        res.writeHead(200).end('ok');
        return;
      }
      if (url.pathname === '/__log') {
        res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(log));
        return;
      }
      res.writeHead(404).end('no');
    });
  })
  .listen(control, '0.0.0.0');
