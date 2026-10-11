#!/usr/bin/env node
// Checks the license of every package in the dependency tree of a pnpm
// project against the license-policy.json next to it. The project is the
// sidecar unless a directory is named:
//
//   node scripts/check-npm-licenses.mjs [directory]
//
// Needs installed dependencies (`just setup-node`).
//
// A package passes when its SPDX expression is satisfied by the allowlist, or
// when an exception names exactly that package with exactly that license. An
// exception that no longer matches anything is an error, so the policy never
// carries dead entries.
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

const project = process.argv[2] ?? 'sidecar';
const policyFile = `${project}/license-policy.json`;
const policy = JSON.parse(readFileSync(policyFile, 'utf8'));
const allowed = new Set(policy.allow);

/** True when the SPDX expression is satisfied using allowed licenses only. */
function satisfied(expression) {
  const tokens = expression.match(/\(|\)|[^\s()]+/g) ?? [];
  let position = 0;

  const atom = () => {
    if (tokens[position] === '(') {
      position += 1;
      const value = either();
      if (tokens[position] !== ')') throw new Error('unbalanced parentheses');
      position += 1;
      return value;
    }
    let id = tokens[position];
    if (id === undefined || id === ')' || id === 'AND' || id === 'OR') {
      throw new Error('license identifier expected');
    }
    position += 1;
    if (tokens[position] === 'WITH') {
      id += ` WITH ${tokens[position + 1]}`;
      position += 2;
    }
    return allowed.has(id);
  };
  const both = () => {
    let value = atom();
    while (tokens[position] === 'AND') {
      position += 1;
      const right = atom();
      value = value && right;
    }
    return value;
  };
  const either = () => {
    let value = both();
    while (tokens[position] === 'OR') {
      position += 1;
      const right = both();
      value = value || right;
    }
    return value;
  };

  try {
    const value = either();
    return value && position === tokens.length;
  } catch {
    // Not an SPDX expression ("SEE LICENSE IN ...", "Unknown", ...).
    return false;
  }
}

let listing;
try {
  listing = JSON.parse(
    execFileSync('pnpm', ['licenses', 'list', '--json'], {
      cwd: project,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    }),
  );
} catch (error) {
  console.error(`error: cannot list the licenses of ${project}: ${error.message}`);
  console.error('       Install the dependencies first: just setup-node');
  process.exit(2);
}

const packages = Object.entries(listing).flatMap(([license, entries]) =>
  entries.map((entry) => ({ name: entry.name, versions: entry.versions ?? [], license })),
);
if (packages.length === 0) {
  console.error(`error: no installed packages found in ${project}. Run: just setup-node`);
  process.exit(2);
}

const usedExceptions = new Set();
const violations = [];
const counts = new Map();

for (const { name, versions, license } of packages) {
  counts.set(license, (counts.get(license) ?? 0) + 1);
  if (satisfied(license)) continue;

  const exception = policy.exceptions.find(
    (candidate) => candidate.package === name && candidate.license === license,
  );
  if (exception) {
    usedExceptions.add(exception);
    continue;
  }
  violations.push(`${name}@${versions.join(',')}: ${license}`);
}

console.log(`Sidecar licenses (${packages.length} packages)`);
for (const [license, count] of [...counts].sort((a, b) => b[1] - a[1])) {
  console.log(`  ${String(count).padStart(4)}  ${license}${satisfied(license) ? '' : '  (not on the allowlist)'}`);
}

for (const exception of usedExceptions) {
  console.log(
    `warning: exception in force for ${exception.package} (${exception.license}), see ${exception.adr}`,
  );
}

const stale = policy.exceptions.filter((exception) => !usedExceptions.has(exception));
for (const exception of stale) {
  console.error(
    `error: stale exception for ${exception.package} (${exception.license}): nothing matches it. Remove it from ${policyFile}.`,
  );
}

if (violations.length > 0) {
  console.error(`error: packages with a license outside ${policyFile}:`);
  for (const violation of violations) console.error(`  ${violation}`);
}

if (violations.length > 0 || stale.length > 0) process.exit(1);
console.log(`ok: every package license of ${project} is allowed`);
