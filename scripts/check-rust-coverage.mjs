#!/usr/bin/env node
// Enforces per-crate line coverage on an llvm-cov JSON summary.
//
//   node scripts/check-rust-coverage.mjs --min-lines 80 --gate kage-core,kage-proxy <summary.json>
//
// Every crate under crates/ is reported. Crates named in --gate must reach
// --min-lines percent, and must have coverage data at all: a gated crate that
// reports no lines is a failure, not a pass.
import { readdirSync, readFileSync } from 'node:fs';
import { parseArgs } from 'node:util';

const { values, positionals } = parseArgs({
  options: {
    'min-lines': { type: 'string' },
    gate: { type: 'string' },
  },
  allowPositionals: true,
});

const minLines = Number(values['min-lines']);
const gated = new Set((values.gate ?? '').split(',').filter(Boolean));
const [summaryPath] = positionals;

if (!summaryPath || !Number.isFinite(minLines) || minLines < 0 || minLines > 100) {
  console.error(
    'usage: check-rust-coverage.mjs --min-lines <0-100> --gate <crate,crate> <summary.json>',
  );
  process.exit(2);
}

const crates = readdirSync('crates', { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name)
  .sort();

for (const name of gated) {
  if (!crates.includes(name)) {
    console.error(`error: gated crate ${name} does not exist under crates/`);
    process.exit(2);
  }
}

const summary = JSON.parse(readFileSync(summaryPath, 'utf8'));
const totals = new Map(crates.map((name) => [name, { count: 0, covered: 0 }]));

for (const file of summary.data.flatMap((entry) => entry.files)) {
  const match = /(?:^|\/)crates\/([^/]+)\/src\//.exec(file.filename.replaceAll('\\', '/'));
  const total = match && totals.get(match[1]);
  if (total) {
    total.count += file.summary.lines.count;
    total.covered += file.summary.lines.covered;
  }
}

let failed = false;
console.log(`Rust line coverage by crate (gate: ${minLines} % on gated crates)`);
for (const [name, { count, covered }] of totals) {
  const isGated = gated.has(name);
  const percent = count === 0 ? null : (covered / count) * 100;
  let verdict = 'reported';
  if (isGated) {
    const ok = percent !== null && percent >= minLines;
    verdict = ok ? 'ok' : 'FAIL';
    failed ||= !ok;
  }
  const shown = percent === null ? 'no data' : `${percent.toFixed(2)} %`;
  console.log(
    `  ${name.padEnd(14)} ${shown.padStart(9)}  ${String(covered).padStart(5)}/${String(count).padEnd(5)} lines  ${isGated ? 'gated   ' : 'ungated '} ${verdict}`,
  );
}

if (failed) {
  console.error(
    `error: a gated crate is below ${minLines} % line coverage. Add tests for the new code.`,
  );
  process.exit(1);
}
