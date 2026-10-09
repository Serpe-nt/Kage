import type {
  EchoExpectation,
  ExpectedIdentity,
  Finding,
  SelftestPageKind,
  SelftestRule,
} from '@kage/contract';
import { z } from 'zod';

import { browserMajorOfUserAgent, osOfHintPlatform, osOfUserAgent } from '../identity.js';

import { FindingList } from './findings.js';

/** One table row of a page: the text of its cells and the class of each cell. */
export interface TableRow {
  readonly cells: readonly string[];
  readonly classes: readonly string[];
}

/** How long each kind of page is given to finish its own tests after load. */
export const DEFAULT_SETTLE_MS: Readonly<Record<SelftestPageKind, number>> = {
  probe: 0,
  echo: 0,
  sannysoft: 4000,
  creepjs: 12_000,
  generic: 3000,
};

/**
 * Above this share CreepJS's "like headless" becomes a warning. The list it
 * is computed from holds entries that are true on every desktop Chrome, so
 * it is never zero; its "headless" and "stealth" numbers are the hard ones.
 */
export const CREEPJS_LIKE_HEADLESS_WARN = 50;

// --- Kage's fingerprint echo ------------------------------------------------

const HeaderPairs = z.array(z.tuple([z.string(), z.string()]));

const EchoAnswerSchema = z.object({
  echo: z.literal('kage-echo'),
  http_version: z.string(),
  tls: z.object({ ja4: z.string() }),
  http2: z.object({ akamai: z.string(), headers: HeaderPairs }).nullable().optional(),
  http1: z.object({ headers: HeaderPairs }).nullable().optional(),
});

/**
 * Judges what the fingerprint echo (`kage echo serve`) saw of the browser:
 * the transport against the recorded capture of the engine's browser major,
 * and the identity headers against the claim.
 *
 * @returns `null` when the text is not an answer of Kage's echo.
 */
export function judgeEcho(
  text: string,
  expected: ExpectedIdentity,
  capture: EchoExpectation | null,
  page: string,
  weight: number,
): Finding[] | null {
  let answer: z.infer<typeof EchoAnswerSchema>;
  try {
    answer = EchoAnswerSchema.parse(JSON.parse(text));
  } catch {
    return null;
  }
  const list = new FindingList(page, weight);
  const headers = answer.http2?.headers ?? answer.http1?.headers ?? [];
  const names = headers.map(([name]) => name.toLowerCase());
  const value = (name: string) => headers.find(([key]) => key.toLowerCase() === name)?.[1] ?? null;

  list.equal('echo.http_version', answer.http_version, 'h2', true);
  if (capture === null) {
    // Nothing to compare with: report what was seen, at no weight.
    list.add('echo.ja4', 'pass', { observed: answer.tls.ja4, weight: 0 });
    list.add('echo.akamai', 'pass', { observed: answer.http2?.akamai ?? null, weight: 0 });
    list.add('echo.header_order', 'pass', { observed: names.join(','), weight: 0 });
  } else {
    list.equal('echo.ja4', answer.tls.ja4, capture.ja4, true);
    list.equal('echo.akamai', answer.http2?.akamai ?? null, capture.akamai, true);
    list.equal('echo.header_order', names.join(','), capture.headers.join(','), true);
  }
  const userAgent = value('user-agent') ?? '';
  list.check(
    'echo.user_agent_os',
    osOfUserAgent(userAgent) === expected.claimed_os,
    true,
    userAgent,
    expected.claimed_os,
  );
  list.check(
    'echo.user_agent_major',
    browserMajorOfUserAgent(userAgent) === expected.browser_major,
    true,
    userAgent,
    String(expected.browser_major),
  );
  const hint = value('sec-ch-ua-platform');
  if (hint !== null) {
    list.check(
      'echo.hint_platform',
      osOfHintPlatform(hint) === expected.claimed_os,
      true,
      hint,
      expected.claimed_os,
    );
  }
  const forced = ['pragma', 'cache-control'].filter((name) => names.includes(name));
  list.check(
    'echo.cache_headers',
    forced.length === 0,
    true,
    forced.join(',') || 'none',
    'no pragma, no cache-control',
  );
  return list.findings;
}

// --- bot.sannysoft.com --------------------------------------------------------

/** Rows whose failure means the browser shows automation, not merely an unusual machine. */
const SANNYSOFT_AUTOMATION =
  /webdriver|chrome|permissions|plugins|phantom|headchr|selenium|sequentum|debug_tools|nightmare|iframe/i;

const SANNYSOFT_CLASSES = { passed: 'pass', failed: 'fail', warn: 'warn' } as const;

/**
 * Judges bot.sannysoft.com from its result tables: each test is a row whose
 * result cell carries the class `passed`, `failed` or `warn`.
 *
 * @returns `null` when no such row exists: the page did not load its tests
 *   or changed its markup.
 */
