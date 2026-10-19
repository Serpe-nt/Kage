// Changes the switch lists of a built Chromium engine for a measurement
// image of the live check (measure-chromium.Dockerfile), and nothing else:
//
//   node measure-switches.mjs <dist/launch-plan.js> with|without
//
// `with`: Kage's switches as they are, plus Chrome's NetLog
// (`--log-net-log`, its default capture mode, which leaves cookies and
// credentials out). `without`: the same, and --disable-background-networking
// left to Chrome. It checks the result by loading the module and fails when
// a list is not what it must be.
import { readFileSync, writeFileSync } from 'node:fs';

const [file, mode] = process.argv.slice(2);
if (!file || !['with', 'without'].includes(mode ?? '')) {
  console.error('usage: measure-switches.mjs <dist/launch-plan.js> with|without');
  process.exit(2);
}
const NETLOG = ["'--log-net-log=/tmp/kage-netlog.json'", "'--net-log-capture-mode=Default'"];
const QUIET = "'--disable-background-networking'";
let text = readFileSync(file, 'utf8');
const insert = (anchor, items) => {
  if (!text.includes(anchor)) {
    console.error(`measure-switches: ${anchor} is not in ${file}`);
    process.exit(1);
  }
  text = text.replace(anchor, `${anchor}\n    ${items.join(',\n    ')},`);
};
insert('export const KAGE_SWITCHES = [', NETLOG);
if (mode === 'without') {
  const kept = text.indexOf('export const DRIVER_SWITCHES_KEPT = [');
  const at = text.indexOf(QUIET, kept);
  if (kept < 0 || at < 0) {
    console.error('measure-switches: --disable-background-networking is not among the kept switches');
    process.exit(1);
  }
  text = `${text.slice(0, at)}${text.slice(at + QUIET.length).replace(/^,/, '')}`;
  insert('export const DRIVER_SWITCHES_LEFT_OUT = [', [QUIET]);
}
writeFileSync(file, text);

const plan = await import(file);
const quiet = '--disable-background-networking';
const ok =
  plan.KAGE_SWITCHES.includes('--log-net-log=/tmp/kage-netlog.json') &&
  plan.DRIVER_SWITCHES_KEPT.includes(quiet) === (mode === 'with') &&
  plan.DRIVER_SWITCHES_LEFT_OUT.includes(quiet) === (mode === 'without');
if (!ok) {
  console.error('measure-switches: the lists are not what the measurement needs');
  process.exit(1);
}
console.log(`measure-switches: ${mode} --disable-background-networking, with the NetLog`);
