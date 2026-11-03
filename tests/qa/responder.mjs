#!/usr/bin/env node
// A programmable web server of the QA round. POST /__set with
//   {"path":"/x","status":302,"headers":{"location":"..."},"body":"...","delay_ms":0,
//    "base64":false,"host":"other.qa.test","times":[{...answer for the first request},{...second}]}
// (host is optional: an answer for that Host only; a path of "*" answers any
// path without an answer of its own; "times" answers in turn, the last repeats)
// GET /__log returns every request received; POST /__reset forgets everything.
// An unknown path answers 404.
import http from 'node:http';

const port = Number(process.argv[process.argv.indexOf('--port') + 1]);
let routes = new Map();
let log = [];
const counts = new Map();

http
  .createServer((req, res) => {
    const url = new URL(req.url ?? '/', 'http://responder.invalid');
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const body = Buffer.concat(chunks);
      if (url.pathname === '/__set' && req.method === 'POST') {
        let route;
        try {
          route = JSON.parse(body.toString('utf8'));
        } catch {
          // Round two: an empty or broken body is refused, not a crash that
          // forgets every route.
          res.writeHead(400).end('not JSON');
          return;
        }
        routes.set(`${route.host ?? ''}|${route.path}`, route);
        counts.delete(`${route.host ?? ''}|${route.path}`);
        res.writeHead(200).end('ok');
        return;
      }
      if (url.pathname === '/__reset' && req.method === 'POST') {
        routes = new Map();
        log = [];
        counts.clear();
        res.writeHead(200).end('ok');
        return;
      }
      if (url.pathname === '/__log') {
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify(log));
        return;
      }
      const host = (req.headers.host ?? '').replace(/:\d+$/, '');
      log.push({
        at: new Date().toISOString(),
        from: (req.socket.remoteAddress ?? '').replace(/^::ffff:/, ''),
        method: req.method,
        host,
        url: req.url,
        headers: req.headers,
        body: body.toString('utf8').slice(0, 4096),
      });
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
      const answer = () => {
        res.writeHead(route.status ?? 200, route.headers ?? { 'content-type': 'text/html; charset=utf-8' });
        res.end(route.base64 ? Buffer.from(route.body ?? '', 'base64') : (route.body ?? ''));
      };
      setTimeout(answer, route.delay_ms ?? 0);
    });
  })
  .listen(port, '0.0.0.0');

for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(0));
