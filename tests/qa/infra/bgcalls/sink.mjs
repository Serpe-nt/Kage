#!/usr/bin/env node
// A stand-in for the hosts a browser calls by itself (QA round three,
// bgcalls). Listens on 443 and 80, reads the first bytes of each
// connection (the TLS ClientHello's server name, or an HTTP request line),
// logs who connected and what it asked for, and closes. Nothing is
// answered and nothing is forwarded anywhere.
//   node sink.mjs --control 8055       GET /__log (every connection), POST /__reset
import net from 'node:net';
import http from 'node:http';

const arg = (name, fallback) => {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : fallback;
};
const log = [];

function sniOf(buf) {
  try {
    if (buf[0] !== 0x16) return null;
    let p = 5 + 4 + 2 + 32; // record header, handshake header, version, random
    p += 1 + buf[p]; // session id
    p += 2 + buf.readUInt16BE(p); // cipher suites
    p += 1 + buf[p]; // compression
    const end = p + 2 + buf.readUInt16BE(p);
    p += 2;
    while (p + 4 <= end) {
      const type = buf.readUInt16BE(p);
      const len = buf.readUInt16BE(p + 2);
      if (type === 0) return buf.subarray(p + 9, p + 9 + buf.readUInt16BE(p + 7)).toString();
      p += 4 + len;
    }
  } catch {}
  return null;
}

for (const port of [443, 80]) {
  net
    .createServer((socket) => {
      const entry = { at: new Date().toISOString(), from: socket.remoteAddress, port };
      log.push(entry);
      socket.once('data', (buf) => {
        entry.sni = sniOf(buf);
        if (!entry.sni) entry.first = buf.subarray(0, 120).toString('latin1').split('\r\n')[0];
        socket.destroy();
      });
      socket.setTimeout(5000, () => socket.destroy());
      socket.on('error', () => {});
    })
    .listen(port, '0.0.0.0');
}

http
  .createServer((req, res) => {
    if (req.method === 'POST' && req.url === '/__reset') log.length = 0;
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify(log));
  })
  .listen(Number(arg('--control', '8055')), '0.0.0.0');
