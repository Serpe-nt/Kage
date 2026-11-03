#!/usr/bin/env node
// QA round two, topic tls: the world's https side. One process, no
// dependencies (node 24 of the browser image):
//
//   node tlsworld.mjs --certs DIR --https-port 443 --http-port 80 --fixture fixture:8088
//
// - On --https-port, TLS with the certificate the client's SNI asks for:
//     tls.kage.test, tls2.kage.test, echo.tls.kage.test   good.pem (signed by ca.pem)
//     secure.kage.test                                    good.pem, and the decrypted
//                                                         stream is piped as it is to
//                                                         the fixture site (--fixture)
//     expired.kage.test                                   expired.pem
//     wrongname.kage.test                                 wrongname.pem (names nothere.kage.test)
//     unknownca.kage.test                                 unknownca.pem (signed by other-ca)
//     selfsigned.kage.test                                selfsigned.pem
//     www.tlsf1.test ... www.tlsf9.test                   fresh.pem (signed by ca.pem)
//     anything else, or no SNI                            good.pem
//   ALPN offers http/1.1 only.
// - On --http-port, the same programmable responder in plain http (for
//   redirects between the schemes of one host).
// - The responder is /scripts/qa/responder.mjs's: POST /__set with
//   {"path","status","headers","body","delay_ms","base64","host","times"},
//   GET /__log, POST /__reset, on either port and either scheme, one state.
//   Every logged request also has `scheme`, `port`, `sni`, `alpn`, `tls`
//   (protocol). A handshake that failed (the client refused the
//   certificate) is logged under GET /__handshakes with the SNI and the
//   error, so a test sees that a client got as far as TLS and no further.
// - GET /__echo answers the request as JSON (headers in wire order, SNI,
//   protocol, cipher), for any host.
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import path from 'node:path';
import tls from 'node:tls';

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 && args[i + 1] !== undefined ? args[i + 1] : fallback;
};
const certs = option('certs', '/qa/infra/tls/certs');
const httpsPort = Number(option('https-port', '443'));
const httpPort = Number(option('http-port', '80'));
const fixture = option('fixture', 'fixture:8088');

const pair = (name) => ({
  key: fs.readFileSync(path.join(certs, `${name}.key`)),
  cert: fs.readFileSync(path.join(certs, `${name}.pem`)),
});
const contexts = Object.fromEntries(
  ['good', 'fresh', 'expired', 'wrongname', 'unknownca', 'selfsigned'].map((n) => [n, tls.createSecureContext(pair(n))]),
);
const variantOf = (servername) =>
  ({
    'expired.kage.test': 'expired',
    'wrongname.kage.test': 'wrongname',
    'unknownca.kage.test': 'unknownca',
    'selfsigned.kage.test': 'selfsigned',
  })[String(servername ?? '').toLowerCase()] ??
  (/^www\.tlsf\d\.test$/i.test(String(servername ?? '')) ? 'fresh' : 'good');

let routes = new Map();
let log = [];
let handshakes = [];
const counts = new Map();

const handler = (req, res) => {
  const socket = req.socket;
  const secure = Boolean(socket.encrypted);
  const url = new URL(req.url ?? '/', 'http://responder.invalid');
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    const body = Buffer.concat(chunks);
    if (url.pathname === '/__set' && req.method === 'POST') {
      const route = JSON.parse(body.toString('utf8'));
      routes.set(`${route.host ?? ''}|${route.path}`, route);
      counts.delete(`${route.host ?? ''}|${route.path}`);
      res.writeHead(200).end('ok');
      return;
    }
    if (url.pathname === '/__reset' && req.method === 'POST') {
      routes = new Map();
      log = [];
      handshakes = [];
      counts.clear();
      res.writeHead(200).end('ok');
      return;
    }
    if (url.pathname === '/__log') {
      res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(log));
      return;
    }
    if (url.pathname === '/__handshakes') {
      res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(handshakes));
      return;
    }
    const host = (req.headers.host ?? '').replace(/:\d+$/, '');
    const entry = {
      at: new Date().toISOString(),
      from: (socket.remoteAddress ?? '').replace(/^::ffff:/, ''),
      scheme: secure ? 'https' : 'http',
      port: socket.localPort,
      sni: secure ? (socket.servername ?? null) : null,
      alpn: secure ? (socket.alpnProtocol ?? null) : null,
      tls: secure ? socket.getProtocol() : null,
      method: req.method,
      host,
      url: req.url,
      headers: req.headers,
      raw_headers: req.rawHeaders,
      body: body.toString('utf8').slice(0, 4096),
    };
    log.push(entry);
    if (url.pathname === '/__echo') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ ...entry, cipher: secure ? socket.getCipher() : null }));
      return;
    }
    const key = [`${host}|${url.pathname}`, `|${url.pathname}`, `${host}|*`, '|*'].find((k) => routes.has(k));
    if (!key) {
      res.writeHead(404, { 'content-type': 'text/plain' }).end('not programmed');
      return;
    }
    let route = routes.get(key);
    if (route.times) {
      const seen = counts.get(key) ?? 0;
      counts.set(key, seen + 1);
      route = route.times[Math.min(seen, route.times.length - 1)];
    }
    setTimeout(() => {
      res.writeHead(route.status ?? 200, route.headers ?? { 'content-type': 'text/html; charset=utf-8' });
      res.end(route.base64 ? Buffer.from(route.body ?? '', 'base64') : (route.body ?? ''));
    }, route.delay_ms ?? 0);
  });
};

const web = http.createServer(handler);
web.listen(httpPort, '0.0.0.0');

const [fixtureHost, fixturePort] = fixture.split(':');
const secureServer = tls.createServer(
  {
    ...pair('good'),
    ALPNProtocols: ['http/1.1'],
    SNICallback: (servername, cb) => cb(null, contexts[variantOf(servername)]),
  },
  (socket) => {
    socket.on('error', () => {});
    if (String(socket.servername ?? '').toLowerCase() === 'secure.kage.test') {
      // The fixture site behind TLS: the stream as it is, so its own log
      // keeps the header lines in wire order.
      const upstream = net.connect(Number(fixturePort), fixtureHost);
      upstream.on('error', () => socket.destroy());
      socket.pipe(upstream);
      upstream.pipe(socket);
      return;
    }
    web.emit('connection', socket);
  },
);
secureServer.on('tlsClientError', (error, socket) => {
  handshakes.push({
    at: new Date().toISOString(),
    from: (socket.remoteAddress ?? '').replace(/^::ffff:/, ''),
    sni: socket.servername ?? null,
    error: String(error.code ?? error.message).slice(0, 200),
    reason: String(error.message ?? '').slice(0, 200),
  });
});
secureServer.listen(httpsPort, '0.0.0.0');

for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(0));
