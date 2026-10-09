import type { ChallengeSignals } from '@kage/contract';

import { hostOf } from './capture.js';

/**
 * Bot-protection signals of a page (spec §10.2, §12).
 *
 * The sidecar reports what it saw; the classifier in Kage, with its
 * signature file, decides what it means. The tables here only need to be
 * good enough to tell "this document is a challenge page, wait for it" from
 * "this is content", and to name the vendors whose code is on the page.
 */

interface VendorRule {
  readonly vendor: string;
  /** Matched against the full URL of every script and frame. */
  readonly urls: readonly RegExp[];
}

const VENDOR_RULES: readonly VendorRule[] = [
  {
    vendor: 'cloudflare',
    urls: [/\/cdn-cgi\/challenge-platform\//, /^https?:\/\/challenges\.cloudflare\.com\//],
  },
  {
    vendor: 'datadome',
    urls: [/^https?:\/\/([a-z0-9-]+\.)?captcha-delivery\.com\//, /^https?:\/\/js\.datadome\.co\//],
  },
  {
    vendor: 'perimeterx',
    urls: [
      /^https?:\/\/([a-z0-9-]+\.)?(perimeterx\.net|px-cdn\.net|px-cloud\.net|pxchk\.net)\//,
      /\/[A-Za-z0-9]{8}\/init\.js(\?|$)/,
    ],
  },
  { vendor: 'akamai', urls: [/\/akam\/\d+\//, /\/_sec\/cp_challenge\//] },
  { vendor: 'kasada', urls: [/\/149e9513-01fa-4fb0-aad4-566afd725d1b\//, /\/ips\.js(\?|$)/] },
  { vendor: 'imperva', urls: [/\/_Incapsula_Resource(\?|$)/] },
  { vendor: 'aws_waf', urls: [/^https?:\/\/[a-z0-9.-]+\.awswaf\.com\//] },
  {
    vendor: 'recaptcha',
    urls: [
      /^https?:\/\/www\.(google\.com|recaptcha\.net)\/recaptcha\//,
      /^https?:\/\/www\.gstatic\.com\/recaptcha\//,
    ],
  },
  { vendor: 'hcaptcha', urls: [/^https?:\/\/([a-z0-9-]+\.)?hcaptcha\.com\//] },
];

interface InterstitialRule {
  /** `vendor:what`, reported as the marker. */
  readonly marker: string;
  readonly title?: RegExp;
  readonly text?: RegExp;
  /** A frame from this vendor is the challenge itself. */
  readonly frameVendor?: string;
}

const INTERSTITIAL_RULES: readonly InterstitialRule[] = [
  { marker: 'cloudflare:just-a-moment', title: /^just a moment\b/i },
  { marker: 'cloudflare:attention-required', title: /^attention required!? \| cloudflare/i },
  {
    marker: 'cloudflare:checking-browser',
    text: /checking (if the site connection is secure|your browser before accessing)|verify(ing)? you are human/i,
  },
  {
    marker: 'akamai:access-denied',
    title: /^access denied$/i,
    text: /reference\s*#\s*[0-9a-f.]+/i,
  },
  { marker: 'perimeterx:press-and-hold', text: /press\s*&\s*hold/i },
  { marker: 'perimeterx:access-denied', title: /^access to this page has been denied/i },
  { marker: 'imperva:incident', text: /incapsula incident id|pardon our interruption/i },
  { marker: 'datadome:captcha', frameVendor: 'datadome' },
];

/** The longest page text the interstitial rules look at. */
export const INTERSTITIAL_TEXT_LIMIT = 4000;

const sortedUnique = (values: readonly (string | null)[]): string[] =>
  [...new Set(values.filter((value): value is string => value !== null))].sort();

function vendorOf(url: string): string | null {
  return (
    VENDOR_RULES.find((rule) => rule.urls.some((pattern) => pattern.test(url)))?.vendor ?? null
  );
}

/** What a page showed when it was looked at. */
export interface PageObservation {
  /** The document title. */
  readonly title: string | null;
  /** The visible text of the document, or its beginning. */
  readonly text: string | null;
  /** URL of every script the page requested. */
  readonly scriptUrls: readonly string[];
  /** URL of every frame the page embeds, without the main frame. */
  readonly frameUrls: readonly string[];
}

/**
 * The marker of the interstitial a page shows, or `null` when it looks like
 * content. A challenge page is short: only the beginning of a long text is
 * examined, so that an article about bot protection is not taken for one.
 */
export function findInterstitial(observation: PageObservation): string | null {
  const title = observation.title?.trim() ?? '';
  const text = observation.text ?? '';
  const short = text.length <= INTERSTITIAL_TEXT_LIMIT;
  const frameVendors = new Set(observation.frameUrls.map(vendorOf));
  for (const rule of INTERSTITIAL_RULES) {
    if (rule.frameVendor !== undefined) {
      if (frameVendors.has(rule.frameVendor) && short) {
        return rule.marker;
      }
      continue;
    }
    const titleMatches = rule.title === undefined || rule.title.test(title);
    const textMatches = rule.text === undefined || (short && rule.text.test(text));
    if (titleMatches && textMatches) {
      return rule.marker;
    }
  }
  return null;
}

/** How the wait for an interstitial ended. */
export interface ChallengeWait {
  /** The marker found on the first look; `null` when the page was content. */
  readonly marker: string | null;
  /** Whether a later look found no interstitial; `null` when there was none to begin with. */
  readonly resolved: boolean | null;
  readonly waitedMs: number;
}

/** Puts the observation of a page and the outcome of the wait into contract form. */
export function challengeSignals(
  observation: PageObservation,
  wait: ChallengeWait,
): ChallengeSignals {
  const urls = [...observation.scriptUrls, ...observation.frameUrls];
  const vendors = sortedUnique([...urls.map(vendorOf), wait.marker?.split(':')[0] ?? null]);
  return {
    vendors,
    script_hosts: sortedUnique(observation.scriptUrls.map(hostOf)),
    iframe_hosts: sortedUnique(observation.frameUrls.map(hostOf)),
    interstitial: wait.marker !== null,
    marker: wait.marker,
    resolved: wait.resolved,
    waited_ms: wait.waitedMs,
  };
}

/**
 * Waits for an interstitial to resolve by itself (spec §12): looks at the
 * page, and while it shows a challenge, looks again every `intervalMs` until
 * `budgetMs` is spent.
 *
 * @param look reads the page; it is called at least once.
 * @param sleep waits; injectable so tests run without a clock.
 */
export async function waitForChallenge(
  look: () => Promise<PageObservation>,
  budgetMs: number,
  sleep: (ms: number) => Promise<void>,
  intervalMs = 500,
): Promise<{ observation: PageObservation; wait: ChallengeWait }> {
  let observation = await look();
  const marker = findInterstitial(observation);
  if (marker === null) {
    return { observation, wait: { marker: null, resolved: null, waitedMs: 0 } };
  }
  let waitedMs = 0;
  while (waitedMs < budgetMs) {
    const step = Math.min(intervalMs, budgetMs - waitedMs);
    await sleep(step);
    waitedMs += step;
    observation = await look();
    if (findInterstitial(observation) === null) {
      return { observation, wait: { marker, resolved: true, waitedMs } };
    }
  }
  return { observation, wait: { marker, resolved: false, waitedMs } };
}
