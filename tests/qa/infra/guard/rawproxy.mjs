#!/usr/bin/env node
// QA round four, guard topic: a "residential proxy" of the e2e world (as
// tests/e2e/world.mjs proxy: Basic authentication, relays plain requests and
// CONNECT tunnels to an allow list) that also LOGS what it received raw:
// the request line's target exactly as sent, the Host header, the header
// names (no values of Proxy-Authorization), for the G5 checks (the
// forwarder relays the target it parsed and judged).
//
//   node rawproxy.mjs --port 3128 --user U --pass P --allow h:p,h:p
//        [--answer h:p=STATUS,h:p=STATUS]   answer that target with STATUS
//                                           (403, 407, 502 ...) instead
//
// Allow and answer lists are matched on the target lower-cased with one
// trailing dot of the host dropped (a provider resolves both alike); the
// log keeps the raw text.
//   GET /__raw        the log (asked directly, not as a proxy)
//   POST /__rawreset  forget it
//   GET /             world.mjs's counters, so /scripts/qa/ssrf/lib.mjs's proxies() works
import http from 'node:http';
import net from 'node:net';

const option = (name, fallback) => {
  const at = process.argv.indexOf(`--${name}`);
  return at > 0 ? process.argv[at + 1] : fallback;
};
const port = Number(option('port', '3128'));
const expected = `Basic ${Buffer.from(`${option('user', '')}:${option('pass', '')}`).toString('base64')}`;
const norm = (target) => {
  const t = String(target).toLowerCase();
  const m = t.match(/^(\[[^\]]*\]|[^:]*)(:\d+)?$/);
  if (!m) return t;
  return m[1].replace(/\.$/, '') + (m[2] ?? '');
};
const allowed = new Set(option('allow', '').split(',').filter(Boolean).map(norm));
const answers = new Map(
  option('answer', '')
    .split(',')
    .filter(Boolean)
    .map((pair) => {
      const [t, s] = pair.split('=');
      return [norm(t), Number(s)];
    }),
);
let log = [];
const targets = {};

const record = (kind, req, decided) => {
  const names = [];
  for (let i = 0; i < req.rawHeaders.length; i += 2) {
    const name = req.rawHeaders[i];
    const value = name.toLowerCase() === 'proxy-authorization' ? '(given)' : req.rawHeaders[i + 1];
    names.push([name, value]);
  }
  log.push({
    at: new Date().toISOString(),
    from: (req.socket.remoteAddress ?? '').replace(/^::ffff:/, ''),
    kind,
    method: req.method,
    target: req.url,
    version: req.httpVersion,
    host: req.headers.host ?? null,
    headers: names,
    decided,
  });
  if (log.length > 5000) log = log.slice(-4000);
};

const server = http.createServer((req, res) => {
  if ((req.url ?? '').startsWith('/')) {
    if (req.url === '/__raw') {
      res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(log));
      return;
    }
    if (req.url === '/__rawreset') {
      log = [];
      res.writeHead(200).end('ok');
      return;
    }
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ targets, refused_credentials: 0, refused_targets: 0 }));
    return;
  }
  let url;
  try {
    url = new URL(req.url);
  } catch {
    record('plain', req, 'bad-url-400');
    res.writeHead(400).end();
    return;
  }
  const target = norm(`${url.hostname}:${url.port || '80'}`);
  if (req.headers['proxy-authorization'] !== expected) {
    record('plain', req, '407-credential');
    res.writeHead(407, { 'proxy-authenticate': 'Basic realm="provider"', connection: 'close' }).end();
    return;
  }
  if (answers.has(target)) {
    record('plain', req, `answer-${answers.get(target)}`);
    res.writeHead(answers.get(target), answers.get(target) === 407 && process.argv.includes('--challenge') ? { 'proxy-authenticate': 'Basic realm="provider"', connection: 'close' } : { connection: 'close' }).end();
    return;
  }
  if (!allowed.has(target)) {
    record('plain', req, '403-not-allowed');
    res.writeHead(403, { connection: 'close' }).end();
    return;
  }
  record('plain', req, 'relayed');
  (targets[target] ??= { connections: 0 }).connections += 1;
  const headers = { ...req.headers };
  delete headers['proxy-authorization'];
  delete headers['proxy-connection'];
  const host = url.hostname.replace(/\.$/, '');
  const upstream = http.request(
    { host, port: Number(url.port || 80), method: req.method, path: url.pathname + url.search, headers },
    (answer) => {
      res.writeHead(answer.statusCode ?? 502, answer.rawHeaders);
      answer.pipe(res);
    },
  );
  upstream.on('error', () => {
    if (!res.headersSent) res.writeHead(502);
    res.end();
  });
  req.pipe(upstream);
});

server.on('connect', (req, socket, head) => {
  const raw = req.url ?? '';
  const target = norm(raw);
  socket.on('error', () => {});
  if (req.headers['proxy-authorization'] !== expected) {
    record('connect', req, '407-credential');
    socket.end('HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm="provider"\r\nContent-Length: 0\r\n\r\n');
    return;
  }
  if (answers.has(target)) {
    const s = answers.get(target);
    record('connect', req, `answer-${s}`);
    socket.end(`HTTP/1.1 ${s} Refused\r\nContent-Length: 0\r\n\r\n`);
    return;
  }
  if (!allowed.has(target)) {
    record('connect', req, '403-not-allowed');
    socket.end('HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n');
    return;
  }
  record('connect', req, 'relayed');
  (targets[target] ??= { connections: 0 }).connections += 1;
  const at = target.lastIndexOf(':');
  const host = target.slice(0, at).replace(/^\[|\]$/g, '');
  const upstream = net.connect(Number(target.slice(at + 1)), host, () => {
    socket.write('HTTP/1.1 200 Connection Established\r\n\r\n');
    upstream.write(head);
    upstream.pipe(socket);
    socket.pipe(upstream);
  });
  upstream.on('error', () => socket.end('HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n'));
  socket.on('error', () => upstream.destroy());
});
server.on('connection', (socket) => socket.on('error', () => {}));
server.listen(port, '0.0.0.0');
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(0));
