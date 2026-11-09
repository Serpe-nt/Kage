// QA round four: reads a result (JSON on stdin, `{dom_html}`) whose document
// is keys4.html after typing, and the texts that were meant for the input
// and the textarea (argv). Prints what arrived, what was lost or added, the
// timing of the keys by kind (an ordinary key, a lent key with no `code`, a
// dead key or the IME's `Process`), presses while the key was held, capitals
// without Shift, the code each character came on, and every typo: the
// slipped key, the key meant, and whether the two are neighbours on a
// physical keyboard (by `code`, which names the position, not the symbol).
import { readFileSync } from 'node:fs';

const result = JSON.parse(readFileSync(0, 'utf8'));
const [wantI = '', wantT = ''] = process.argv.slice(2);
const dom = String(result.dom_html ?? '');
const pre = (id) => {
  const m = dom.match(new RegExp(`<pre id="${id}">([\\s\\S]*?)</pre>`));
  if (!m) return null;
  return m[1].replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/&amp;/g, '&');
};
const parse = (s, d) => { try { return JSON.parse(s); } catch { return d; } };
const gotI = parse(pre('vi') || '""', '');
const gotT = parse(pre('vt') || '""', '');
const log = parse(pre('log') || '[]', []) || [];
const inputs = parse(pre('inp') || '[]', []) || [];
const env = parse(pre('env') || '{}', {});
const ev = (e) => ({ kind: e[0], key: e[1], code: e[2], t: e[3], repeat: e[4], target: e[5], altGr: e[6], shift: e[7], trusted: e[8], keyCode: e[9], location: e[10], composing: e[11], caps: e[12] });
const events = log.map(ev);

const diff = (want, got) => {
  const w = [...want];
  const g = [...String(got)];
  const lost = [];
  let j = 0;
  for (let i = 0; i < w.length; i += 1) {
    if (g[j] === w[i]) { j += 1; continue; }
    lost.push({ at: i, char: w[i], before: w.slice(Math.max(0, i - 3), i).join('') });
  }
  return { lost, extra: g.length - j };
};
const report = {
  env: { lang: env.lang, langs: env.langs, layoutSample: env.layout ? Object.fromEntries(['KeyQ', 'KeyW', 'KeyY', 'KeyZ', 'KeyA', 'KeyM', 'Semicolon', 'Digit2', 'Backquote', 'Quote', 'BracketLeft', 'IntlBackslash', 'Slash'].map((c) => [c, env.layout[c]])) : null },
  input: { want: wantI, got: gotI, same: gotI === wantI, diff: diff(wantI, gotI) },
  textarea: { want: wantT, got: gotT, same: gotT === wantT, diff: diff(wantT, gotT) },
  submits: pre('sub'),
  select: parse(pre('vs') || '""', ''),
  mouse: parse(pre('mouse') || '{}', {}),
};

