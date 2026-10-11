#!/usr/bin/env node
// A receiver of a job's webhooks for the live check of jobs
// (tests/live/jobs.sh), as an operator would write one (spec §16.6). It
// listens on this machine's loopback only.
//
//   KAGE_LIVE_HOOK_SECRET=... node hooks-receiver.mjs --port N --fail-first 1
//
// Every delivery is verified: the signature is recomputed over the
// timestamp that was sent and the body that arrived, compared in constant
// time, and a timestamp older than five minutes is refused. The first
// `--fail-first` verified deliveries are answered 503, so that Kage tries
// them again; the others 204. GET /__deliveries returns what arrived, in
// order: when, which event, its id, whether it verified, what was answered,
// and the job as the event shows it. No address is recorded.
//
// The secret comes from the environment, not from the command line, so
// that it is not in the process list.
import crypto from 'node:crypto';
import http from 'node:http';

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 && args[index + 1] !== undefined ? args[index + 1] : fallback;
};
const port = Number(option('port', '0'));
const secret = process.env.KAGE_LIVE_HOOK_SECRET ?? '';
let failures = Number(option('fail-first', '0'));
if (!Number.isInteger(port) || port <= 0 || secret === '') {
  process.stderr.write('usage: KAGE_LIVE_HOOK_SECRET=... hooks-receiver.mjs --port N [--fail-first N]\n');
  process.exit(2);
}

/** Every delivery that arrived, in order. */
const deliveries = [];
const loopback = (socket) => /^(::ffff:)?127\.|^::1$/.test(socket.remoteAddress ?? '');

http
  .createServer((req, res) => {
    const path = new URL(req.url ?? '/', 'http://receiver.invalid').pathname;
    if (req.method === 'GET' && path === '/__deliveries') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(deliveries));
      return;
    }
    const chunks = [];
    req.on('data', (chunk) => chunks.push(chunk));
    req.on('end', () => {
      const body = Buffer.concat(chunks).toString('utf8');
      const timestamp = String(req.headers['x-kage-timestamp'] ?? '');
      const signed = String(req.headers['x-kage-signature'] ?? '');
      const expected = `sha256=${crypto.createHmac('sha256', secret).update(`${timestamp}.${body}`).digest('hex')}`;
      const age = Math.abs(Date.now() / 1000 - Number(timestamp));
      const verified =
        signed.length === expected.length &&
        crypto.timingSafeEqual(Buffer.from(signed), Buffer.from(expected)) &&
        age <= 300;
      let event = null;
      try {
        event = JSON.parse(body);
      } catch {
        // Recorded as it is: not JSON.
      }
      const status = !verified ? 401 : failures > 0 ? 503 : 204;
      if (verified && failures > 0) failures -= 1;
      deliveries.push({
        at: new Date().toISOString(),
        method: req.method,
        path,
        event: req.headers['x-kage-event'] ?? null,
        delivery: req.headers['x-kage-delivery'] ?? null,
        user_agent: req.headers['user-agent'] ?? null,
        content_type: req.headers['content-type'] ?? null,
        // A request that came through a proxy would say so.
        forwarding_headers: ['x-forwarded-for', 'forwarded', 'via', 'x-real-ip'].filter((name) => name in req.headers),
        from_loopback: loopback(req.socket),
        verified,
        age_s: Math.round(age),
        answered: status,
        body: event,
      });
      res.writeHead(status);
      res.end();
    });
  })
  .listen(port, '127.0.0.1');

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => process.exit(0));
}
