// Runs inside the browser-firefox container; tests/e2e/smoke.sh pipes it to
// node. It checks what spec §19 and §22 ask of the container while a real
// context is open: a non-root user, a read-only root filesystem, the pinned
// Camoufox build, a persona that claims Windows with nothing injected into
// its pages, Firefox's own sandbox around the content processes, and the
// sound server the browser was given.
//
// The context gets a proxy address nobody listens on and is never navigated:
// the check needs a running browser, not a page.
import { execFileSync } from 'node:child_process';
import { randomBytes, randomUUID } from 'node:crypto';
import { readFileSync, readdirSync, writeFileSync } from 'node:fs';

const origin = `http://127.0.0.1:${process.env.SIDECAR_PORT ?? '8090'}`;
const token = process.env.SIDECAR_TOKEN ?? '';
const camoufoxBinary = `${process.env.SIDECAR_CAMOUFOX_DIR ?? '/opt/camoufox'}/camoufox-bin`;

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
    // A first start writes the persona's font cache; the engine allows 90 s.
    signal: AbortSignal.timeout(120_000),
  });
  const text = await response.text();
  if (!response.ok) {
    fail(`${method} ${path} answered ${response.status}: ${text}`);
  }
  return text === '' ? null : JSON.parse(text);
}

/** Every process of the container as `{ pid, command, status }`. */
function processes() {
  const found = [];
  for (const entry of readdirSync('/proc')) {
    if (!/^\d+$/.test(entry)) {
      continue;
    }
    try {
      const command = readFileSync(`/proc/${entry}/cmdline`, 'utf8').replaceAll('\0', ' ').trim();
      const status = readFileSync(`/proc/${entry}/status`, 'utf8');
      found.push({ pid: Number(entry), command, status });
    } catch {
      // The process ended between the listing and the read.
    }
  }
  return found;
}

/** The Camoufox processes: the browser and the processes it starts for content. */
const camoufoxProcesses = () =>
  processes().filter((entry) => entry.command.startsWith(camoufoxBinary));

const flags = (command) => command.split(/\s+/);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const field = (status, name) =>
  (new RegExp(`^${name}:\\s*(.*)$`, 'm').exec(status)?.[1] ?? '').trim();

/** One variable of a process's environment, or undefined. */
function environmentOf(pid, name) {
  return readFileSync(`/proc/${pid}/environ`, 'utf8')
    .split('\0')
    .find((entry) => entry.startsWith(`${name}=`))
    ?.slice(name.length + 1);
}

/**
 * The sound server the browser was started with, when the sidecar runs one
 * (`SIDECAR_AUDIO=pulse`, the image's default): a PulseAudio process, its
 * address in the browser's `PULSE_SERVER`, and the devices it names when
 * asked there. Without an output a page finds no media device, which is
 * what a container looks like.
 */
function soundServer(browserPid) {
  if (process.env.SIDECAR_AUDIO !== 'pulse') {
    return { server: 'none' };
  }
  if (!processes().some((entry) => flags(entry.command)[0] === 'pulseaudio')) {
    fail('SIDECAR_AUDIO is pulse and no PulseAudio process is running');
  }
  const address = environmentOf(browserPid, 'PULSE_SERVER');
  if (address === undefined) {
    fail('SIDECAR_AUDIO is pulse and the browser was started without PULSE_SERVER');
  }
  // One line per device: its index, its name, and more, separated by tabs.
  const devices = (kind) => {
    let listed;
    try {
      listed = execFileSync('pactl', ['--server', address, 'list', 'short', kind], {
        encoding: 'utf8',
        stdio: ['ignore', 'pipe', 'ignore'],
        timeout: 10_000,
      });
    } catch (error) {
      return fail(`the sound server at ${address} does not answer: ${error.message}`);
    }
    return listed
      .split(/\n/)
      .filter((line) => line !== '')
      .map((line) => line.split(/\t/)[1]);
  };
  const outputs = devices('sinks');
  const inputs = devices('sources');
  // The two devices the server is configured with (engine-core, audio.ts).
  // The output's monitor is an input too, and not the one a page should
  // be offered as the microphone.
  if (!outputs.includes('speakers')) {
    fail(`the sound server at ${address} has no output named speakers: ${outputs.join(', ')}`);
  }
  if (!inputs.includes('microphone')) {
    fail(`the sound server at ${address} has no input named microphone: ${inputs.join(', ')}`);
  }
  return { server: 'pulse', outputs, inputs };
}

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

