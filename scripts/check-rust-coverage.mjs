#!/usr/bin/env node
// Enforces per-crate line coverage on an llvm-cov JSON summary.
//
//   node scripts/check-rust-coverage.mjs --min-lines 80 --gate kage-core,kage-proxy [--badge <badge.json>] <summary.json>
//
// Every crate under crates/ is reported. Crates named in --gate must reach
// --min-lines percent, and must have coverage data at all: a gated crate that
// reports no lines is a failure, not a pass.
//
// With --badge, the line coverage of the whole workspace, counted the same
// way (the files under crates/*/src), is also written as a shields.io
// endpoint badge: CI publishes it for the README (docs/runbooks/repo.md,
// "Badges").
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';

/**
 * Lines and covered lines per crate of `crates`, from an llvm-cov summary:
 * the files under `crates/<name>/src/` only, so tests, benches and build
 * scripts count for nobody.
 */
export function countByCrate(summary, crates) {
  const totals = new Map(crates.map((name) => [name, { count: 0, covered: 0 }]));
  for (const file of summary.data.flatMap((entry) => entry.files)) {
    const match = /(?:^|\/)crates\/([^/]+)\/src\//.exec(file.filename.replaceAll('\\', '/'));
    const total = match && totals.get(match[1]);
    if (total) {
      total.count += file.summary.lines.count;
      total.covered += file.summary.lines.covered;
    }
  }
  return totals;
}

/** The colour of a coverage figure, in shields.io's names. */
export function colourOf(percent) {
  if (percent >= 90) return 'brightgreen';
  if (percent >= 80) return 'yellowgreen';
  if (percent >= 70) return 'yellow';
  return 'red';
}

/**
 * The shields.io endpoint badge of the workspace's line coverage: every
 * crate's lines together, one decimal, rounded down so that the badge never
 * says more than was measured.
 *
 * @throws {Error} when the summary counted no line at all.
 */
export function badgeOf(totals) {
  let count = 0;
  let covered = 0;
  for (const total of totals.values()) {
    count += total.count;
    covered += total.covered;
  }
  if (count === 0) {
    throw new Error('the summary has no line of any crate: no coverage figure to publish');
  }
  const percent = Math.floor((covered / count) * 1000) / 10;
  return {
    schemaVersion: 1,
    label: 'coverage',
    message: `${percent.toFixed(1)} %`,
    color: colourOf(percent),
  };
}

function main() {
  const { values, positionals } = parseArgs({
    options: {
      'min-lines': { type: 'string' },
      gate: { type: 'string' },
      badge: { type: 'string' },
    },
    allowPositionals: true,
  });

  const minLines = Number(values['min-lines']);
  const gated = new Set((values.gate ?? '').split(',').filter(Boolean));
  const [summaryPath] = positionals;

  if (!summaryPath || !Number.isFinite(minLines) || minLines < 0 || minLines > 100) {
    console.error(
      'usage: check-rust-coverage.mjs --min-lines <0-100> --gate <crate,crate> [--badge <badge.json>] <summary.json>',
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
  const totals = countByCrate(summary, crates);

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

  if (values.badge) {
    const badge = badgeOf(totals);
    writeFileSync(values.badge, `${JSON.stringify(badge)}\n`);
    console.log(`  workspace      ${badge.message.padStart(9)}  written to ${values.badge}`);
  }

  if (failed) {
    console.error(
      `error: a gated crate is below ${minLines} % line coverage. Add tests for the new code.`,
    );
    process.exit(1);
  }
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  main();
}
