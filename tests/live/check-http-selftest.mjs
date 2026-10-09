// Judges the reports of the live fingerprint self-test.
//
//   node check-http-selftest.mjs <report json>...
//
// Each file is the output of `kage selftest http --json`. The check passes
// when there is a report for a Chrome and for a Firefox profile and every
// comparison in every report held. It prints what was compared, and never
// the address the echo saw.
import { readFileSync } from "node:fs";

const files = process.argv.slice(2);
if (files.length === 0) {
  console.error("usage: check-http-selftest.mjs <report json>...");
  process.exit(2);
}

const problems = [];
const families = new Set();
for (const file of files) {
  let report;
  try {
    report = JSON.parse(readFileSync(file, "utf8"));
  } catch (error) {
    problems.push(`${file}: no report (${error.message})`);
    continue;
  }
  const name = `${report.profile} on ${report.os}`;
  families.add(String(report.profile).split("_")[0]);
  const timing = report.timing ?? {};
  console.log(
    `    ${name}: asked ${report.echo} (${report.observed?.source} format), ` +
      `tunnel ${timing.connect_ms ?? "-"} ms, handshake ${timing.tls_ms ?? "-"} ms, ` +
      `total ${timing.total_ms ?? "-"} ms`,
  );
  for (const finding of report.findings ?? []) {
    if (finding.passed) {
      console.log(`      ${finding.check}: ${finding.observed}`);
    } else {
      console.log(`      ${finding.check}: DIFFERS`);
      console.log(`        recorded: ${finding.expected}`);
      console.log(`        observed: ${finding.observed}`);
      problems.push(`${name}: ${finding.check} differs from the recorded value`);
    }
  }
  if ((report.findings ?? []).length === 0) {
    problems.push(`${name}: the report holds no comparison`);
  }
  if (report.passed !== true && !problems.some((problem) => problem.startsWith(name))) {
    problems.push(`${name}: the report does not say passed`);
  }
}
for (const family of ["chrome", "firefox"]) {
  if (!families.has(family)) {
    problems.push(`no report for a ${family} profile`);
  }
}

if (problems.length > 0) {
  for (const problem of problems) {
    console.error(`error: ${problem}`);
  }
  process.exit(1);
}
console.log(`    ${files.length} self-tests passed: the wire fingerprints are the recorded ones`);