const MOD = new Set(['Shift', 'AltGraph', 'Alt', 'Control', 'Meta', 'CapsLock']);
const kindOf = (e) => (e.code === '' ? 'lent' : e.key === 'Dead' || e.key === 'Process' ? 'dead' : 'plain');
// Holds: a keydown and the next keyup of the same physical key (by code, or
// by key where there is no code).
const id = (e) => (e.code !== '' ? `c:${e.code}` : `k:${e.key}`);
const down = new Map();
const holds = { plain: [], lent: [], dead: [] };
const whileHeld = [];
const pressesNoCode = [];
let lastCodeless = null;
for (const e of events) {
  if (e.kind === 'p') continue;
  if (e.kind === 'd') {
    if (e.code === '') pressesNoCode.push({ key: e.key, keyCode: e.keyCode, t: e.t });
    if (down.has(id(e)) && !MOD.has(e.key)) whileHeld.push({ key: e.key, code: e.code, t: e.t });
    if (!e.repeat) down.set(id(e), e);
    // Lent keys share no code: a codeless keydown while another is held is still a press of a held key position.
    if (e.code === '' && lastCodeless !== null && !lastCodeless.up) whileHeld.push({ key: e.key, code: '', t: e.t, codelessOverlap: lastCodeless.key });
    if (e.code === '') lastCodeless = { key: e.key, up: false };
  } else {
    const d = down.get(id(e)) ?? (e.code === '' ? [...down.values()].find((x) => x.code === '') : undefined);
    if (d) {
      if (!MOD.has(d.key)) holds[kindOf(d)].push(Math.round((e.t - d.t) * 10) / 10);
      down.delete(id(d));
    }
    if (e.code === '' && lastCodeless) lastCodeless.up = true;
  }
}
const stats = (a) => {
  if (a.length === 0) return null;
  const s = [...a].sort((x, y) => x - y);
  const q = (p) => s[Math.min(s.length - 1, Math.floor(p * s.length))];
  return { n: s.length, min: s[0], p05: q(0.05), median: q(0.5), p95: q(0.95), max: s.at(-1), mean: Math.round(s.reduce((x, y) => x + y, 0) / s.length) };
};
const keydowns = events.filter((e) => e.kind === 'd' && !MOD.has(e.key));
const intervals = [];
for (let k = 1; k < keydowns.length; k += 1) {
  if (keydowns[k].key === 'Backspace' || keydowns[k - 1].target !== keydowns[k].target) continue;
  intervals.push(Math.round((keydowns[k].t - keydowns[k - 1].t) * 10) / 10);
}
report.events = events.length;
report.untrusted = events.filter((e) => !e.trusted).length;
report.repeats = events.filter((e) => e.repeat).length;
report.intervals = stats(intervals);
report.intervalsOutside60to220 = intervals.filter((x) => x < 58 || x > 222);
report.holds = { plain: stats(holds.plain), lent: stats(holds.lent), dead: stats(holds.dead) };
report.holdsOutside35to160 = Object.fromEntries(Object.entries(holds).map(([k, v]) => [k, v.filter((x) => x < 33 || x > 162)]));
report.pressWhileHeld = whileHeld;
report.pressesWithNoCode = pressesNoCode;
report.nulKeys = keydowns.filter((e) => e.key === '\u0000').length;
report.deadOrProcess = keydowns.filter((e) => e.key === 'Dead' || e.key === 'Process').map((e) => `${e.key}/${e.code}`);
report.enterPresses = keydowns.filter((e) => e.key === 'Enter').length;
report.newlinesWanted = [...wantI, ...wantT].filter((c) => c === '\n' || c === '\r').length;
report.capitalsWithoutShift = keydowns
  .filter((e) => [...e.key].length === 1 && e.key !== e.key.toLowerCase() && e.key === e.key.toUpperCase() && !e.shift && !e.caps)
  .map((e) => `${e.key}/${e.code || '-'}`);
report.codeOf = Object.fromEntries([...new Map(keydowns.filter((e) => [...e.key].length === 1).map((e) => [e.key, `${e.code || '-'}${e.shift ? '+S' : ''}${e.altGr ? '+AG' : ''}`])).entries()].sort());
report.beforeinput = {
  n: inputs.filter((x) => x[0] === 'beforeinput').length,
  types: [...new Set(inputs.filter((x) => x[0] === 'beforeinput').map((x) => x[1]))],
  compositions: inputs.filter((x) => x[0].startsWith('composition')).length,
};

// Typos: a keydown followed (on the same target) by a Backspace. The physical
// positions (`code`) of the slip and of the key typed after the Backspace.
const ROWS = [
  ['Backquote', 'Digit1', 'Digit2', 'Digit3', 'Digit4', 'Digit5', 'Digit6', 'Digit7', 'Digit8', 'Digit9', 'Digit0', 'Minus', 'Equal'],
  ['', 'KeyQ', 'KeyW', 'KeyE', 'KeyR', 'KeyT', 'KeyY', 'KeyU', 'KeyI', 'KeyO', 'KeyP', 'BracketLeft', 'BracketRight'],
  ['', 'KeyA', 'KeyS', 'KeyD', 'KeyF', 'KeyG', 'KeyH', 'KeyJ', 'KeyK', 'KeyL', 'Semicolon', 'Quote', 'Backslash'],
  ['IntlBackslash', 'KeyZ', 'KeyX', 'KeyC', 'KeyV', 'KeyB', 'KeyN', 'KeyM', 'Comma', 'Period', 'Slash', '', ''],
];
const where = new Map();
ROWS.forEach((row, r) => row.forEach((code, c) => { if (code !== '') where.set(code, [r, c + r * 0.5]); }));
const neighbours = (a, b) => {
  const p = where.get(a);
  const q = where.get(b);
  if (!p || !q || a === b) return null;
  return Math.abs(p[0] - q[0]) <= 1 && Math.abs(p[1] - q[1]) <= 1.5;
};
const typos = [];
for (let k = 0; k + 2 < keydowns.length; k += 1) {
  if (keydowns[k + 1].key === 'Backspace' && keydowns[k].key !== 'Backspace') {
    const slip = keydowns[k];
    const meant = keydowns[k + 2];
    typos.push({ slip: slip.key, slipCode: slip.code, meant: meant.key, meantCode: meant.code, neighbours: neighbours(slip.code, meant.code) });
  }
}
report.typos = { n: typos.length, notNeighbours: typos.filter((x) => x.neighbours === false).length, list: typos };
console.log(JSON.stringify(report, null, process.env.PRETTY ? 1 : 0));
