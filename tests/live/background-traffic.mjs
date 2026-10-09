// Reports what an idle browser context sent and received by itself.
//
//   node background-traffic.mjs <samples file> <sidecar log>
//
// The samples are lines `<epoch ms> <rx bytes> <tx bytes>`: the counters of
// the sidecar container's network interface, read every few seconds while
// `kage selftest browser --idle-before` kept one context open and untouched.
// Everything that container sends goes to the persona's proxy, so between
// the moment the context opened and the moment the self-test asked for its
// first page, the counters hold Chrome's own traffic and nothing else:
// component updates, safe browsing lists, field trial data.
//
// The interface counts whole packets, so the figures include TCP and IP
// headers; a proxy provider bills a little less. This is a measurement, not
// a check: it always exits 0 and says when it could not measure.
import { readFileSync } from "node:fs";

const [samplesFile, logFile] = process.argv.slice(2);
if (!samplesFile || !logFile) {
  console.error("usage: background-traffic.mjs <samples file> <sidecar log>");
  process.exit(2);
}

const samples = readFileSync(samplesFile, "utf8")
  .split("\n")
  .map((line) => line.trim().split(/\s+/).map(Number))
  .filter((fields) => fields.length === 3 && fields.every(Number.isFinite))
  .map(([at, rx, tx]) => ({ at, rx, tx }));

let opened = null;
let asked = null;
for (const line of readFileSync(logFile, "utf8").split("\n")) {
  let entry;
  try {
    entry = JSON.parse(line);
  } catch {
    continue;
  }
  if (typeof entry?.time !== "number") {
    continue;
  }
  if (opened === null && entry.msg === "context opened") {
    opened = entry.time;
  } else if (opened !== null && asked === null && String(entry.req?.url ?? "").endsWith("/selftest")) {
    asked = entry.time;
  }
}

const inside = samples.filter((sample) => opened !== null && asked !== null && sample.at >= opened && sample.at <= asked);
if (inside.length < 3) {
  console.log("    background traffic: not measured (no idle window in the samples)");
  process.exit(0);
}

const first = inside[0];
const last = inside.at(-1);
const seconds = (last.at - first.at) / 1000;
const total = (sample) => sample.rx + sample.tx;
const bytes = total(last) - total(first);
/** Bytes between the first sample and the last one taken within `ms` of it. */
const until = (ms) => {
  const sample = inside.findLast((candidate) => candidate.at - first.at <= ms) ?? first;
  return { bytes: total(sample) - total(first), seconds: (sample.at - first.at) / 1000 };
};

// A browser that has just started fetches more than one that has been
// open for a while, so the start and the rest are told apart.
const START_MS = 120_000;
const start = until(START_MS);
const rest = { bytes: bytes - start.bytes, seconds: seconds - start.seconds };
const perHour = (part) => (part.seconds > 0 ? Math.round((part.bytes / part.seconds) * 3600) : 0);
const kb = (count) => `${(count / 1000).toFixed(1)} kB`;

console.log(`    background traffic of one idle context, ${Math.round(seconds)} s after it opened:`);
console.log(`      received ${kb(last.rx - first.rx)}, sent ${kb(last.tx - first.tx)}, together ${kb(bytes)}`);
console.log(`      the first ${Math.round(start.seconds)} s (the browser has just started): ${kb(start.bytes)}`);
if (rest.seconds >= 60) {
  console.log(
    `      the ${Math.round(rest.seconds)} s after that: ${kb(rest.bytes)}, which is ${kb(perHour(rest))} per hour`,
  );
  console.log(
    `      one persona-hour that starts with a browser start: about ${kb(start.bytes + Math.round((perHour(rest) * (3600 - start.seconds)) / 3600))}`,
  );
}
const minutes = [];
for (let minute = 1; (minute - 1) * 60_000 < last.at - first.at; minute += 1) {
  minutes.push(until(minute * 60_000).bytes - until((minute - 1) * 60_000).bytes);
}
console.log(`      per minute: ${minutes.map(kb).join(", ")}`);