const pinned = readFileSync('/etc/kage/browser-version', 'utf8').trim();
const build = readFileSync('/etc/kage/camoufox-version', 'utf8').trim();
const health = await call('GET', '/v1/health');
if (health.browser_version !== pinned) {
  fail(`health reports Firefox ${health.browser_version}, the image records ${pinned}`);
}

// The persona claims Windows, in a Linux container: Camoufox makes such a
// claim inside the browser, so the context must report it as applied with
// nothing injected.
const personaId = randomUUID();
const generated = await call('POST', '/v1/personas/generate', {
  seed: randomBytes(32).toString('hex'),
  claimed_os: 'windows',
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
  if (opened.applied.claimed_os !== 'windows' || opened.applied.inject !== 'none') {
    fail(
      `the context applied claimed_os ${opened.applied.claimed_os} with inject ${opened.applied.inject}; expected windows and none`,
    );
  }

  // The browser runs once the context is open. Its content processes start
  // a moment later and put themselves into Firefox's sandbox as they come
  // up: a seccomp filter of their own on top of the container's. (Docker's
  // default profile refuses the user namespaces Firefox would add to that.)
  // A kernel older than 5.9 does not count the filters of a process; there
  // only the processes themselves are looked for.
  const filters = (entry) => field(entry.status, 'Seccomp_filters');
  const sandboxed = (entry, parent) => Number(filters(entry)) > Number(filters(parent));
  let camoufox = [];
  let browser;
  let content = [];
  for (let attempt = 0; attempt < 75; attempt += 1) {
    await sleep(200);
    camoufox = camoufoxProcesses();
    browser = camoufox.find((entry) => !flags(entry.command).includes('-contentproc'));
    content = camoufox.filter((entry) => flags(entry.command).includes('-isForBrowser'));
    if (
      browser !== undefined &&
      content.length > 0 &&
      (filters(browser) === '' || content.every((entry) => sandboxed(entry, browser)))
    ) {
      break;
    }
  }
  if (browser === undefined || content.length === 0) {
    fail(
      `expected a browser and a content process, found ${camoufox.length} Camoufox processes: ${camoufox.map((entry) => entry.command.slice(0, 60)).join(' | ')}`,
    );
  }
  const counted = filters(browser) !== '';
  if (counted && !content.every((entry) => sandboxed(entry, browser))) {
    fail(
      `content processes are outside Firefox's sandbox: the browser has ${filters(browser)} seccomp filter(s), its content processes have ${content.map(filters).join(', ')}`,
    );
  }
  // The engine gives the browser an environment of its own.
  if (environmentOf(browser.pid, 'SIDECAR_TOKEN') !== undefined) {
    fail('the shared secret of the sidecar is in the environment of the browser');
  }
  report = {
    uid: process.getuid(),
    read_only_root: true,
    camoufox: build,
    applied: {
      claimed_os: opened.applied.claimed_os,
      screen: opened.applied.screen,
      gl: opened.applied.gl,
      inject: opened.applied.inject,
      hardware_concurrency: opened.applied.hardware_concurrency,
    },
    processes: camoufox.length,
    content_processes: content.map((entry) => ({
      name: field(entry.status, 'Name'),
      no_new_privs: field(entry.status, 'NoNewPrivs'),
      cap_eff: field(entry.status, 'CapEff'),
      // Filters of Firefox's own, on top of those every process here has.
      seccomp_filters_of_firefox: counted
        ? Number(filters(entry)) - Number(filters(browser))
        : null,
    })),
    audio: soundServer(browser.pid),
  };
} finally {
  await call('DELETE', `/v1/contexts/${opened.context_id}`);
}

let left = camoufoxProcesses();
for (let attempt = 0; attempt < 25 && left.length > 0; attempt += 1) {
  await sleep(200);
  left = camoufoxProcesses();
}
if (left.length > 0) {
  fail(`${left.length} Camoufox processes outlived the context`);
}
process.stdout.write(`${JSON.stringify(report)}\n`);
