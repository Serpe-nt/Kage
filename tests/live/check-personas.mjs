// Judges the personas the live check generated for the real pool, and
// picks the two whose self-test it runs.
//
//   node check-personas.mjs <persona list json> <proxy list json>
//
// The lists are the outputs of `kage persona list --json` and `kage proxy
// list --json`. The check passes when every usable proxy has exactly one
// live persona, every persona is where its proxy was observed to be and
// speaks that country's language, a chromium persona claims Linux and a
// firefox persona Windows or macOS, the engines and systems are within two
// personas of the shares of the default mix, no two personas share a seed,
// and the pool shows at least three different screens.
//
// Prints two lines, `<engine> <persona id>`: the first active persona of
// each engine by id.
import { readFileSync } from "node:fs";

const [personaFile, proxyFile] = process.argv.slice(2);
if (!personaFile || !proxyFile) {
  console.error("usage: check-personas.mjs <persona list json> <proxy list json>");
  process.exit(2);
}

// The default mix (personas.mix in deploy/kage.example.yaml).
const MIX = [
  ["chromium/linux", 0.4],
  ["firefox/windows", 0.45],
  ["firefox/macos", 0.15],
];

const personas = JSON.parse(readFileSync(personaFile, "utf8")).personas.filter(
  (persona) => persona.state !== "retired",
);
const proxies = JSON.parse(readFileSync(proxyFile, "utf8")).proxies;
const usable = proxies.filter((proxy) => ["healthy", "degraded"].includes(proxy.state));
const problems = [];

const byProxy = new Map();
for (const persona of personas) {
  if (byProxy.has(persona.proxy_id)) {
    problems.push(`proxy ${persona.proxy_id} has two live personas`);
  }
  byProxy.set(persona.proxy_id, persona);
}
for (const proxy of usable) {
  const persona = byProxy.get(proxy.id);
  if (persona === undefined) {
    problems.push(`usable proxy ${proxy.id} has no persona`);
    continue;
  }
  // Where the address was observed to be wins over what the provider says.
  const country = String(proxy.observed_country ?? proxy.country ?? "").toUpperCase();
  if (persona.country !== country) {
    problems.push(`persona ${persona.id} is in ${persona.country}, its proxy in ${country}`);
  }
  if (!String(persona.locale).endsWith(`-${country}`)) {
    problems.push(`persona ${persona.id} in ${country} has the locale ${persona.locale}`);
  }
  if (!Array.isArray(persona.languages) || persona.languages.length === 0) {
    problems.push(`persona ${persona.id} has no languages`);
  }
  const allowed = persona.engine === "chromium" ? ["linux"] : ["windows", "macos"];
  if (!allowed.includes(persona.claimed_os)) {
    problems.push(`${persona.engine} persona ${persona.id} claims ${persona.claimed_os}`);
  }
}
if (personas.length !== usable.length) {
  problems.push(`${personas.length} live personas for ${usable.length} usable proxies`);
}

const counts = new Map();
for (const persona of personas) {
  const entry = `${persona.engine}/${persona.claimed_os}`;
  counts.set(entry, (counts.get(entry) ?? 0) + 1);
}
for (const [entry, share] of MIX) {
  const count = counts.get(entry) ?? 0;
  const wanted = share * personas.length;
  if (Math.abs(count - wanted) > 2) {
    problems.push(`${count} personas are ${entry}; the mix asks for about ${wanted.toFixed(1)}`);
  }
}

const seeds = new Set(personas.map((persona) => persona.seed));
if (seeds.size !== personas.length) {
  problems.push("two personas share a seed");
}
const screens = new Set(
  personas.map((persona) => `${persona.screen?.width}x${persona.screen?.height}`),
);
if (personas.length >= 10 && screens.size < 3) {
  problems.push(`the pool shows only ${screens.size} different screens`);
}

const mix = MIX.map(([entry]) => `${entry} ${counts.get(entry) ?? 0}`).join(", ");
console.error(`    ${personas.length} personas: ${mix}; ${screens.size} screens`);
if (problems.length > 0) {
  for (const problem of problems) {
    console.error(`error: ${problem}`);
  }
  process.exit(1);
}

const active = personas
  .filter((persona) => persona.state === "active")
  .sort((a, b) => (a.id < b.id ? -1 : 1));
for (const engine of ["chromium", "firefox"]) {
  const persona = active.find((candidate) => candidate.engine === engine);
  if (persona === undefined) {
    console.error(`error: the pool has no active ${engine} persona`);
    process.exit(1);
  }
  console.log(`${engine} ${persona.id}`);
}
