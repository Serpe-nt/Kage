// Checks of the observability end-to-end test (tests/e2e/observability.sh),
// run with node in a helper container of the compose world. Reads stdin.
//
//   node observability.mjs trace <request id>
//       stdin: Jaeger's answer to /api/v3/traces for the service kage (OTLP
//       in JSON; Jaeger 2.22 has no v1 search). Exits 0 when one trace holds
//       the request from the API to the browser: the spans of Kage
//       (api.request, pipeline.fetch, scheduler.wait, attempt,
//       sidecar.navigate as the client) and of a sidecar (sidecar.navigate as
//       the server, the child of Kage's), and no span says a URL's path, an
//       address or a secret. Prints the trace id and what it holds.
//
//   AUDIT_SECRETS=<one per line> node observability.mjs audit
//       stdin: every log line of every service and every span exported.
//       Exits 1 when a secret, a proxy credential header or an API token
//       appears; says where by line number and which kind, never what.

import { readFileSync } from 'node:fs';

const input = readFileSync(0, 'utf8');
const [command, ...rest] = process.argv.slice(2);

function fail(message) {
  console.error(`FAIL: ${message}`);
  process.exit(1);
}

/** An id as hex, whether Jaeger wrote it in hex or in base64. */
function hex(id) {
  if (typeof id !== 'string' || id === '') return '';
  return /^[0-9a-f]+$/i.test(id) ? id.toLowerCase() : Buffer.from(id, 'base64').toString('hex');
}

/** The value of an OTLP attribute, as text. */
function valueOf(value) {
  if (value === undefined || value === null) return '';
  for (const key of ['stringValue', 'intValue', 'doubleValue', 'boolValue']) {
    if (key in value) return String(value[key]);
  }
  return JSON.stringify(value);
}

/** Every span of an OTLP answer, flattened, with the service it is of. */
function spansOf(answer) {
  const spans = [];
  for (const resource of answer.result?.resourceSpans ?? answer.resourceSpans ?? []) {
    const service = valueOf(
      (resource.resource?.attributes ?? []).find((attribute) => attribute.key === 'service.name')?.value,
    );
    for (const scope of resource.scopeSpans ?? []) {
      for (const span of scope.spans ?? []) {
        spans.push({
          traceId: hex(span.traceId),
          spanId: hex(span.spanId),
          parentSpanId: hex(span.parentSpanId),
          name: span.name,
          kind: span.kind,
          service,
          attributes: Object.fromEntries(
            (span.attributes ?? []).map((attribute) => [attribute.key, valueOf(attribute.value)]),
          ),
        });
      }
    }
  }
  return spans;
}

if (command === 'trace') {
  const [request] = rest;
  const all = spansOf(JSON.parse(input));
  const root = all.find((span) => span.name === 'api.request' && span.attributes.request === request);
  if (root === undefined) {
    fail(`no trace of Kage holds the request ${request} (${all.length} spans)`);
  }
  const trace = all.filter((span) => span.traceId === root.traceId);
  const named = (name, from) => trace.filter((span) => span.name === name && from(span.service));
  const kage = (service) => service === 'kage';
  for (const name of ['api.request', 'pipeline.fetch', 'scheduler.wait', 'attempt']) {
    if (named(name, kage).length === 0) {
      fail(`the trace has no span ${name} of kage: ${trace.map((span) => span.name)}`);
    }
  }
  const client = named('sidecar.navigate', kage);
  const server = named('sidecar.navigate', (service) => service.startsWith('kage-sidecar-'));
  if (client.length === 0 || server.length === 0) {
    fail(`the trace does not reach a sidecar: ${trace.map((span) => `${span.name}@${span.service}`)}`);
  }
  if (!server.some((span) => client.some((parent) => parent.spanId === span.parentSpanId))) {
    fail("no sidecar span is the child of Kage's sidecar.navigate");
  }
  if (named('pipeline.fetch', kage)[0].parentSpanId !== root.spanId) {
    fail('pipeline.fetch is not the child of api.request');
  }
  // What a span may say: no URL with a path, no address, no secret.
  for (const span of trace) {
    for (const [key, value] of Object.entries(span.attributes)) {
      if (/[a-z]+:\/\/[^\s"/]+\/\S/.test(value)) fail(`span ${span.name} says a URL in ${key}`);
      if (/\b\d{1,3}(\.\d{1,3}){3}\b/.test(value)) fail(`span ${span.name} says an address in ${key}`);
      if (/token|password|secret|cookie|authorization/i.test(key)) fail(`span ${span.name} has the attribute ${key}`);
    }
  }
  console.log(
    JSON.stringify({
      trace: root.traceId,
      spans: trace.length,
      services: [...new Set(trace.map((span) => span.service))].sort(),
      operations: [...new Set(trace.map((span) => `${span.name}@${span.service}`))].sort(),
    }),
  );
} else if (command === 'audit') {
  const secrets = (process.env.AUDIT_SECRETS ?? '')
    .split('\n')
    .map((secret) => secret.trim())
    .filter((secret) => secret.length >= 8);
  if (secrets.length === 0) {
    fail('no secrets to look for');
  }
  const lines = input.split('\n');
  const found = [];
  lines.forEach((line, index) => {
    secrets.forEach((secret, which) => {
      if (line.includes(secret)) found.push(`line ${index + 1}: secret #${which + 1}`);
    });
    if (/proxy-authorization/i.test(line)) found.push(`line ${index + 1}: a Proxy-Authorization header`);
    if (/kage_[0-9a-f-]{36}_[A-Za-z0-9_-]{43}/.test(line)) found.push(`line ${index + 1}: an API token`);
    if (/Bearer\s+[A-Za-z0-9._-]{16,}/.test(line)) found.push(`line ${index + 1}: a bearer token`);
  });
  if (found.length > 0) {
    fail(`${found.length} finding(s): ${found.slice(0, 20).join('; ')}`);
  }
  console.log(JSON.stringify({ lines: lines.length, secrets: secrets.length, findings: 0 }));
} else {
  fail(`unknown command ${command}`);
}
