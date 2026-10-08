// Judges the output of `kage proxy health --json` for the live check
// (tests/live/proxy-health.sh): the pool has the expected number of proxies
// and every one answered with a latency and a country.
//
//   node tests/live/check-proxy-health.mjs <health.json> <expected-count>
//
// It prints one line per proxy without its address, so the output is safe to
// keep in a CI log.
import { readFileSync } from "node:fs";

/**
 * Checks a parsed `kage proxy health --json` document.
 *
 * @param {unknown} document the parsed output
 * @param {number} expected how many proxies the plan has
 * @returns {{ lines: string[], problems: string[] }} one line per proxy, and
 *   what is wrong (empty when the check passes)
 */
function judge(document, expected) {
  const lines = [];
  const problems = [];
  const results = Array.isArray(document?.results) ? document.results : null;
  if (results === null) {
    return { lines, problems: ["the output has no `results` list"] };
  }

  if (results.length !== expected) {
    problems.push(`expected ${expected} proxies, the provider's list has ${results.length}`);
  }
  if (document.host_address_known !== true) {
    problems.push("the host's own address could not be read, so the leak check did not run");
  }
  if (document.summary?.egress_unsafe !== false) {
    problems.push("the pool is flagged egress_unsafe: a proxy let the host's own address through");
  }

  results.forEach((result, index) => {
    const health = result.last_health ?? {};
    const latency = health.latency_ms;
    const country = result.observed_country ?? health.exit_country ?? null;
    const label = `proxy ${String(index + 1).padStart(2, "0")} (${result.id})`;
    lines.push(
      [
        label,
        String(result.verdict).padEnd(11),
        (Number.isFinite(latency) ? `${latency} ms` : "-").padStart(8),
        `observed ${country ?? "-"}`,
        `provider ${result.country ?? "-"}`,
        result.state,
      ].join("  "),
    );

    if (result.verdict !== "ok") {
      problems.push(`${label}: the probe ended with ${result.verdict}`);
      return;
    }
    if (!Number.isFinite(latency) || latency <= 0) {
      problems.push(`${label}: no latency was measured`);
    }
    if (typeof country !== "string" || !/^[A-Z]{2}$/.test(country)) {
      problems.push(`${label}: the geo lookup named no country`);
    }
    if (health.leak_detected !== false) {
      problems.push(`${label}: the leak check did not pass`);
    }
  });

  return { lines, problems };
}

function main(argv) {
  const [file, count] = argv;
  const expected = Number.parseInt(count ?? "", 10);
  if (!file || !Number.isInteger(expected) || expected < 1) {
    console.error("usage: check-proxy-health.mjs <health.json> <expected-count>");
    return 2;
  }

  let document;
  try {
    document = JSON.parse(readFileSync(file, "utf8"));
  } catch (error) {
    console.error(`error: cannot read ${file} as JSON: ${error.message}`);
    return 1;
  }

  const { lines, problems } = judge(document, expected);
  for (const line of lines) {
    console.log(`    ${line}`);
  }
  if (problems.length > 0) {
    for (const problem of problems) {
      console.error(`FAIL: ${problem}`);
    }
    return 1;
  }
  console.log(`    ${lines.length} proxies answered, each with a latency and a country`);
  return 0;
}

process.exitCode = main(process.argv.slice(2));
