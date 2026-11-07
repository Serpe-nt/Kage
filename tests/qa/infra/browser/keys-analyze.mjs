// Reads a job item's result (JSON on stdin) whose dom_html is keys.html after
// typing, and the texts that were typed (argv: input text, textarea text).
// Prints what arrived, what was lost, and the timing of the keys.
import { readFileSync } from 'node:fs';
const result = JSON.parse(readFileSync(0, 'utf8'));
const [wantI = '', wantT = ''] = process.argv.slice(2);
const dom = String(result.dom_html ?? '');
const pre = (id) => {
  const m = dom.match(new RegExp(`<pre id="${id}">([\\s\\S]*?)</pre>`));
  if (!m) return null;
  const text = m[1].replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&');
  return text;
};
const parse = (s) => { try { return JSON.parse(s); } catch { return s; } };
const gotI = parse(pre('vi') || '""');
const gotT = parse(pre('vt') || '""');
const log = parse(pre('log') || '[]') || [];
const env = parse(pre('env') || '{}');
const report = { env, input: { want: wantI, got: gotI, same: gotI === wantI }, textarea: { want: wantT, got: gotT, same: gotT === wantT }, submits: pre('sub') };
// Lost characters: an alignment by a simple diff of want and got.
const lost = (want, got) => {
  const out = [];
  let j = 0;
  for (let i = 0; i < want.length; i += 1) {
    if (got[j] === want[i]) { j += 1; continue; }
    out.push({ at: i, char: want[i], before: want.slice(Math.max(0, i - 3), i) });
  }
  return { lost: out, extra: got.length - j };
};
report.input.diff = lost([...wantI].join(''), String(gotI));
report.textarea.diff = lost([...wantT].join(''), String(gotT));
const MOD = new Set(['Shift', 'AltGraph', 'Alt', 'Control', 'Meta', 'CapsLock']);
const downs = log.filter((e) => e[0] === 'd');
const keys = downs.filter((e) => !MOD.has(e[1]));
const intervals = [];
for (let k = 1; k < keys.length; k += 1) {
  if (keys[k][1] === 'Backspace' || keys[k - 1][5] !== keys[k][5]) continue;
  intervals.push(keys[k][3] - keys[k - 1][3]);
}
// Holds and presses of a key still down, by code.
const downAt = new Map();
const holds = [];
const whileHeld = [];
for (const e of log) {
  const [type, key, code, t, repeat] = e;
  if (type === 'd') {
    if (downAt.has(code) && !MOD.has(key)) whileHeld.push({ key, code, t });
    if (!repeat) downAt.set(code, { t, key });
  } else {
    const d = downAt.get(code);
    if (d) { if (!MOD.has(key)) holds.push(t - d.t); downAt.delete(code); }
  }
}
const stats = (a) => {
  if (a.length === 0) return null;
  const s = [...a].sort((x, y) => x - y);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(p * s.length))];
  return { n: s.length, min: s[0], p05: q(0.05), median: q(0.5), p95: q(0.95), max: s.at(-1), mean: Math.round(s.reduce((x, y) => x + y, 0) / s.length) };
};
report.events = log.length;
report.repeats = log.filter((e) => e[4]).length;
report.untrusted = log.filter((e) => !e[8]).length;
report.intervals = stats(intervals);
report.intervalsOutside60to220 = intervals.filter((x) => x < 58 || x > 222).map((x) => Math.round(x));
report.holds = stats(holds);
report.pressWhileHeld = whileHeld;
report.altGrKeys = [...new Set(log.filter((e) => e[0] === 'd' && e[6]).map((e) => e[1]))].join('');
report.codesOf = Object.fromEntries([...new Set(keys.map((e) => e[1]))].filter((k) => /[^a-z0-9 ]/i.test(k)).map((k) => [k, keys.find((e) => e[1] === k)[2]]));
console.log(JSON.stringify(report, null, process.env.PRETTY ? 1 : 0));
