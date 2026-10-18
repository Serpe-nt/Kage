#!/usr/bin/env node
// The preference audit of the pinned Camoufox build (docs/runbooks/
// engine-firefox.md, "Moving to a new Camoufox build"): every preference
// whose value twelve seconds after start differs between a persona that
// Kage's Firefox engine opens and a stock Mozilla Firefox of the same
// version that nothing drives, with where the persona's value comes from
// and what the previous audit said about it. A row the previous audit does
// not have is marked `new: review`; that is the work of a bump.
//
//   node scripts/audit-camoufox-prefs.mjs --stock <firefox dir> --out <tsv>
//       [--camoufox <dir>] [--previous <tsv>] [--os windows|macos]
//
// --stock     Mozilla's Linux build of the same Firefox version, unpacked
//             (the directory that holds `firefox`), checked against
//             Mozilla's SHA256SUMS. Nothing of it is installed.
// --camoufox  the pinned build (default SIDECAR_CAMOUFOX_DIR, else
//             /opt/camoufox): `just setup-camoufox`.
// --previous  the last audit (default docs/evidence/camoufox-156-prefs-audit.tsv).
//
// Needs the sidecar built (`corepack pnpm -r build` in sidecar/), Xvfb, and
// a user that is not root. Both browsers run from copies of their build
// directories with a preference dump appended to the autoconfig file
// (scripts/camoufox-prefdump.cfg.js); the builds themselves are not
// touched. Neither browser reaches the network: the stock one is given a
// proxy that is not there, the persona the engine's test proxy, which
// relays nothing but its one test page.
import { spawn, spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const { values: args } = parseArgs({
  options: {
    stock: { type: 'string' },
    camoufox: { type: 'string', default: process.env.SIDECAR_CAMOUFOX_DIR ?? '/opt/camoufox' },
    previous: {
      type: 'string',
      default: path.join(repo, 'docs/evidence/camoufox-156-prefs-audit.tsv'),
    },
    out: { type: 'string' },
    os: { type: 'string', default: 'windows' },
  },
});
if (args.stock === undefined || args.out === undefined) {
  console.error('usage: audit-camoufox-prefs.mjs --stock <firefox dir> --out <tsv> [--camoufox <dir>] [--previous <tsv>] [--os windows|macos]');
  process.exit(2);
}
if (process.getuid?.() === 0) {
  console.error('run it as a user that is not root: Firefox does not run its sandbox as root');
  process.exit(2);
}

const DUMP = readFileSync(path.join(repo, 'scripts/camoufox-prefdump.cfg.js'), 'utf8');
/** Twelve seconds for the dump, and two to spare. */
const HOLD_MS = 14_000;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const root = mkdtempSync(path.join(tmpdir(), 'kage-prefs-audit-'));

/**
 * A copy of a build directory with its own autoconfig file: hard links
 * where the work directory is on the same filesystem, a full copy
 * otherwise. (Links to the files would not do: Firefox finds its
 * directory through its executable's real path.)
 */
function measurementCopy(given, name, cfgFile, extra) {
  const from = realpathSync(given);
  const to = path.join(root, name);
  if (spawnSync('cp', ['-al', from, to], { stdio: 'ignore' }).status !== 0) {
    rmSync(to, { recursive: true, force: true });
    console.error(`copying ${from} (another filesystem than ${root})`);
    if (spawnSync('cp', ['-a', from, to], { stdio: 'inherit' }).status !== 0) {
      throw new Error(`cannot copy ${from}`);
    }
  }
  // The directories are the copy's own, the files may be the build's: only
  // the directories are made writable, so that a file can be replaced.
  spawnSync('find', [to, '-type', 'd', '-exec', 'chmod', 'u+w', '{}', '+'], { stdio: 'inherit' });
  const cfg = path.join(to, cfgFile);
  const original = existsSync(cfg) ? readFileSync(cfg, 'utf8') : '// Kage measurement only\n';
  rmSync(cfg, { force: true });
  writeFileSync(cfg, `${original}\n${DUMP}`);
  extra?.(to);
  return to;
}

/** One Xvfb display, for one browser. */
async function display() {
  const number = 80 + Math.floor(Math.random() * 15);
  const xvfb = spawn('Xvfb', [`:${String(number)}`, '-screen', '0', '1920x1080x24', '-nolisten', 'tcp'], {
    stdio: 'ignore',
  });
  await sleep(800);
  return { name: `:${String(number)}`, stop: () => xvfb.kill('SIGTERM') };
}

function readDump(home) {
  const file = path.join(home, 'kage-prefs.json');
  if (!existsSync(file)) throw new Error(`no preference dump at ${file}: the browser did not run twelve seconds`);
  return new Map(JSON.parse(readFileSync(file, 'utf8')).map((pref) => [pref.name, pref]));
}

/** Stock Firefox, started with an address on its command line and left alone. */
async function stockPrefs() {
  const dir = measurementCopy(args.stock, 'stock', 'kage-dump.cfg', (copy) => {
    const autoconfig = path.join(copy, 'defaults/pref/autoconfig.js');
    rmSync(autoconfig, { force: true });
    writeFileSync(
      autoconfig,
      [
        'pref("general.config.filename", "kage-dump.cfg");',
        'pref("general.config.obscure_value", 0);',
        'pref("general.config.sandbox_enabled", false);',
        '',
      ].join('\n'),
    );
  });
  const profile = path.join(root, 'stock-profile');
  const home = path.join(root, 'stock-home');
  mkdirSync(profile, { recursive: true });
  mkdirSync(home, { recursive: true });
  // Only what keeps a first start from opening its own pages, and a proxy
  // that is not there for everything the browser would fetch by itself.
  writeFileSync(
    path.join(profile, 'user.js'),
    [
      'user_pref("browser.aboutwelcome.enabled", false);',
      'user_pref("browser.shell.checkDefaultBrowser", false);',
      'user_pref("datareporting.policy.dataSubmissionPolicyBypassNotification", true);',
      'user_pref("browser.startup.homepage_override.mstone", "ignore");',
      'user_pref("startup.homepage_welcome_url", "");',
      'user_pref("startup.homepage_welcome_url.additional", "");',
      'user_pref("network.proxy.type", 1);',
      'user_pref("network.proxy.http", "127.0.0.1");',
      'user_pref("network.proxy.http_port", 9);',
      'user_pref("network.proxy.ssl", "127.0.0.1");',
      'user_pref("network.proxy.ssl_port", 9);',
      'user_pref("network.proxy.share_proxy_settings", true);',
      '',
    ].join('\n'),
  );
  const screen = await display();
  const browser = spawn(
    path.join(dir, 'firefox'),
    ['--profile', profile, '--no-remote', '--new-instance', 'about:blank'],
    { env: { PATH: process.env.PATH, HOME: home, DISPLAY: screen.name }, stdio: 'ignore' },
  );
  await sleep(HOLD_MS);
  browser.kill('SIGTERM');
  await sleep(1000);
  screen.stop();
  return readDump(home);
}

/** A persona of Kage's engine on a copy of the pinned build. */
async function personaPrefs() {
  const build = measurementCopy(path.resolve(args.camoufox), 'camoufox', 'camoufox.cfg');
  process.env.SIDECAR_CAMOUFOX_DIR = build;
  const module = (name) =>
    import(pathToFileURL(path.join(repo, 'sidecar/packages', name)).href);
  const core = await module('engine-core/dist/index.js');
  const { startTestProxy } = await module('engine-core/dist/testing/test-proxy.js');
  const firefox = await module('engine-firefox/dist/index.js');

  const page = http.createServer((_, response) => {
    response.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    response.end('<!doctype html><title>audit</title><p>audit</p>');
  });
  await new Promise((resolve) => page.listen(0, '127.0.0.1', resolve));
  const proxy = await startTestProxy({ httpPort: page.address().port });
  const personas = path.join(root, 'personas');
  const engine = await firefox.createEngine({
    personasDir: personas,
    allowCrossOsClaims: false,
    gl: 'llvmpipe',
    selftestPages: [],
    log: core.silentLog,
  });
  const languages = ['de', 'en-US', 'en'];
  const { fingerprint } = await engine.generatePersona({
    seed: '5f1c0e9a7b3d2468ace013579bdf02468ace13579bdf0246813579bdf02468ac',
    claimed_os: args.os,
    languages,
  });
  const request = {
    persona_id: '0199c1f2-6b7a-7c3e-9d41-2f8a5b6c7d8e',
    fingerprint,
    proxy: { server: proxy.server, username: proxy.username, password: proxy.password },
    locale: 'de-DE',
    languages,
    timezone: 'Europe/Berlin',
    geolocation: null,
    screen: fingerprint.native.screen,
    profile_dir: 'personas/audit',
  };
  const host = await firefox.detectHost(build);
  const plan = firefox.launchPlan(fingerprint, request, host, path.join(root, 'page.pdf'));
  const context = await engine.openContext(request);
  const opened = Date.now();
  await context.navigate(
    {
      url: 'http://audit.test/',
      wait: { until: 'load' },
      capture: { dom: false, raw: false, xhr: null, console: false, screenshot: null, pdf: null },
    },
    { artifacts: new core.ArtifactWriter(path.join(root, 'artifacts')), defaultArtifactPrefix: 'audit' },
  );
  await sleep(Math.max(0, HOLD_MS - (Date.now() - opened)));
  await context.close();
  await engine.close();
  await proxy.stop();
  page.close();
  const dumped = readDump(firefox.profilePaths(path.join(personas, request.profile_dir)).home);
  return { prefs: dumped, plan: new Set(Object.keys(plan.preferences)), cfg: readFileSync(path.join(build, 'camoufox.cfg'), 'utf8') };
}

/** Values that name this profile or this run, not the browser. */
const VOLATILE =
  /impressionId|lastColdStartupCheck|cachedClientID|cachedUsageProfile|ProfileGroupID|profileId|storeID|webextensions\.uuids|nextupdatetime|lastupdatetime|uiCustomization|lastFetch|region\.update|migrationsApplied|user_id|lastAppBuildId|lastAppVersion|iniFile|previousBuildID|placeholderName|print\.printer_Mozilla|network\.dns\.localDomains|general\.config\.sandbox|signon\.storage\.rust|doh-rollout|distribution\.|backgroundtasks|toolbarbuttons\.introduced|releaseNotesURL|quotaManager\.backgroundTask|contextId|first_run|firstUpdateComplete|hasUserInteractedWithETPSettings|onnxNative|policies\.|^network\.proxy\.(type|http|http_port|ssl|ssl_port|share_proxy_settings|no_proxies_on)$/;

function previousAudit() {
  if (!existsSync(args.previous)) return new Map();
  const [header, ...lines] = readFileSync(args.previous, 'utf8').trimEnd().split('\n');
  const columns = header.split('\t');
  return new Map(
    lines.map((line) => {
      const cells = line.split('\t');
      return [cells[0], Object.fromEntries(columns.map((column, at) => [column, cells[at] ?? '']))];
    }),
  );
}

try {
  const stock = await stockPrefs();
  const persona = await personaPrefs();
  const previous = previousAudit();
  const inCfg = (name) => persona.cfg.includes(`"${name}"`);
  const rows = [];
  for (const name of [...new Set([...stock.keys(), ...persona.prefs.keys()])].sort()) {
    if (VOLATILE.test(name)) continue;
    const s = stock.get(name);
    const k = persona.prefs.get(name);
    const sv = s === undefined ? '(absent)' : JSON.stringify(s.value);
    const kv = k === undefined ? '(absent)' : JSON.stringify(k.value);
    if (sv === kv) continue;
    const source =
      k === undefined
        ? 'absent in Camoufox'
        : s === undefined
          ? 'only in Camoufox'
          : persona.plan.has(name)
            ? 'Kage launch plan'
            : k.user
              ? 'user value'
              : inCfg(name)
                ? 'camoufox.cfg'
                : 'build default (not in camoufox.cfg)';
    const known = previous.get(name);
    rows.push([
      name,
      sv,
      kv,
      source,
      known?.['who can see it'] ?? 'new: review',
      known?.['what it does'] ?? '',
      known?.action ?? 'new: review',
    ]);
  }
  const header = ['pref', 'stock', 'Kage persona', 'where it comes from', 'who can see it', 'what it does', 'action'];
  writeFileSync(args.out, `${[header, ...rows].map((row) => row.join('\t')).join('\n')}\n`);
  const gone = [...previous.keys()].filter((name) => !rows.some((row) => row[0] === name));
  console.log(
    JSON.stringify(
      {
        rows: rows.length,
        new: rows.filter((row) => row[6] === 'new: review').map((row) => row[0]),
        no_longer_different: gone,
      },
      null,
      1,
    ),
  );
} finally {
  rmSync(root, { recursive: true, force: true });
}
process.exit(0);
