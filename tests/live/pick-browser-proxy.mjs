// Picks the two proxies of one live browser self-test run.
//
//   node pick-browser-proxy.mjs <proxy list json> <day number>
//
// Prints two lines, `<proxy id> <country>`. The time zone and the languages
// of the browser are not chosen here: `kage selftest browser` derives them
// from the proxy's observed geo record and the stock language list of the
// engine's browser in that country, as it does for a persona, so that the
// browser the detection pages see fits the address it comes from. The
// pair moves through the pool by the day number and is never one of the
// two that the http self-test shows a third-party echo on the same day
// (pick-selftest-pair.mjs takes 2·day and 2·day + 1).
import { readFileSync } from "node:fs";

// The countries of the pool (docs/00-decisions.md): the ones Kage has a
// locale and a stock language list for (`kage_core::persona::locale`).
export const COUNTRIES = new Set(["AT", "DE", "FR", "GB", "IT"]);

const [listFile, dayText] = process.argv.slice(2);
if (!listFile || !dayText) {
  console.error("usage: pick-browser-proxy.mjs <proxy list json> <day number>");
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
  .sort((a, b) => (a.id < b.id ? -1 : 1));
if (proxies.length < 4) {
  console.error(`the pool has ${proxies.length} usable proxies; the live suite needs four`);
  process.exit(1);
}

// Where the address was observed to be wins over what the provider says.
const countryOf = (proxy) => String(proxy.observed_country ?? proxy.country ?? "").toUpperCase();
const picked = [];
for (let step = 0; step < proxies.length - 2 && picked.length < 2; step += 1) {
  const proxy = proxies[(2 * day + 2 + step) % proxies.length];
  const country = countryOf(proxy);
  if (COUNTRIES.has(country)) {
    picked.push(`${proxy.id} ${country}`);
  }
}
if (picked.length < 2) {
  console.error(
    `fewer than two usable proxies are in a country of the pool (${[...COUNTRIES].join(", ")})`,
  );
  process.exit(1);
}
for (const line of picked) {
  console.log(line);
}
