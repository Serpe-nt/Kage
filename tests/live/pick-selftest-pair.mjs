// Picks the two proxies and the two profiles of one live self-test run.
//
//   node pick-selftest-pair.mjs <proxy list json> <config check json> <day number>
//
// Prints two lines, `<proxy id> <profile>`: the newest Chrome profile and the
// newest Firefox profile this build serves, each through one usable proxy.
// The pair moves through the pool by the day number, so that an echo run by
// someone else sees each address about once in (pool size / 2) runs.
import { readFileSync } from "node:fs";

const [listFile, configFile, dayText] = process.argv.slice(2);
if (!listFile || !configFile || !dayText) {
  console.error("usage: pick-selftest-pair.mjs <proxy list json> <config check json> <day number>");
  process.exit(2);
}

const day = Number(dayText);
if (!Number.isInteger(day) || day < 0) {
  console.error(`the day number ${JSON.stringify(dayText)} is not a non-negative integer`);
  process.exit(2);
}

// A proxy in backoff or being replaced would fail for its own reasons.
const usable = new Set(["new", "healthy", "degraded"]);
const proxies = JSON.parse(readFileSync(listFile, "utf8"))
  .proxies.filter((proxy) => usable.has(proxy.state))
  .map((proxy) => proxy.id)
  .sort();
if (proxies.length < 2) {
  console.error(`the pool has ${proxies.length} usable proxies; the self-test needs two`);
  process.exit(1);
}

const profiles = JSON.parse(readFileSync(configFile, "utf8")).http.exact_profiles;
const newest = (majors) => Math.max(...majors);
const chrome = `chrome_${newest(profiles.chromium)}`;
const firefox = `firefox_${newest(profiles.firefox)}`;

const first = (2 * day) % proxies.length;
const second = (2 * day + 1) % proxies.length;
console.log(`${proxies[first]} ${chrome}`);
console.log(`${proxies[second]} ${firefox}`);