export function judgeSannysoft(
  rows: readonly TableRow[],
  page: string,
  weight: number,
): Finding[] | null {
  const list = new FindingList(page, weight);
  const seen = new Set<string>();
  for (const row of rows) {
    const index = row.classes.findIndex((value) =>
      value.split(/\s+/).some((name) => name in SANNYSOFT_CLASSES),
    );
    const name = row.cells[0]?.trim().replace(/\s+/g, ' ') ?? '';
    if (index < 0 || name === '' || seen.has(name)) {
      continue;
    }
    seen.add(name);
    const className = (row.classes[index] ?? '')
      .split(/\s+/)
      .find((value): value is keyof typeof SANNYSOFT_CLASSES => value in SANNYSOFT_CLASSES);
    const status = SANNYSOFT_CLASSES[className ?? 'warn'];
    list.add(`sannysoft.${name}`, status, {
      hard: SANNYSOFT_AUTOMATION.test(name),
      observed: (row.cells[index] ?? row.cells.at(-1) ?? '').trim().slice(0, 200),
    });
  }
  return list.findings.length === 0 ? null : list.findings;
}

// --- CreepJS ------------------------------------------------------------------

/**
 * A share CreepJS prints for a label, in either wording: `31% like headless`
 * or `like headless: 31%`. Number and label must be on one line: the number
 * of the line above belongs to another label.
 */
function percent(text: string, label: string): number | null {
  const escaped = label.replace(/\s+/g, '[ \\t]+');
  const before = new RegExp(String.raw`(\d+(?:\.\d+)?)[ \t]*%[ \t]*${escaped}\b`, 'i').exec(text);
  const after = new RegExp(String.raw`\b${escaped}[ \t]*:[ \t]*(\d+(?:\.\d+)?)[ \t]*%`, 'i').exec(
    text,
  );
  const found = before?.[1] ?? after?.[1];
  return found === undefined ? null : Number.parseFloat(found);
}

/**
 * Judges CreepJS from the visible text of its page. It prints three shares:
 * "headless" (automation flags), "stealth" (tampered prototypes) and "like
 * headless" (traits headless browsers share with some real ones).
 *
 * @returns `null` when neither "headless" nor "stealth" can be read.
 */
export function judgeCreepjs(text: string, page: string, weight: number): Finding[] | null {
  // "like headless" contains "headless": take it out before reading the other.
  const likeHeadless = percent(text, 'like headless');
  const withoutLike = text.replace(
    /\d+(?:\.\d+)?[ \t]*%[ \t]*like[ \t]+headless|like[ \t]+headless[ \t]*:[ \t]*\d+(?:\.\d+)?[ \t]*%/gi,
    ' ',
  );
  const headless = percent(withoutLike, 'headless');
  const stealth = percent(withoutLike, 'stealth');
  if (headless === null && stealth === null) {
    return null;
  }
  const list = new FindingList(page, weight);
  const share = (check: string, value: number | null) => {
    if (value === null) {
      list.add(check, 'unknown');
    } else {
      list.check(check, value === 0, true, `${value}%`, '0%');
    }
  };
  share('creepjs.headless', headless);
  share('creepjs.stealth', stealth);
  if (likeHeadless !== null) {
    list.add('creepjs.like_headless', likeHeadless > CREEPJS_LIKE_HEADLESS_WARN ? 'warn' : 'pass', {
      observed: `${likeHeadless}%`,
      expected: `at most ${CREEPJS_LIKE_HEADLESS_WARN}%`,
    });
  }
  const lies = /\blies\s*\((\d+)\)/i.exec(text);
  if (lies?.[1] !== undefined || /\blies\s*:?\s*none\b/i.test(text)) {
    const count = lies?.[1] === undefined ? 0 : Number.parseInt(lies[1], 10);
    list.check('creepjs.lies', count === 0, false, String(count), '0');
  }
  return list.findings;
}

// --- any page, by regular expressions ----------------------------------------

/**
 * Judges any page with the regular expressions of the request. A pattern
 * that is not a valid regular expression gives an `unknown` finding and
 * breaks nothing else.
 */
export function judgeGeneric(
  text: string,
  rules: readonly SelftestRule[],
  page: string,
  weight: number,
): Finding[] {
  const list = new FindingList(page, weight);
  for (const rule of rules) {
    let found: RegExpExecArray | null;
    try {
      found = new RegExp(rule.pattern, 'i').exec(text);
    } catch {
      list.add(rule.check, 'unknown', { observed: 'the pattern is not a regular expression' });
      continue;
    }
    const ok = rule.expect === 'match' ? found !== null : found === null;
    list.check(
      rule.check,
      ok,
      rule.hard,
      found?.[0].slice(0, 200) ?? null,
      `${rule.expect} /${rule.pattern}/i`,
    );
  }
  return list.findings;
}
