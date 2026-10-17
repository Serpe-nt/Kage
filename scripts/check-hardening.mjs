#!/usr/bin/env node
// The container hardening checklist (spec §19, §22), checked on every
// Dockerfile, every compose file and, with --images, every image Kage
// builds. Run by `just hardening` (in `just ci`):
//
//   node scripts/check-hardening.mjs            Dockerfiles and compose files
//   node scripts/check-hardening.mjs --images   also the built images (kage:dev
//                                               and both sidecars, or what
//                                               KAGE_IMAGE and the two
//                                               KAGE_BROWSER_*_IMAGE name)
//
// A compose file is read as Docker Compose resolves it (`docker compose
// config`), with its anchors and overlays applied, for every file and
// profile Kage ships. What a service may do that the checklist forbids is
// listed below with its reason; nothing else may.

import { execFileSync, spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const args = process.argv.slice(2);
const root = resolve(args.find((arg) => !arg.startsWith('--')) ?? '.');
const images = args.includes('--images');
const problems = [];
const note = (where, what) => problems.push(`${where}: ${what}`);

/** What a service may do that the checklist does not allow, and why. */
const EXCEPTIONS = {
  postgres: {
    writable: 'compose injects a secret taken from the environment only into a writable container; the data lives on a volume',
    capAdd: ['CHOWN', 'DAC_OVERRIDE', 'FOWNER', 'SETGID', 'SETUID'],
    capAddReason: "the image's entrypoint fixes the ownership of the data directory as root, then drops to postgres",
  },
  echo: {
    published: "the fingerprint echo must be reached by the proxies' exits (docs/runbooks/fingerprint-echo.md)",
  },
};
/** Services that may run without a Docker healthcheck, and why. */
const NO_HEALTHCHECK = {
  migrate: 'it runs once and exits; kage waits for it to complete',
  echo: "the image's own check probes the API server, which this is not",
  'otel-collector': 'the image is one binary without a shell or a health command; Prometheus watches it',
};

// --- Dockerfiles -------------------------------------------------------------

for (const file of ['deploy/docker/kage.Dockerfile', 'deploy/docker/sidecar.Dockerfile']) {
  const text = readFileSync(join(root, file), 'utf8');
  const lines = text.split('\n');
  const stages = new Set();
  const imageArgs = new Map();
  for (const line of lines) {
    const arg = /^ARG ([A-Z_]*IMAGE)=(\S+)/.exec(line);
    if (arg) {
      imageArgs.set(arg[1], arg[2]);
      if (!arg[2].includes('@sha256:')) note(file, `ARG ${arg[1]} is not pinned by digest`);
    }
    const from = /^FROM (\S+)(?: AS (\S+))?/i.exec(line);
    if (from) {
      const base = from[1];
      const named = /^\$\{(\w+)\}$/.exec(base);
      if (named) {
        if (!imageArgs.has(named[1])) note(file, `FROM ${base} names no ARG with a pinned default`);
      } else if (!stages.has(base) && !base.includes('@sha256:')) {
        note(file, `FROM ${base} is not pinned by digest`);
      }
      if (from[2]) stages.add(from[2]);
    }
    if (/^ADD\s+https?:/i.test(line)) note(file, 'ADD fetches a URL: download with a checksum instead');
  }
  const runtime = text.slice(text.search(/^FROM \S+ AS runtime/m));
  if (runtime.length === text.length) note(file, 'has no stage named runtime');
  const user = /^USER (\d+):(\d+)\s*$/m.exec(runtime);
  if (!user) {
    note(file, 'the runtime stage sets no numeric USER uid:gid');
  } else if (user[1] === '0' || user[2] === '0') {
    note(file, 'the runtime stage runs as root');
  }
  if (!/^HEALTHCHECK /m.test(runtime)) note(file, 'the runtime stage has no HEALTHCHECK');
  const downloads = (text.match(/curl [^\n]*--output/g) ?? []).length;
  const checked = (text.match(/sha256sum --check/g) ?? []).length;
  if (checked < downloads) note(file, `${downloads} downloads and only ${checked} checksum checks`);
}

// --- Compose files -----------------------------------------------------------

const PLACEHOLDERS = {
  KAGE_POSTGRES_PASSWORD: 'placeholder',
  KAGE_SIDECAR_TOKEN: 'placeholder',
  KAGE_MASTER_KEY: 'placeholder',
  KAGE_GRAFANA_ADMIN_PASSWORD: 'placeholder',
  KAGE_E2E_POLICIES: '/tmp',
};
const STACKS = [
  { files: ['deploy/compose.yaml'], profiles: ['echo'] },
  { files: ['deploy/compose.yaml', 'deploy/compose.observability.yaml'], profiles: ['observability'] },
  { files: ['deploy/compose.yaml', 'deploy/compose.e2e.yaml'], profiles: [] },
];

function resolved(stack) {
  const command = ['compose', '--project-directory', join(root, 'deploy')];
  for (const file of stack.files) command.push('--file', join(root, file));
  for (const profile of stack.profiles) command.push('--profile', profile);
  command.push('config', '--format', 'json');
  const output = execFileSync('docker', command, {
    env: { ...process.env, ...PLACEHOLDERS },
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  return JSON.parse(output);
}

const seen = new Set();
for (const stack of STACKS) {
  const where = stack.files.join(' + ');
  let config;
  try {
    config = resolved(stack);
  } catch (error) {
    note(where, `docker compose config failed: ${String(error.stderr ?? error.message).trim()}`);
    continue;
  }
  // An image a service of the stack builds from this repository.
  const builtHere = new Set(
    Object.values(config.services ?? {})
      .filter((service) => service.build !== undefined)
      .map((service) => service.image),
  );
  for (const [name, service] of Object.entries(config.services ?? {})) {
    const at = `${where}: service ${name}`;
    seen.add(name);
    const exception = EXCEPTIONS[name] ?? {};
    const image = service.image ?? '';
    if (!builtHere.has(image) && !image.includes('@sha256:')) {
      note(at, `image ${image} is neither built here nor pinned by digest`);
    }
    if (service.read_only !== true && exception.writable === undefined) note(at, 'is not read_only');
    const dropped = service.cap_drop ?? [];
    if (!dropped.includes('ALL')) note(at, 'does not drop every capability');
    for (const added of service.cap_add ?? []) {
      if (!(exception.capAdd ?? []).includes(added)) note(at, `adds the capability ${added}`);
    }
    const options = service.security_opt ?? [];
    if (!options.includes('no-new-privileges:true')) note(at, 'allows new privileges');
    for (const option of options) {
      if (/unconfined/.test(option)) note(at, `runs ${option}`);
    }
    if (service.privileged) note(at, 'is privileged');
    for (const key of ['network_mode', 'pid', 'ipc', 'userns_mode']) {
      if (service[key] === 'host') note(at, `shares the host's ${key}`);
    }
    if ((service.devices ?? []).length > 0) note(at, 'is given devices of the host');
    if (/^(0|root)(:|$)/.test(String(service.user ?? ''))) note(at, 'runs as root');
    if (!service.mem_limit && !service.deploy?.resources?.limits?.memory) note(at, 'has no memory limit');
    if (!service.cpus && !service.deploy?.resources?.limits?.cpus) note(at, 'has no CPU limit');
    if (!service.pids_limit && !service.deploy?.resources?.limits?.pids) note(at, 'has no limit of processes');
    if (!service.restart) note(at, 'has no restart policy');
    const health = service.healthcheck;
    if (health === undefined) {
      note(at, 'has no healthcheck');
    } else if (health.disable === true && NO_HEALTHCHECK[name] === undefined) {
      note(at, 'disables its healthcheck without a reason in scripts/check-hardening.mjs');
    }
    for (const volume of service.volumes ?? []) {
      const source = String(volume.source ?? '');
      if (/docker\.sock/.test(source) || /docker\.sock/.test(String(volume.target ?? ''))) {
        note(at, 'mounts the Docker socket');
      }
      if (volume.type === 'bind' && volume.read_only !== true) note(at, `binds ${volume.target} writable`);
    }
    for (const port of service.ports ?? []) {
      if (port.host_ip !== '127.0.0.1' && exception.published === undefined) {
        note(at, `publishes ${port.target} beyond loopback`);
      }
    }
  }
}
for (const name of [...Object.keys(EXCEPTIONS), ...Object.keys(NO_HEALTHCHECK)]) {
  if (!seen.has(name)) note('scripts/check-hardening.mjs', `an exception names ${name}, which no compose file has`);
}

// --- Images ------------------------------------------------------------------

if (images) {
  const built = [
    process.env.KAGE_IMAGE ?? 'kage:dev',
    process.env.KAGE_BROWSER_CHROMIUM_IMAGE ?? 'kage-browser-chromium:dev',
    process.env.KAGE_BROWSER_FIREFOX_IMAGE ?? 'kage-browser-firefox:dev',
  ];
  for (const image of built) {
    let inspected;
    try {
      [inspected] = JSON.parse(execFileSync('docker', ['image', 'inspect', image], { encoding: 'utf8' }));
    } catch {
      note(image, 'is not built');
      continue;
    }
    const user = inspected.Config?.User ?? '';
    if (user === '' || /^(0|root)(:|$)/.test(user)) note(image, `runs as ${user || 'root'}`);
    if (!inspected.Config?.Healthcheck?.Test) note(image, 'has no healthcheck');
    // No file a process could gain privileges through. Kage's runtime is
    // distroless: it has no find, and no setuid file either.
    if (image !== built[0]) {
      // As root, so that no directory is out of its sight, in a container
      // without a network that is gone afterwards.
      const found = spawnSync(
        'docker',
        ['run', '--rm', '--network', 'none', '--user', '0', '--entrypoint', 'find', image, '/', '-xdev', '-perm', '/6000', '-type', 'f'],
        { encoding: 'utf8' },
      );
      const privileged = (found.stdout ?? '').trim();
      if (found.status !== 0) note(image, `could not be searched: ${(found.stderr ?? '').trim().slice(0, 300)}`);
      if (privileged !== '') note(image, `has setuid or setgid files: ${privileged.split('\n').join(', ')}`);
    }
  }
}

if (problems.length > 0) {
  for (const problem of problems) console.error(`hardening: ${problem}`);
  process.exit(1);
}
console.log(
  `hardening: 2 Dockerfiles, ${seen.size} services in ${STACKS.length} stacks${images ? ', 3 images' : ''}: every check holds`,
);
