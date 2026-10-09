// Judges the report of the live browser self-test.
//
//   node check-browser-selftest.mjs <report json> <baseline directory> [--record]
//
// The report is the output of `kage selftest browser --json`. The check
// passes when the sidecar passed the self-test, no check carries an
// automation flag or an identity mismatch (a hard failure), every page
// could be read, and no check is worse than in the recorded baseline
// (`<baseline directory>/<engine>-<major>-live.json`).
//
// `--record` writes the findings as the new baseline. What is written holds
// no address: the report names none, and anything that looks like one in
// what a page showed is replaced before it is stored.
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const [reportFile, baselineDir, flag] = process.argv.slice(2);
if (!reportFile || !baselineDir || (flag !== undefined && flag !== "--record")) {
  console.error("usage: check-browser-selftest.mjs <report json> <baseline directory> [--record]");
  process.exit(2);
}

let report;
try {
  report = JSON.parse(readFileSync(reportFile, "utf8"));
} catch (error) {
  console.error(`error: no report (${error.message})`);
  process.exit(1);
}

const IPV4 = /(?<![\w.])(?:\d{1,3}\.){3}\d{1,3}(?![\w.])/g;
const IPV6 = /\b(?:[0-9a-f]{1,4}:){3,7}[0-9a-f]{1,4}\b/gi;
// Four dotted numbers are an address, with one exception: the version of a
// reduced user agent, `Chrome/155.0.0.0`, is a name, a slash and a number
// followed by three zeros. The user agent is a value the baseline should keep.
const address = (found, offset, text) =>
  /^\d{1,3}\.0\.0\.0$/.test(found) && /[A-Za-z]\/$/.test(text.slice(0, offset))
    ? found
    : "[address]";
/** What a page showed, without anything that looks like an address. */
export const scrub = (text) =>
  typeof text === "string" ? text.replace(IPV4, address).replace(IPV6, "[address]") : text;

const RANK = { pass: 0, warn: 1, fail: 2 };
const problems = [];
const findings = report.findings ?? [];
const pages = report.pages ?? [];
const persona = report.persona ?? {};
const name = `${report.engine} ${persona.browser_version} on ${persona.claimed_os}`;

console.log(`    ${name}: score ${report.score} of 100, threshold ${report.min_score}`);
for (const page of pages) {
  console.log(`      page ${page.id} (${page.kind}): ${page.status}, HTTP ${page.http_status ?? "-"}`);
  if (page.status !== "ok") {
    problems.push(`page ${page.id} is ${page.status}: the verdict would rest on fewer checks`);
  }
}
for (const finding of findings) {
  const label = finding.status === "fail" && finding.hard ? "FAIL" : finding.status;
  console.log(`      ${finding.page}/${finding.check}: ${label} (${scrub(finding.observed) ?? "-"})`);
  if (finding.status === "fail" && finding.hard) {
    problems.push(`${finding.page}/${finding.check} failed: ${scrub(finding.observed) ?? "-"}`);
  }
}
if (findings.length === 0) {
  problems.push("the report holds no finding");
}
if (report.passed !== true) {
  problems.push("the sidecar did not pass the self-test");
}

const major = String(persona.browser_major ?? "unknown");
const baselineFile = join(baselineDir, `${report.engine}-${major}-live.json`);
if (existsSync(baselineFile)) {
  const baseline = JSON.parse(readFileSync(baselineFile, "utf8"));
  const before = new Map(
    baseline.findings.map((finding) => [`${finding.page}/${finding.check}`, finding.status]),
  );
  for (const finding of findings) {
    const key = `${finding.page}/${finding.check}`;
    const was = before.get(key);
    // A check that could not be read on either side says nothing.
    if (was in RANK && finding.status in RANK && RANK[finding.status] > RANK[was]) {
      problems.push(`${key} was ${was} in the baseline of ${baseline.recorded} and is ${finding.status} now`);
    }
  }
  for (const key of before.keys()) {
    if (!findings.some((finding) => `${finding.page}/${finding.check}` === key)) {
      problems.push(`${key} is in the baseline of ${baseline.recorded} and was not checked now`);
    }
  }
  console.log(`    compared with the baseline of ${baseline.recorded} (${before.size} checks)`);
} else {
  console.log(`    no baseline yet (${baselineFile}); run once with KAGE_LIVE_RECORD=1`);
}

if (flag === "--record") {
  if (problems.length > 0) {
    console.error("error: a failing run is not recorded as the baseline");
  } else {
    mkdirSync(baselineDir, { recursive: true });
    const document = {
      engine: report.engine,
      browser_version: persona.browser_version,
      claimed_os: persona.claimed_os,
      recorded: new Date().toISOString().slice(0, 10),
      score: report.score,
      min_score: report.min_score,
      pages: pages.map((page) => ({ id: page.id, kind: page.kind, url: page.url })),
      findings: findings.map((finding) => ({
        page: finding.page,
        check: finding.check,
        status: finding.status,
        hard: finding.hard,
        weight: finding.weight,
        observed: scrub(finding.observed),
        expected: scrub(finding.expected),
      })),
    };
    writeFileSync(baselineFile, `${JSON.stringify(document, null, 2)}\n`);
    console.log(`    recorded ${baselineFile}`);
  }
}

if (problems.length > 0) {
  for (const problem of problems) {
    console.error(`error: ${problem}`);
  }
  process.exit(1);
}
console.log("    the browser self-test passed: no automation flag, the identity is coherent");
