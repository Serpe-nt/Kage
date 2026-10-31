import { z } from 'zod';

import { ArtifactRefSchema, HeaderSchema } from './common.js';
import { SeedSchema } from './personas.js';

/** Longest URL the sidecar accepts (spec §16.4). */
export const MAX_URL_LENGTH = 8192;

/** Default of `timeout_ms` (spec §8.4). */
export const DEFAULT_NAVIGATE_TIMEOUT_MS = 60_000;
/** Hard cap of `timeout_ms` (spec §8.4). */
export const MAX_NAVIGATE_TIMEOUT_MS = 300_000;
/** Default of `challenge_wait_ms` (spec §12). */
export const DEFAULT_CHALLENGE_WAIT_MS = 15_000;
/** Default of `max_body_bytes`: 10 MB, the http path's default. */
export const DEFAULT_MAX_BODY_BYTES = 10_000_000;

/** An absolute `http` or `https` URL. */
export const httpUrl = (description: string) =>
  z
    .string()
    .max(MAX_URL_LENGTH)
    .regex(/^https?:\/\/[^\s/?#]+[^\s]*$/i)
    .describe(description);

/** A path below the artifacts volume: relative, forward slashes, no dots-only parts. */
export const artifactPrefix = (description: string) =>
  z
    .string()
    .regex(/^[A-Za-z0-9][A-Za-z0-9._-]*(\/[A-Za-z0-9][A-Za-z0-9._-]*)*$/)
    .max(200)
    .describe(description);

/** When a navigation counts as arrived. */
export const WaitUntilSchema = z.enum(['commit', 'domcontentloaded', 'load', 'network_idle']).meta({
  id: 'WaitUntil',
  description:
    'When a navigation counts as arrived. `commit`: the response started. `domcontentloaded`: the document is parsed. `load`: the load event fired. `network_idle`: load, then no request for 500 ms.',
});
export type WaitUntil = z.infer<typeof WaitUntilSchema>;

/** What to wait for before capturing. */
export const WaitSchema = z
  .strictObject({
    until: WaitUntilSchema.describe('The lifecycle point to wait for.'),
    selector: z
      .string()
      .max(1000)
      .nullable()
      .optional()
      .describe(
        'A CSS selector that must match an attached element afterwards; null or absent for none.',
      ),
    extra_ms: z
      .number()
      .int()
      .min(0)
      .max(60_000)
      .optional()
      .describe('Milliseconds to wait after everything else; default 0.'),
  })
  .meta({ id: 'Wait', description: 'What a navigation waits for before the page is captured.' });
export type Wait = z.infer<typeof WaitSchema>;

/** Image format of a screenshot. */
export const ScreenshotFormatSchema = z.enum(['png', 'jpeg', 'webp']).meta({
  id: 'ScreenshotFormat',
  description: 'Image format of a screenshot.',
});
export type ScreenshotFormat = z.infer<typeof ScreenshotFormatSchema>;

/** Screenshot request. */
export const ScreenshotOptionsSchema = z
  .strictObject({
    full_page: z
      .boolean()
      .describe(
        'False: the visible viewport, which no page can observe. True: the whole document, which fires `resize` in the page (once or twice); the result names that side effect.',
      ),
    format: ScreenshotFormatSchema.describe('Image format.'),
    quality: z
      .number()
      .int()
      .min(1)
      .max(100)
      .optional()
      .describe('Quality for `jpeg` and `webp`; default 80. Ignored for `png`.'),
  })
  .meta({ id: 'ScreenshotOptions', description: 'How a screenshot is taken.' });
export type ScreenshotOptions = z.infer<typeof ScreenshotOptionsSchema>;

/** Paper size of a PDF. */
export const PaperFormatSchema = z.enum(['a4', 'letter']).meta({
  id: 'PaperFormat',
  description: 'Paper size of a PDF.',
});
export type PaperFormat = z.infer<typeof PaperFormatSchema>;

/** PDF request. */
export const PdfOptionsSchema = z
  .strictObject({
    format: PaperFormatSchema.optional().describe('Paper size; default `a4`.'),
    landscape: z.boolean().optional().describe('Landscape orientation; default false.'),
    print_background: z.boolean().optional().describe('Print background graphics; default true.'),
  })
  .meta({
    id: 'PdfOptions',
    description:
      'How a PDF is printed. Printing fires `beforeprint` and `afterprint` in the page, as printing by hand does; the result names both.',
  });
export type PdfOptions = z.infer<typeof PdfOptionsSchema>;

/** Which XHR and fetch() responses are kept. */
export const XhrCaptureSchema = z
  .strictObject({
    content_types: z
      .array(z.string().min(1).max(100))
      .min(1)
      .max(16)
      .describe(
        'Media types to keep, compared without parameters and case; a value ending in `/*` or `+json` style suffixes is matched as a prefix or suffix. Example: `application/json`.',
      ),
    max_bytes: z
      .number()
      .int()
      .min(1)
      .max(50_000_000)
      .describe('Largest body kept per response; a longer body is cut and flagged.'),
    max_responses: z
      .number()
      .int()
      .min(1)
      .max(500)
      .optional()
      .describe('Most responses kept per page; default 100. Later ones are dropped.'),
  })
  .meta({ id: 'XhrCapture', description: 'Which XHR and fetch() responses a navigation keeps.' });
export type XhrCapture = z.infer<typeof XhrCaptureSchema>;

/** What a navigation captures. */
export const CaptureOptionsSchema = z
  .strictObject({
    dom: z.boolean().describe('The document as HTML after the wait settled.'),
    raw: z.boolean().describe('The body of the main response as the server sent it, decoded.'),
    xhr: XhrCaptureSchema.nullable().describe('XHR and fetch() responses to keep; null for none.'),
    console: z
      .boolean()
      .describe(
        "Log entries the engine can collect without a page noticing. On chromium: what the browser itself logs (failed loads, security and deprecation messages); `console.*` calls and uncaught exceptions are not available there, because reading them needs a debugger domain that pages can detect. On firefox: the page's `console.*` calls and uncaught errors.",
      ),
    screenshot: ScreenshotOptionsSchema.nullable().describe('A screenshot; null for none.'),
    pdf: PdfOptionsSchema.nullable().describe('A PDF; null for none.'),
    links: z
      .boolean()
      .optional()
      .describe(
        'The links of the document, each with whether a reader can see it and whether it stands in what the site repeats on every page. Read in the isolated world, like the document; the page cannot notice. Default false.',
      ),
  })
  .meta({ id: 'CaptureOptions', description: 'What a navigation captures.' });
export type CaptureOptions = z.infer<typeof CaptureOptionsSchema>;

/** How a page is arrived at (spec §10.4). */
export const EntryStrategySchema = z.enum(['none', 'direct', 'link', 'search_engine']).meta({
  id: 'EntryStrategy',
  description:
    "How the target is arrived at. `none` and `direct`: a typed navigation, with no `Referer`, `Sec-Fetch-Site: none` and `Sec-Fetch-User: ?1`; the two names are equal on the wire. `link`: the link to the target is found on `from_url` and clicked; `from_url` is loaded as a typed navigation first or, with `from_open_page`, looked for in the tab. `search_engine`: `from_url` is a results page; the first result that leads to the target's host is clicked. A referer is never forged: it is whatever the browser sends for the click.",
});
export type EntryStrategy = z.infer<typeof EntryStrategySchema>;

/** What happens when the page to start from has no link to the target. */
export const EntryFallbackSchema = z.enum(['stop', 'none']).meta({
  id: 'EntryFallback',
  description:
    'What to do when the link or result is not there. `stop`: load nothing else and say so. `none`: load the target as a typed navigation.',
});
export type EntryFallback = z.infer<typeof EntryFallbackSchema>;

/** The entry of a navigation. */
export const EntrySchema = z
  .strictObject({
    strategy: EntryStrategySchema.describe('The strategy.'),
    from_url: httpUrl('The page to start from.')
      .nullable()
      .optional()
      .describe(
        'The page to start from: required for `link` and `search_engine`, ignored otherwise. For `search_engine` it is the results URL Kage built from its policy template.',
      ),
    fallback: EntryFallbackSchema.optional().describe(
      'What to do without a link to the target; default `stop`.',
    ),
    from_open_page: z
      .boolean()
      .optional()
      .describe(
        "Only with `link`. True: `from_url` is a page the persona has been reading, and nothing is loaded to start from. When it is the page that is open, its link to the target is clicked there. When it is one of the `max_backs` pages before it in the tab, the persona goes back to it with the keyboard first (the browser's own way back: no request when the browser kept the page) and clicks there. When the tab does not hold it, or it holds no such link, `fallback` decides; a link no reader could click there (one that is not rendered, lies under something else, or opens another tab) is none. The report says how many entries back the page was. Default false.",
      ),
    max_backs: z
      .number()
      .int()
      .min(0)
      .max(10)
      .optional()
      .describe(
        'With `from_open_page`: how many entries of the tab the persona goes back at most to find `from_url`; default 0, the open page only.',
      ),
  })
  .meta({ id: 'Entry', description: 'How a navigation arrives at its target.' });
export type Entry = z.infer<typeof EntrySchema>;

/** Where the dwell time of a navigation comes from. */
export const DwellModeSchema = z.enum(['fixed', 'human']).meta({
  id: 'DwellMode',
  description:
    "Where the dwell time comes from. `fixed`: `dwell_ms` of the request. `human`: drawn from the persona's own distribution (log-normal, median about 6 s, at most 45 s), never longer than the budget allows.",
});
export type DwellMode = z.infer<typeof DwellModeSchema>;

/** What kind of visit the behavior of a navigation is (contract 1.5.0). */
export const BehaviorModeSchema = z.enum(['human', 'still']).meta({
  id: 'BehaviorMode',
  description:
    "What kind of visit a navigation is. `human`: the persona at its mouse and keyboard, as the other fields of the behavior say. `still`: the persona's browser loads the page and nothing else: no input at all (no pointer, no wheel, no key), a typed entry, the fixed dwell `dwell_ms`, no scrolling, no wandering, no patience. A page sees of it what it sees of a visitor whose hand rests.",
});
export type BehaviorMode = z.infer<typeof BehaviorModeSchema>;

/** The fields of a behavior, before the rule that ties them to its mode. */
const BehaviorOptionsFields = z.strictObject({
  seed: SeedSchema.optional().describe(
    "Seed of this request. With the persona's own seed it decides every movement; the same pair replays the same behavior. Absent: the sidecar draws one and returns it.",
  ),
  mode: BehaviorModeSchema.optional().describe(
    'What kind of visit this is; default `human`. `still`: a visit that sends no input at all (no pointer, no wheel, no key), arrives by a typed navigation (`entry` absent, `direct` or `none`), dwells for the fixed `dwell_ms`, and neither scrolls, wanders nor waits with patience; a request that also names `scroll_depth`, `patience_ms`, `wander: true`, `dwell: human`, an entry by `link` or `search_engine`, or `from_open_page: true` is refused. Its report has the seed, the profile of the persona, the entry, the dwell, and every count 0. Since 1.5.0.',
  ),
  entry: EntrySchema.optional().describe(
    'How the target is arrived at; default a typed navigation.',
  ),
  scroll_depth: z
    .number()
    .min(0)
    .max(1)
    .optional()
    .describe(
      'Scroll down to this share of the document with the wheel, a flick at a time, resting after each as long as the text in view takes the persona to skim. Absent: no scrolling.',
    ),
  dwell: DwellModeSchema.optional().describe('Where the dwell time comes from; default `fixed`.'),
  wander: z
    .boolean()
    .optional()
    .describe(
      'Move the mouse a little now and then while dwelling, as an idle hand does; default true, and never in a `still` visit.',
    ),
  patience_ms: z
    .number()
    .int()
    .min(1000)
    .max(600_000)
    .optional()
    .describe(
      'How long the persona goes on scrolling this page at most. With it, `timeout_ms` of the request bounds reaching the page only (the navigation, its waits, the challenge wait): the scrolling ends at `scroll_depth` or when the patience is used up, whichever comes first, and the dwell follows in full. Whoever asks draws it anew for every page, so that no two long pages are left after the same time. Absent: the scrolling and the dwell share what `timeout_ms` leaves, as before 1.3.0, and every page too long for that is left at the same moment.',
    ),
});

/** A field of a behavior that a still visit cannot have, and why. */
export interface StillConflict {
  /** Where the field is, below the behavior. */
  readonly path: readonly string[];
  /** Why a still visit refuses it, as a sentence for the log. */
  readonly message: string;
}

/**
 * What a still visit (`mode: still`) is asked for besides itself: every
 * field that asks for input, or for a time that only input fills. Empty for
 * a behavior that is not still, and for a still one that asks for none of
 * it. The schema refuses a request with any; an engine checks again before
 * it loads anything.
 */
export function stillConflicts(behavior: z.infer<typeof BehaviorOptionsFields>): StillConflict[] {
  if (behavior.mode !== 'still') {
    return [];
  }
  const conflicts: StillConflict[] = [];
  const refuse = (path: string[], message: string) => {
    conflicts.push({ path, message });
  };
  if (behavior.scroll_depth !== undefined) {
    refuse(['scroll_depth'], 'a still visit does not scroll');
  }
  if (behavior.patience_ms !== undefined) {
    refuse(['patience_ms'], 'a still visit does not scroll, so it has no patience to spend');
  }
  if (behavior.wander === true) {
    refuse(['wander'], 'a still visit does not move the mouse');
  }
  if (behavior.dwell === 'human') {
    refuse(['dwell'], 'a still visit dwells for the fixed dwell_ms');
  }
  const { entry } = behavior;
  if (entry?.strategy === 'link' || entry?.strategy === 'search_engine') {
    refuse(['entry', 'strategy'], 'a still visit arrives by a typed navigation: direct or none');
  }
  if (entry?.from_open_page === true) {
    refuse(['entry', 'from_open_page'], 'a still visit clicks nothing on the open page');
  }
  return conflicts;
}

/** What the behavior engine does during a navigation (spec §10.4). */
export const BehaviorOptionsSchema = BehaviorOptionsFields.superRefine((behavior, context) => {
  for (const conflict of stillConflicts(behavior)) {
    context.addIssue({ code: 'custom', path: [...conflict.path], message: conflict.message });
  }
}).meta({
  id: 'BehaviorOptions',
  description:
    "What a persona does on a page besides loading it. All input is sent through the mouse and keyboard of the context's own display; a `still` visit sends none and is reported all the same. Absent from a request: no input at all, a bare page load.",
});
export type BehaviorOptions = z.infer<typeof BehaviorOptionsSchema>;

/** Body of `POST /v1/contexts/{id}/navigate`. */
export const NavigateRequestSchema = z
  .strictObject({
    url: httpUrl(
      'The page to load. `http` and `https` only. There is no referer option: a referer set by the driver leaves with `Sec-Fetch-Site: none`, a pair no real navigation has. Arriving from another page is `behavior.entry`: a link there is clicked.',
    ),
    wait: WaitSchema.describe('What to wait for.'),
    capture: CaptureOptionsSchema.describe('What to capture.'),
    timeout_ms: z
      .number()
      .int()
      .min(1000)
      .max(MAX_NAVIGATE_TIMEOUT_MS)
      .optional()
      .describe(
        'Budget for the navigation and its waits; default 60000. Without `behavior.patience_ms` it also bounds the behavior on the page.',
      ),
    challenge_wait_ms: z
      .number()
      .int()
      .min(0)
      .max(120_000)
      .optional()
      .describe('How long a detected interstitial is given to resolve by itself; default 15000.'),
    dwell_ms: z
      .number()
      .int()
      .min(0)
      .max(120_000)
      .optional()
      .describe(
        'Time spent on the page after the wait and before the capture; default 0. Ignored when `behavior.dwell` is `human`.',
      ),
    max_body_bytes: z
      .number()
      .int()
      .min(1)
      .max(100_000_000)
      .optional()
      .describe(
        'Largest raw body and DOM kept; default 10000000. A longer one is cut and flagged.',
      ),
    artifact_prefix: artifactPrefix(
      'Directory below the artifacts volume for the files of this navigation; default `sidecar/<context id>`.',
    ).optional(),
    behavior: BehaviorOptionsSchema.optional().describe(
      'What the persona does on the page; absent for a bare page load.',
    ),
    tab: z
      .string()
      .min(1)
      .max(128)
      .optional()
      .describe(
        "The tab the navigation runs in, named by whoever knows which pages belong together: Kage names a crawl's job, so that the crawl reaches its next page by a click on the page it left open, or on one it goes back to (`behavior.entry.from_open_page`). A navigation that names the tab the context's open tab was last used under continues in it, with its pages and history. One that names another tab, or none, first replaces the open tab by a fresh one: no page before the one it loads, no history, an empty session storage; cookies and local storage are the profile's and stay. Every flow and every self-test starts in a fresh tab and leaves it nobody's. Default: none, a fresh tab.",
      ),
  })
  .meta({ id: 'NavigateRequest', description: 'One page load in a persona context.' });
export type NavigateRequest = z.infer<typeof NavigateRequestSchema>;

/** How `data` of a captured body is written. */
export const BodyEncodingSchema = z.enum(['utf8', 'base64']).meta({
  id: 'BodyEncoding',
  description: 'How a captured body is written: as text, or base64 when it is not valid UTF-8.',
});
export type BodyEncoding = z.infer<typeof BodyEncodingSchema>;

/** A response body. */
export const CapturedBodySchema = z
  .strictObject({
    bytes: z.number().int().nonnegative().describe('Size of the whole decoded body in bytes.'),
    truncated: z.boolean().describe('Whether `data` holds less than the whole body.'),
    encoding: BodyEncodingSchema.describe('How `data` is written.'),
    data: z.string().describe('The body, or its first part when truncated.'),
  })
  .meta({ id: 'CapturedBody', description: 'A captured response body.' });
export type CapturedBody = z.infer<typeof CapturedBodySchema>;

/** One hop of the main navigation's redirect chain. */
export const RedirectHopSchema = z
  .strictObject({
    url: z.string().describe('The URL that was requested.'),
    status: z.number().int().describe('The redirect status it answered with.'),
    location: z.string().nullable().describe('Its `Location` header; null when it had none.'),
  })
  .meta({ id: 'RedirectHop', description: 'One answered redirect of the main navigation.' });
export type RedirectHop = z.infer<typeof RedirectHopSchema>;

/** How a script asked for a resource. */
export const XhrKindSchema = z.enum(['xhr', 'fetch']).meta({
  id: 'XhrKind',
  description: 'How a script asked: `XMLHttpRequest` or `fetch()`.',
});
export type XhrKind = z.infer<typeof XhrKindSchema>;

/** One captured XHR or fetch() response. */
export const XhrResponseSchema = z
  .strictObject({
    url: z.string().describe('The request URL.'),
    method: z.string().describe('The request method.'),
    kind: XhrKindSchema.describe('How the page asked.'),
    status: z.number().int().describe('The response status.'),
    content_type: z.string().nullable().describe('The response media type, without parameters.'),
    body: CapturedBodySchema.nullable().describe(
      'The response body; null when the browser no longer held it.',
    ),
  })
  .meta({ id: 'XhrResponse', description: 'One XHR or fetch() response a page received.' });
export type XhrResponse = z.infer<typeof XhrResponseSchema>;

/** Severity of a browser log entry. */
export const ConsoleLevelSchema = z.enum(['error', 'warning', 'info', 'verbose']).meta({
  id: 'ConsoleLevel',
  description: 'Severity of a browser log entry.',
});
export type ConsoleLevel = z.infer<typeof ConsoleLevelSchema>;

/** One browser log entry. */
export const ConsoleEntrySchema = z
  .strictObject({
    level: ConsoleLevelSchema.describe('Severity.'),
    source: z
      .string()
      .describe('Where the browser says it came from: `network`, `security`, `deprecation`, ...'),
    text: z.string().describe('The message.'),
    url: z.string().nullable().describe('The resource it is about; null when unknown.'),
  })
  .meta({ id: 'ConsoleEntry', description: 'One entry the browser itself logged about a page.' });
export type ConsoleEntry = z.infer<typeof ConsoleEntrySchema>;

/** What the `console` list of a page result holds. */
export const ConsoleCaptureSchema = z.enum(['browser_log', 'unsupported', 'not_requested']).meta({
  id: 'ConsoleCapture',
  description:
    "What the console list holds. `browser_log`: entries the engine collected without the page noticing (chromium: the browser's own log; firefox: console calls and uncaught errors of the page). `unsupported`: the engine cannot read any log without becoming detectable. `not_requested`: the request did not ask.",
});
export type ConsoleCapture = z.infer<typeof ConsoleCaptureSchema>;

/** Milestones of the main navigation, from the moment it was started. */
export const TimingSchema = z
  .strictObject({
    ttfb_ms: z.number().int().nonnegative().nullable().describe('Until the first response byte.'),
    dom_content_loaded_ms: z
      .number()
      .int()
      .nonnegative()
      .nullable()
      .describe('Until the document was parsed.'),
    load_ms: z.number().int().nonnegative().nullable().describe('Until the load event.'),
    total_ms: z.number().int().nonnegative().describe('Until every wait and capture was done.'),
  })
  .meta({
    id: 'Timing',
    description: 'Milestones of a navigation in milliseconds; null for one that was not reached.',
  });
export type Timing = z.infer<typeof TimingSchema>;

/** What a page shows of bot-protection vendors. The classifier in Kage decides. */
export const ChallengeSignalsSchema = z
  .strictObject({
    vendors: z
      .array(z.string())
      .describe('Vendors whose scripts, frames or markers were seen, for example `cloudflare`.'),
    script_hosts: z.array(z.string()).describe('Hosts of every script the page requested, sorted.'),
    iframe_hosts: z.array(z.string()).describe('Hosts of every frame the page embedded, sorted.'),
    interstitial: z
      .boolean()
      .describe('Whether the first document looked like a challenge page and not like content.'),
    marker: z
      .string()
      .nullable()
      .describe('The marker that identified the interstitial; null without one.'),
    resolved: z
      .boolean()
      .nullable()
      .describe(
        'Whether the interstitial went away by itself within the wait; null when there was none.',
      ),
    waited_ms: z.number().int().nonnegative().describe('How long the interstitial was waited for.'),
  })
  .meta({
    id: 'ChallengeSignals',
    description: 'What a page shows of bot-protection vendors. Signals only; Kage classifies.',
  });
export type ChallengeSignals = z.infer<typeof ChallengeSignalsSchema>;

/** Why a navigation did not arrive. */
export const NavigationErrorCodeSchema = z
  .enum(['timeout', 'navigation_failed', 'proxy_failed', 'aborted'])
  .meta({
    id: 'NavigationErrorCode',
    description:
      'Why a navigation did not arrive. `timeout`: the budget ran out. `proxy_failed`: the proxy refused, failed or could not be reached. `navigation_failed`: the browser reported a network error for the target. `aborted`: the page turned into a download or was closed, or the entry did not find its link (`behavior.entry.outcome` says so).',
  });
export type NavigationErrorCode = z.infer<typeof NavigationErrorCodeSchema>;

/** A navigation that did not arrive. Whatever was captured before is still returned. */
export const NavigationErrorSchema = z
  .strictObject({
    code: NavigationErrorCodeSchema.describe('What kind of failure.'),
    message: z.string().describe('A sentence for the log.'),
    net_error: z
      .string()
      .nullable()
      .describe(
        "The browser's own error name, for example `ERR_NAME_NOT_RESOLVED`; null without one.",
      ),
  })
  .meta({ id: 'NavigationError', description: 'Why a navigation did not arrive.' });
export type NavigationError = z.infer<typeof NavigationErrorSchema>;

/** Something a capture did that a script of the page could have noticed. */
export const SideEffectSchema = z.enum(['resize', 'beforeprint', 'afterprint']).meta({
  id: 'SideEffect',
  description:
    'An event a capture fired in the page. `resize`: a full-page screenshot. `beforeprint` and `afterprint`: a PDF.',
});
export type SideEffect = z.infer<typeof SideEffectSchema>;

/** How an entry ended. */
export const EntryOutcomeSchema = z
  .enum(['arrived', 'fell_back', 'link_not_found', 'result_not_found', 'blocked'])
  .meta({
    id: 'EntryOutcome',
    description:
      'How an entry ended. `arrived`: the target was reached the way the strategy says. `fell_back`: no link was there and the target was loaded as a typed navigation instead. `link_not_found`, `result_not_found`: no link or result led to the target and nothing else was loaded. `blocked`: the page to start from did not load or showed a wall; nothing was typed into it.',
  });
export type EntryOutcome = z.infer<typeof EntryOutcomeSchema>;

/** What the entry of a navigation did. */
export const EntryReportSchema = z
  .strictObject({
    strategy: EntryStrategySchema.describe('The strategy that was asked for.'),
    outcome: EntryOutcomeSchema.describe('How it ended.'),
    from_url: z
      .string()
      .nullable()
      .describe('The page the entry started from; null for a typed navigation.'),
    referer: z
      .string()
      .nullable()
      .describe(
        'The `Referer` the browser sent with the request for the target; null when it sent none.',
      ),
    backs: z
      .number()
      .int()
      .nonnegative()
      .nullable()
      .describe(
        'With `from_open_page`: how many entries the persona went back in the tab before it clicked, 0 when the page that was open held the link; null for every other entry, and when the target was not reached by a click.',
      ),
  })
  .meta({ id: 'EntryReport', description: 'How a navigation arrived at its target.' });
export type EntryReport = z.infer<typeof EntryReportSchema>;

const count = (description: string) => z.number().int().nonnegative().describe(description);

/** Why the scrolling of a page ended. */
export const ScrollEndSchema = z.enum(['depth', 'patience', 'budget', 'page']).meta({
  id: 'ScrollEnd',
  description:
    'Why the scrolling of a page ended. `depth`: the depth asked was reached. `patience`: `behavior.patience_ms` was used up first. `budget`: no patience was given and what `timeout_ms` left was used up first. `page`: the page does not scroll, or not any further. A page that grows for as long as it is scrolled ends after 400 turns of the wheel, reported as `patience` or `budget`.',
});
export type ScrollEnd = z.infer<typeof ScrollEndSchema>;

/** What the behavior engine did (spec §10.4). */
export const BehaviorReportSchema = z
  .strictObject({
    seed: SeedSchema.describe(
      'The seed of this request: the one given, or the one the sidecar drew.',
    ),
    profile_id: z
      .string()
      .regex(/^[0-9a-f]{16}$/)
      .describe(
        'Identifies the motor profile of the persona: the same for every request of one persona.',
      ),
    entry: EntryReportSchema.nullable().describe(
      'What the entry did; null when the request named none.',
    ),
    dwell_ms: count(
      'Time spent dwelling: on the page of a navigation, or on all the pages a paced flow arrived on together.',
    ),
    moves: count('Movements of the pointer.'),
    positions: count('Positions the mouse reported during them.'),
    overshoots: count('Movements that went past their target and came back.'),
    clicks: count('Clicks.'),
    notches: count('Notches of the wheel.'),
    keys: count('Keys pressed, modifiers and corrections included.'),
    typos: count('Characters typed wrong and corrected.'),
    wanders: count('Idle movements while dwelling.'),
    patience_ms: z
      .number()
      .int()
      .nonnegative()
      .nullable()
      .describe('The patience the request gave the scrolling; null when it gave none.'),
    scroll_reached: z
      .number()
      .min(0)
      .max(1)
      .nullable()
      .describe(
        'How far down the document the page was when the scrolling of a navigation ended, as a share of its scrollable height: 0 at the top, 1 at the end, and 1 for a document that ends inside its window; null when no scrolling was asked for, and for a flow.',
      ),
    scroll_ended_by: ScrollEndSchema.nullable().describe(
      'Why that scrolling ended; null when no scrolling was asked for, and for a flow.',
    ),
  })
  .meta({
    id: 'BehaviorReport',
    description:
      'What the behavior engine did during a request. With the persona and this seed the behavior can be replayed.',
  });
export type BehaviorReport = z.infer<typeof BehaviorReportSchema>;

/** One link of a document. */
export const PageLinkSchema = z
  .strictObject({
    url: z.string().describe('Where it leads: the absolute address the browser resolved.'),
    text: z
      .string()
      .describe('The text of the link, white space collapsed, at most 200 characters.'),
    rel: z
      .array(z.string())
      .describe(
        'The tokens of its `rel` attribute, in lower case: the first 16, each cut at 64 characters.',
      ),
    visible: z
      .boolean()
      .describe(
        'Whether a reader can see it: it is rendered (not `display: none`, not `visibility: hidden`, not fully transparent, nor inside anything that is), has a box, and that box lies on the canvas of the document. A link that is not is one nobody can click.',
      ),
    furniture: z
      .boolean()
      .describe(
        'Whether it stands in what a site repeats on every page: inside `nav`, `header`, `footer` or `aside`, or an element with the ARIA role of one.',
      ),
  })
  .meta({ id: 'PageLink', description: 'One link of a document, as a reader meets it.' });
export type PageLink = z.infer<typeof PageLinkSchema>;

/** Most links a page result holds. */
export const MAX_PAGE_LINKS = 5000;

/** Result of one page load (spec §10.2). */
export const PageResultSchema = z
  .strictObject({
    url: z.string().describe('The URL that was asked for.'),
    final_url: z.string().describe('The URL of the document when it was captured.'),
    status: z
      .number()
      .int()
      .nullable()
      .describe('Status of the main response; null when there was none.'),
    http_version: z
      .string()
      .nullable()
      .describe('Protocol of the main response as the browser names it, for example `h2`.'),
    headers: z
      .array(HeaderSchema)
      .describe('Headers of the main response, in order, names in lower case.'),
    redirects: z
      .array(RedirectHopSchema)
      .describe(
        'The redirects that were answered before the main response, in order. Kage checks every hop against its SSRF rules afterwards.',
      ),
    title: z.string().nullable().describe('The document title; null when nothing was loaded.'),
    dom_html: z
      .string()
      .nullable()
      .describe(
        'The document as HTML after the wait settled; null when not requested or not loaded.',
      ),
    dom_truncated: z.boolean().describe('Whether `dom_html` was cut at `max_body_bytes`.'),
    raw_body: CapturedBodySchema.nullable().describe(
      'The body of the main response; null when not requested or not available.',
    ),
    xhr: z
      .array(XhrResponseSchema)
      .describe("The XHR and fetch() responses that matched the request's filter."),
    xhr_dropped: z
      .number()
      .int()
      .nonnegative()
      .describe(
        'Matching responses that were not kept because the page had more than `max_responses`.',
      ),
    console: z.array(ConsoleEntrySchema).describe('Browser log entries, when requested.'),
    console_capture: ConsoleCaptureSchema.describe('What `console` holds.'),
    timing: TimingSchema.describe('Milestones of the navigation.'),
    challenge: ChallengeSignalsSchema.describe('Bot-protection signals.'),
    screenshot: ArtifactRefSchema.nullable().describe('The screenshot file; null without one.'),
    pdf: ArtifactRefSchema.nullable().describe('The PDF file; null without one.'),
    side_effects: z
      .array(SideEffectSchema)
      .describe(
        'Events the captures fired in the page, in order. Empty when the page could notice nothing.',
      ),
    error: NavigationErrorSchema.nullable().describe(
      'Why the navigation did not arrive; null when it did. Whatever was captured is still present.',
    ),
    behavior: BehaviorReportSchema.nullable().describe(
      'What the behavior engine did; null when the request asked for no behavior.',
    ),
    links: z
      .array(PageLinkSchema)
      .max(MAX_PAGE_LINKS)
      .nullable()
      .describe(
        'The links of the document to `http` and `https` addresses, in document order, a link that occurs twice listed twice; an address longer than a navigation takes (8192 characters) is left out. Null when not requested or nothing was loaded.',
      ),
    links_truncated: z
      .boolean()
      .describe('Whether the document held more links than `links` lists.'),
  })
  .meta({ id: 'PageResult', description: 'What one page load produced.' });
export type PageResult = z.infer<typeof PageResultSchema>;
