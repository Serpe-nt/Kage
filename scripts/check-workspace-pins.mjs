#!/usr/bin/env node
// Every dependency pinned in `[workspace.dependencies]` is used by a crate
// of the workspace (docs/versions.md: a pin nobody uses is a version nobody
// compiles, tests or audits, and it reads as a dependency Kage has).
//
//   node scripts/check-workspace-pins.mjs [repository root]
//
// Exits 1 and names the pins no crate inherits with `workspace = true`.

import { readdirSync, readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const root = resolve(process.argv[2] ?? '.');

// Pins that are declared on purpose although no crate inherits them, each
// with its reason. An entry that a crate starts to use, or that is gone
// from Cargo.toml, fails the check too: the list holds no stale excuse.
const DECLARED_ONLY = new Map([
  // docs/versions.md: the version `scraper` is built against, recorded
  // beside it; kage-crawl and kage-extract reach it through `scraper`.
  ['html5ever', 'the version scraper is built against, recorded beside it'],
  // Pinned since M0 for the traces of spec §20; first used by M12.
  ['tracing-opentelemetry', 'pinned for the traces of spec §20'],
  ['opentelemetry', 'pinned for the traces of spec §20'],
  ['opentelemetry_sdk', 'pinned for the traces of spec §20'],
  ['opentelemetry-otlp', 'pinned for the traces of spec §20'],
]);

/** The keys of one table of a manifest: `name = ...` lines until the next table. */
function keysOf(manifest, table) {
  const keys = [];
  let inside = false;
  for (const line of manifest.split('\n')) {
    const header = /^\s*\[([^\]]+)\]\s*$/.exec(line);
    if (header) {
      inside = header[1] === table;
      continue;
    }
    const key = /^([A-Za-z0-9_-]+)\s*=/.exec(line);
    if (inside && key) {
      keys.push(key[1]);
    }
  }
  return keys;
}

/** The dependencies a crate inherits from the workspace, in any of its tables. */
function inheritedBy(manifest) {
  const used = new Set();
  // `name = { workspace = true, ... }` and `name.workspace = true`.
  const inline = /^([A-Za-z0-9_-]+)\s*=\s*\{[^}]*\bworkspace\s*=\s*true/gm;
  const dotted = /^([A-Za-z0-9_-]+)\.workspace\s*=\s*true/gm;
  for (const pattern of [inline, dotted]) {
    for (const match of manifest.matchAll(pattern)) {
      used.add(match[1]);
    }
  }
  return used;
}

const workspace = readFileSync(join(root, 'Cargo.toml'), 'utf8');
const pinned = keysOf(workspace, 'workspace.dependencies');
if (pinned.length === 0) {
  console.error('check-workspace-pins: no [workspace.dependencies] in Cargo.toml');
  process.exit(1);
}

const used = new Set();
const crates = readdirSync(join(root, 'crates'), { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name);
for (const name of crates) {
  for (const dependency of inheritedBy(readFileSync(join(root, 'crates', name, 'Cargo.toml'), 'utf8'))) {
    used.add(dependency);
  }
}

const unused = pinned.filter((name) => !used.has(name) && !DECLARED_ONLY.has(name));
const stale = [...DECLARED_ONLY.keys()].filter((name) => used.has(name) || !pinned.includes(name));
if (unused.length > 0) {
  console.error(
    `check-workspace-pins: ${unused.length} of ${pinned.length} pins in [workspace.dependencies] are used by no crate:`,
  );
  for (const name of unused) {
    console.error(`  ${name}`);
  }
  console.error('Remove the pin, or use it; docs/versions.md records either.');
}
if (stale.length > 0) {
  console.error('check-workspace-pins: listed as declared only, but used by a crate or no longer pinned:');
  for (const name of stale) {
    console.error(`  ${name}`);
  }
  console.error('Take the entry out of DECLARED_ONLY in scripts/check-workspace-pins.mjs.');
}
if (unused.length > 0 || stale.length > 0) {
  process.exit(1);
}
const declared = [...DECLARED_ONLY.keys()].join(', ');
console.log(
  `check-workspace-pins: ${pinned.length} pins; ${pinned.length - DECLARED_ONLY.size} used by a crate of ${crates.length}, ${DECLARED_ONLY.size} declared only (${declared})`,
);
