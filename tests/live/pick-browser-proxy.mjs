// Picks the two proxies of one live browser self-test run.
//
//   node pick-browser-proxy.mjs <proxy list json> <day number>
//
// Prints two lines, `<proxy id> <time zone> <languages, comma separated>`.
// The time zone and the languages are those of the proxy's country, so that
// the browser the detection pages see fits the address it comes from. The
// pair moves through the pool by the day number and is never one of the
// two that the http self-test shows a third-party echo on the same day
// (pick-selftest-pair.mjs takes 2·day and 2·day + 1).
import { readFileSync } from "node:fs";

// The countries of the pool (docs/00-decisions.md). A persona's language
// list is its country's language first and English last, as browsers
// installed in that country are set up.
export const COUNTRIES = {
  AT: { timezone: "Europe/Vienna", languages: ["de-AT", "de", "en"] },
  DE: { timezone: "Europe/Berlin", languages: ["de-DE", "de", "en"] },
  FR: { timezone: "Europe/Paris", languages: ["fr-FR", "fr", "en"] },
  GB: { timezone: "Europe/London", languages: ["en-GB", "en"] },
  IT: { timezone: "Europe/Rome", languages: ["it-IT", "it", "en"] },
};

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
  const country = COUNTRIES[countryOf(proxy)];
  if (country !== undefined) {
    picked.push(`${proxy.id} ${country.timezone} ${country.languages.join(",")}`);
  }
}
if (picked.length < 2) {
  console.error(
    `fewer than two usable proxies are in a country this check knows the time zone of (${Object.keys(COUNTRIES).join(", ")})`,
  );
  process.exit(1);
}
for (const line of picked) {
  console.log(line);
}
