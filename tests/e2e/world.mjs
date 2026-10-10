#!/usr/bin/env node
// The world outside Kage for the compose end-to-end test of the fetch
// pipeline (tests/e2e/fetch.sh). It has no dependencies and never reaches
// the internet. Two roles:
//
//   node world.mjs proxy --port 3128 --user U --pass P --allow host:port,host:port
//     A residential proxy as a provider runs one: Basic authentication,
//     absolute-form requests and CONNECT. Only the targets named in
//     --allow are relayed, so a browser's own background traffic ends
//     here. It counts the bytes of every client connection, by the first
//     target asked for on it: what a provider would bill.
//     GET /__stats (asked directly, not as a proxy) returns the counts.
//
//   node world.mjs echo --port 8089
//     What the proxy health probe asks: /ip answers with the address the
//     request came from, /geo places that address in Berlin.
import http from 'node:http';
import net from 'node:net';

const [role, ...args] = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 && args[index + 1] !== undefined ? args[index + 1] : fallback;
};
const port = Number(option('port', '0'));
const remote = (socket) => (socket.remoteAddress ?? '').replace(/^::ffff:/, '');

if (role === 'echo') {
  http
    .createServer((req, res) => {
      const ip = remote(req.socket);
      const path = new URL(req.url ?? '/', 'http://echo.invalid').pathname;
      if (path === '/geo') {
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(
          JSON.stringify({
            ip,
            city: 'Berlin',
            region: 'Berlin',
            country: 'DE',
            loc: '52.5200,13.4050',
            org: 'AS64500 Example Broadband',
            timezone: 'Europe/Berlin',
          }),
        );
        return;
      }
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end(`${ip}\n`);
    })
    .listen(port, '0.0.0.0');
} else if (role === 'proxy') {
  const expected = `Basic ${Buffer.from(`${option('user', '')}:${option('pass', '')}`).toString('base64')}`;
  const allowed = new Set(option('allow', '').split(',').filter(Boolean));
  /** Bytes per first target of a connection: what came in, what went out. */
  const targets = {};
  let refusedCredentials = 0;
  let refusedTargets = 0;

  const account = (socket) => {
    const target = socket.kageTarget ?? '(nothing asked)';
    const entry = (targets[target] ??= { connections: 0, bytes_in: 0, bytes_out: 0 });
    entry.connections += 1;
    entry.bytes_in += socket.bytesRead;
    entry.bytes_out += socket.bytesWritten;
  };

  const server = http.createServer((req, res) => {
    // Asked as a web server, not as a proxy: the counters.
    if ((req.url ?? '').startsWith('/')) {
      req.socket.kageTarget = '(stats)';
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ targets, refused_credentials: refusedCredentials, refused_targets: refusedTargets }));
      return;
    }
    let url;
    try {
      url = new URL(req.url);
    } catch {
      res.writeHead(400).end();
      return;
    }
    const target = `${url.hostname}:${url.port || '80'}`;
    req.socket.kageTarget ??= target;
    if (req.headers['proxy-authorization'] !== expected) {
      refusedCredentials += 1;
      res.writeHead(407, { 'proxy-authenticate': 'Basic realm="provider"', connection: 'close' }).end();
      return;
    }
    if (!allowed.has(target)) {
      refusedTargets += 1;
      res.writeHead(403, { connection: 'close' }).end();
      return;
    }
    const headers = { ...req.headers };
    delete headers['proxy-authorization'];
    delete headers['proxy-connection'];
    const upstream = http.request(
      { host: url.hostname, port: Number(url.port || 80), method: req.method, path: url.pathname + url.search, headers },
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
    const target = req.url ?? '';
    socket.kageTarget ??= target;
    if (req.headers['proxy-authorization'] !== expected) {
      refusedCredentials += 1;
      socket.end('HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm="provider"\r\nContent-Length: 0\r\n\r\n');
      return;
    }
    if (!allowed.has(target)) {
      refusedTargets += 1;
      socket.end('HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n');
      return;
    }
    const [host, targetPort] = target.split(':');
    const upstream = net.connect(Number(targetPort), host, () => {
      socket.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      upstream.write(head);
      upstream.pipe(socket);
      socket.pipe(upstream);
    });
    upstream.on('error', () => socket.end('HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n'));
    socket.on('error', () => upstream.destroy());
  });

  server.on('connection', (socket) => {
    socket.on('close', () => account(socket));
    socket.on('error', () => {});
  });
  server.listen(port, '0.0.0.0');
} else {
  process.stderr.write('usage: world.mjs proxy|echo --port N ...\n');
  process.exit(2);
}

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => process.exit(0));
}
