// Measures what the forwarder adds to a connection across the compose
// bridge (tests/e2e/fetch.sh pipes this into a sidecar container's node).
//
// Each sample is one new TCP connection and one plain request for the
// echo's /ip: straight to a provider proxy with the provider's credential,
// and through Kage's forwarder with a persona's credential, which then
// goes through that persona's own provider proxy. The difference is the
// forwarder: one more hop on the bridge, its lookup of the persona's
// proxy, and the rewritten head.
//
// Environment: MASTER_KEY (64 hex), PERSONA (uuid), FORWARDER (host:port),
// PROXY (host:port), PROXY_USER, PROXY_PASS, TARGET (http URL), SAMPLES.
import { createHmac } from 'node:crypto';
import net from 'node:net';

const env = process.env;
const samples = Number(env.SAMPLES ?? '40');

/** The token of a persona, as kage_scheduler::forwarder::Tokens makes it. */
const token = (masterKeyHex, persona) => {
  const key = createHmac('sha256', Buffer.from(masterKeyHex, 'hex')).update('kage-forwarder-key-v1').digest();
  const id = Buffer.from(persona.replaceAll('-', ''), 'hex');
  return createHmac('sha256', key).update('kage-forwarder-token-v1').update(id).digest('hex').slice(0, 32);
};

const once = (address, username, password) =>
  new Promise((resolve, reject) => {
    const [host, port] = address.split(':');
    const started = process.hrtime.bigint();
    const socket = net.connect(Number(port), host);
    let answer = '';
    socket.setTimeout(10_000, () => socket.destroy(new Error('timeout')));
    socket.on('connect', () => {
      const credential = Buffer.from(`${username}:${password}`).toString('base64');
      socket.write(
        `GET ${env.TARGET} HTTP/1.1\r\nHost: ${new URL(env.TARGET).host}\r\nProxy-Authorization: Basic ${credential}\r\nConnection: close\r\n\r\n`,
      );
    });
    socket.on('data', (chunk) => (answer += chunk));
    socket.on('error', reject);
    socket.on('close', () => {
      if (!answer.startsWith('HTTP/1.1 200')) {
        reject(new Error(`unexpected answer from ${address}: ${answer.slice(0, 80)}`));
        return;
      }
      resolve(Number(process.hrtime.bigint() - started) / 1e6);
    });
  });

const summary = (times) => {
  const sorted = [...times].sort((a, b) => a - b);
  const at = (share) => sorted[Math.min(sorted.length - 1, Math.floor(share * sorted.length))];
  const round = (value) => Math.round(value * 100) / 100;
  return { median_ms: round(at(0.5)), p95_ms: round(at(0.95)), min_ms: round(sorted[0]) };
};

const direct = [];
const forwarded = [];
const personaToken = token(env.MASTER_KEY, env.PERSONA);
// A few connections first, so that neither side pays for a cold start.
for (let warm = 0; warm < 3; warm += 1) {
  await once(env.PROXY, env.PROXY_USER, env.PROXY_PASS);
  await once(env.FORWARDER, env.PERSONA, personaToken);
}
for (let sample = 0; sample < samples; sample += 1) {
  direct.push(await once(env.PROXY, env.PROXY_USER, env.PROXY_PASS));
  forwarded.push(await once(env.FORWARDER, env.PERSONA, personaToken));
}
const result = { samples, direct: summary(direct), forwarder: summary(forwarded) };
result.added_median_ms = Math.round((result.forwarder.median_ms - result.direct.median_ms) * 100) / 100;
result.added_p95_ms = Math.round((result.forwarder.p95_ms - result.direct.p95_ms) * 100) / 100;
console.log(JSON.stringify(result));
