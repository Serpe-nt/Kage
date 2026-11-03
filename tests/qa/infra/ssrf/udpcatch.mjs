#!/usr/bin/env node
// A UDP catcher of the QA world: listens on the given UDP ports (a STUN
// server's 3478 among them), logs every datagram it receives and answers
// nothing. GET /__log on --control lists them; POST /__reset forgets them.
//
//   node udpcatch.mjs --ports 3478,19302,5353 --control 8054
import dgram from 'node:dgram';
import http from 'node:http';

const arg = (name, fallback) => {
  const at = process.argv.indexOf(`--${name}`);
  return at > 0 ? process.argv[at + 1] : fallback;
};
const ports = arg('ports', '3478').split(',').map(Number);
const control = Number(arg('control', '8054'));
let log = [];

for (const port of ports) {
  const socket = dgram.createSocket({ type: 'udp4', reuseAddr: true });
  socket.on('message', (msg, rinfo) => {
    log.push({ at: new Date().toISOString(), port, from: `${rinfo.address}:${rinfo.port}`, bytes: msg.length, head: msg.subarray(0, 24).toString('hex') });
  });
  socket.bind(port, '0.0.0.0');
}
http
  .createServer((req, res) => {
    if (req.url === '/__reset' && req.method === 'POST') {
      log = [];
      res.writeHead(200).end('ok');
      return;
    }
    res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(log));
  })
  .listen(control, '0.0.0.0');
