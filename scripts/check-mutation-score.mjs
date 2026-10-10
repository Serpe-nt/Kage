#!/usr/bin/env node
// Judges a cargo-mutants run (spec §21: the mutation score of kage-core is
// at least 70 % from M7 on).
//
//   node scripts/check-mutation-score.mjs <mutants.out directory> <minimum percent>
//
// The score is the share of mutants the tests noticed among those that
// were tested: caught / (caught + missed). A mutant that made the tests
// run into their timeout was noticed too, but it is reported apart and
// left out of the score, so that a suite cannot pass by hanging; the
// figure with every timeout counted as missed is printed as well.
// Mutants that do not compile say nothing about the tests.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

const [directory, minimumText] = process.argv.slice(2);
const minimum = Number(minimumText);
if (directory === undefined || !Number.isFinite(minimum)) {
  process.stderr.write('usage: check-mutation-score.mjs <mutants.out directory> <minimum percent>\n');
  process.exit(2);
}

const count = (name) => {
  try {
    return readFileSync(join(directory, `${name}.txt`), 'utf8').split('\n').filter((line) => line.trim() !== '').length;
  } catch {
    return 0;
  }
};
const caught = count('caught');
const missed = count('missed');
const timeout = count('timeout');
const unviable = count('unviable');
const tested = caught + missed;
if (tested === 0) {
  process.stderr.write(`mutation score: no mutant was tested (looked in ${directory})\n`);
  process.exit(1);
}
const percent = (part, whole) => Math.round((part / whole) * 1000) / 10;
const score = percent(caught, tested);
const strict = percent(caught, tested + timeout);
process.stdout.write(
  `mutation score: ${score} % (${caught} caught, ${missed} missed; ${timeout} timed out, ${unviable} did not compile; ` +
    `${strict} % with every timeout counted as missed)\n`,
);
if (score < minimum) {
  process.stderr.write(`mutation score ${score} % is below the required ${minimum} %\n`);
  process.exit(1);
}
