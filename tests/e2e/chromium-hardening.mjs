// Runs inside the browser-chromium container; tests/e2e/smoke.sh pipes it to
// node. It checks what spec §19 and §22 ask of the container while a real
// context is open: a non-root user, a read-only root filesystem, the pinned
// Chrome, and Chrome's own sandbox around every renderer.
//
// The context gets a proxy address nobody listens on and is never navigated:
// the check needs a running browser, not a page.
import { randomBytes, randomUUID } from 'node:crypto';
import { readFileSync, readdirSync, writeFileSync } from 'node:fs';

const origin = `http://127.0.0.1:${process.env.SIDECAR_PORT ?? '8090'}`;
const token = process.env.SIDECAR_TOKEN ?? '';

function fail(message) {
  process.stderr.write(`FAIL: ${message}\n`);
  process.exit(1);
}

async function call(method, path, body) {
  const response = await fetch(new URL(path, origin), {
    method,
    headers: {
      'x-kage-sidecar-token': token,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    signal: AbortSignal.timeout(60_000),
  });
  const text = await response.text();
  if (!response.ok) {
    fail(`${method} ${path} answered ${response.status}: ${text}`);
  }
  return text === '' ? null : JSON.parse(text);
}

/**
 * Every Chrome process of the container as `{ pid, command, status }`.
 *
 * The command line is read as one string: Chrome rewrites the title of the
 * processes it forks from its zygote, which leaves their arguments
 * separated by spaces and not by NUL bytes.
 */
function chromeProcesses() {
  const found = [];
  for (const entry of readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) {
      continue;
    }
    try {
      const command = readFileSync(`/proc/${entry}/cmdline`, 'utf8').replaceAll('\0', ' ').trim();
      if (command.startsWith('/opt/google/chrome/chrome')) {
        const status = readFileSync(`/proc/${entry}/status`, 'utf8');
        found.push({ pid: Number(entry), command, status });
      }
    } catch {
      // The process ended between the listing and the read.
    }
  }
  return found;
}

const flags = (command) => command.split(/\s+/);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const field = (status, name) =>
  (new RegExp(`^${name}:\\s*(.*)$`, 'm').exec(status)?.[1] ?? '').trim();

if (process.getuid() !== 65532) {
  fail(`the sidecar runs as uid ${process.getuid()}, not 65532`);
}
try {
  writeFileSync('/app/sidecar/kage-probe', 'x');
  fail('the root filesystem is writable');
} catch (error) {
  if (error.code !== 'EROFS') {
    fail(`writing to the root filesystem failed with ${error.code}, not EROFS`);
  }
}

const pinned = readFileSync('/etc/kage/chrome-version', 'utf8').trim();
const health = await call('GET', '/v1/health');
if (health.browser_version !== pinned) {
  fail(`health reports Chrome ${health.browser_version}, the image records ${pinned}`);
}

const personaId = randomUUID();
const generated = await call('POST', '/v1/personas/generate', {
  seed: randomBytes(32).toString('hex'),
  claimed_os: 'linux',
  languages: ['en-GB', 'en'],
});
const opened = await call('POST', '/v1/contexts', {
  persona_id: personaId,
  fingerprint: generated.fingerprint,
  proxy: { server: 'http://127.0.0.1:9', username: 'smoke', password: 'smoke-test-only' },
  locale: 'en-GB',
  languages: ['en-GB', 'en'],
  timezone: 'Europe/London',
  geolocation: null,
  screen: generated.summary.screen,
  profile_dir: `personas/${personaId}`,
});

let report;
try {
  // The first renderer starts a moment after the window opens.
  let chrome = [];
  let renderers = [];
  for (let attempt = 0; attempt < 50 && renderers.length === 0; attempt += 1) {
    await sleep(200);
    chrome = chromeProcesses();
    renderers = chrome.filter((entry) => flags(entry.command).includes('--type=renderer'));
  }
  const browser = chrome.find(
    (entry) => !flags(entry.command).some((flag) => flag.startsWith('--type=')),
  );
  if (browser === undefined || renderers.length === 0) {
    fail(
      `expected a browser and a renderer, found ${chrome.length} Chrome processes: ${chrome.map((entry) => entry.command.slice(0, 60)).join(' | ')}`,
    );
  }
  const unsandboxed = chrome.filter((entry) =>
    flags(entry.command).some(
      (flag) => flag === '--no-sandbox' || flag === '--disable-setuid-sandbox',
    ),
  );
  if (unsandboxed.length > 0) {
    fail(`Chrome runs without its sandbox: ${unsandboxed[0].command}`);
  }
  for (const renderer of renderers) {
    // Seccomp 2: a filter of Chrome's own on top of the container's. More
    // than one NSpid entry: the renderer lives in a PID namespace of its own.
    const seccomp = field(renderer.status, 'Seccomp');
    const depth = field(renderer.status, 'NSpid').split(/\s+/).length;
    const noNewPrivileges = field(renderer.status, 'NoNewPrivs');
    if (seccomp !== '2' || depth < 2 || noNewPrivileges !== '1') {
      fail(
        `renderer ${renderer.pid} is not sandboxed: Seccomp=${seccomp} NSpid depth=${depth} NoNewPrivs=${noNewPrivileges}`,
      );
    }
  }
  report = {
    uid: process.getuid(),
    read_only_root: true,
    chrome: pinned,
    applied: {
      screen: opened.applied.screen,
      gl: opened.applied.gl,
      inject: opened.applied.inject,
      hardware_concurrency: opened.applied.hardware_concurrency,
    },
    renderers: renderers.map((renderer) => ({
      seccomp: field(renderer.status, 'Seccomp'),
      pid_namespaces: field(renderer.status, 'NSpid').split(/\s+/).length,
      no_new_privs: field(renderer.status, 'NoNewPrivs'),
      cap_eff: field(renderer.status, 'CapEff'),
    })),
  };
} finally {
  await call('DELETE', `/v1/contexts/${opened.context_id}`);
}

let left = chromeProcesses();
for (let attempt = 0; attempt < 25 && left.length > 0; attempt += 1) {
  await sleep(200);
  left = chromeProcesses();
}
if (left.length > 0) {
  fail(`${left.length} Chrome processes outlived the context`);
}
process.stdout.write(`${JSON.stringify(report)}\n`);
