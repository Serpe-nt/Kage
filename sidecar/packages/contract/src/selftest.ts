import { z } from 'zod';

import { ArtifactRefSchema, ClaimedOsSchema, languageTag, timestamp } from './common.js';
import { artifactPrefix, httpUrl } from './navigate.js';

/** Default of `min_score`. */
export const DEFAULT_MIN_SCORE = 90;

/**
 * How a self-test page is read.
 *
 * `probe` and `echo` are Kage's own pages. `sannysoft` and `creepjs` are
 * third parties: they are loaded on demand and in the gated live suite only,
 * never on a timer. `generic` reads any page with regular expressions from
 * the request.
 */
export const SelftestPageKindSchema = z
  .enum(['probe', 'echo', 'sannysoft', 'creepjs', 'generic'])
  .meta({
    id: 'SelftestPageKind',
    description:
      "How a self-test page is read. `probe`: Kage's coherence probe (it publishes its findings as JSON). `echo`: Kage's fingerprint echo (TLS, HTTP/2 and header order). `sannysoft`: bot.sannysoft.com. `creepjs`: CreepJS. `generic`: any page, read with the regular expressions of the request.",
  });
export type SelftestPageKind = z.infer<typeof SelftestPageKindSchema>;

/** Whether a rule wants its pattern to be found or to be absent. */
export const RuleExpectationSchema = z.enum(['match', 'no_match']).meta({
  id: 'RuleExpectation',
  description:
    'Whether a rule passes when its pattern is found in the page text or when it is not.',
});

/** One regular-expression check of a `generic` page. */
export const SelftestRuleSchema = z
  .strictObject({
    check: z.string().min(1).max(100).describe('Name of the finding this rule produces.'),
    pattern: z
      .string()
      .min(1)
      .max(500)
      .describe(
        'A regular expression, matched case-insensitively against the visible text of the page.',
      ),
    expect: RuleExpectationSchema.describe('Whether the pattern must be found or must be absent.'),
    hard: z.boolean().describe('Whether a failure of this rule fails the whole self-test.'),
  })
  .meta({ id: 'SelftestRule', description: 'One regular-expression check of a `generic` page.' });
export type SelftestRule = z.infer<typeof SelftestRuleSchema>;

/** One page of a self-test. */
export const SelftestPageSchema = z
  .strictObject({
    id: z
      .string()
      .regex(/^[a-z0-9][a-z0-9_-]*$/)
      .max(40)
      .describe('Short name of the page; findings and artifacts carry it.'),
    url: httpUrl('The page to load.'),
    kind: SelftestPageKindSchema.describe('How the page is read.'),
    settle_ms: z
      .number()
      .int()
      .min(0)
      .max(120_000)
      .optional()
      .describe(
        'How long the page is given to finish its own tests after load; default depends on the kind.',
      ),
    weight: z
      .number()
      .min(0)
      .max(100)
      .optional()
      .describe('Weight of each finding of this page in the score; default 1.'),
    rules: z
      .array(SelftestRuleSchema)
      .max(50)
      .optional()
      .describe('The checks of a `generic` page. Ignored for the other kinds.'),
  })
  .meta({ id: 'SelftestPage', description: 'One page a self-test loads.' });
export type SelftestPage = z.infer<typeof SelftestPageSchema>;

/** What the persona claims to be; the findings are judged against it. */
export const ExpectedIdentitySchema = z
  .strictObject({
    claimed_os: ClaimedOsSchema.describe('The operating system the persona claims.'),
    browser_major: z.number().int().positive().describe('The browser major the persona presents.'),
    timezone: z.string().max(64).describe("The persona's IANA time zone."),
    languages: z
      .array(languageTag('A BCP 47 language tag.'))
      .min(1)
      .max(8)
      .describe("The persona's languages, most preferred first."),
  })
  .meta({ id: 'ExpectedIdentity', description: 'The identity a self-test expects to see.' });
export type ExpectedIdentity = z.infer<typeof ExpectedIdentitySchema>;

/** The recorded capture of the engine's browser against the echo. */
export const EchoExpectationSchema = z
  .strictObject({
    ja4: z.string().describe("The JA4 of the browser's TLS ClientHello."),
    akamai: z.string().describe("The HTTP/2 fingerprint in Akamai's notation."),
    headers: z.array(z.string()).describe('The header names of a typed navigation, in wire order.'),
  })
  .meta({
    id: 'EchoExpectation',
    description:
      'What the fingerprint echo must report for this browser: the capture recorded in `tests/fixtures/fingerprints/browser/`.',
  });
export type EchoExpectation = z.infer<typeof EchoExpectationSchema>;

/** Body of `POST /v1/contexts/{id}/selftest`. */
export const SelftestRequestSchema = z
  .strictObject({
    pages: z
      .array(SelftestPageSchema)
      .min(1)
      .max(12)
      .optional()
      .describe(
        "The pages to load; default: the sidecar's configured set, which holds Kage's own pages only. Third-party pages are only ever loaded because a request names them.",
      ),
    expected: ExpectedIdentitySchema.describe('The identity the persona claims.'),
    echo: EchoExpectationSchema.nullable()
      .optional()
      .describe(
        'What an `echo` page must report; without it an echo page only reports what it saw.',
      ),
    min_score: z
      .number()
      .min(0)
      .max(100)
      .optional()
      .describe('The lowest score that passes; default 90.'),
    timeout_ms: z
      .number()
      .int()
      .min(1000)
      .max(300_000)
      .optional()
      .describe('Budget per page; default 45000.'),
    artifact_prefix: artifactPrefix(
      'Directory below the artifacts volume for the evidence files; default `sidecar/<context id>/selftest`.',
    ).optional(),
  })
  .meta({ id: 'SelftestRequest', description: 'A self-test of one persona context.' });
export type SelftestRequest = z.infer<typeof SelftestRequestSchema>;

/** Verdict of one check. */
export const FindingStatusSchema = z.enum(['pass', 'warn', 'fail', 'unknown']).meta({
  id: 'FindingStatus',
  description:
    'Verdict of one check. `unknown`: the page could not be reached or read; it changes no score.',
});
export type FindingStatus = z.infer<typeof FindingStatusSchema>;

/** One check of a self-test. */
export const FindingSchema = z
  .strictObject({
    page: z.string().describe('The id of the page the check ran on.'),
    check: z.string().describe('Name of the check, stable per page kind.'),
    status: FindingStatusSchema.describe('The verdict.'),
    hard: z
      .boolean()
      .describe(
        'Whether a `fail` here fails the whole self-test: an automation flag, an identity mismatch or a transport mismatch.',
      ),
    weight: z.number().nonnegative().describe('Weight of the check in the score.'),
    observed: z.string().nullable().describe('What the page showed; null when nothing was read.'),
    expected: z
      .string()
      .nullable()
      .describe('What was expected; null when the check has no fixed expectation.'),
  })
  .meta({ id: 'Finding', description: 'One check of a self-test and its verdict.' });
export type Finding = z.infer<typeof FindingSchema>;

/** Whether a self-test page could be used. */
export const SelftestPageStatusSchema = z.enum(['ok', 'unreachable', 'unparsed']).meta({
  id: 'SelftestPageStatus',
  description:
    'Whether a page could be used. `unreachable`: it did not load. `unparsed`: it loaded but did not look as its kind expects.',
});

/** What happened with one page of a self-test. */
export const SelftestPageReportSchema = z
  .strictObject({
    id: z.string().describe('The page id.'),
    url: z.string().describe('The page URL.'),
    kind: SelftestPageKindSchema.describe('How the page was read.'),
    status: SelftestPageStatusSchema.describe('Whether it could be used.'),
    http_status: z
      .number()
      .int()
      .nullable()
      .describe("Status of the page's main response; null without one."),
    duration_ms: z.number().int().nonnegative().describe('How long the page took.'),
    text: ArtifactRefSchema.nullable().describe(
      'The visible text of the page as a file, as evidence.',
    ),
    screenshot: ArtifactRefSchema.nullable().describe('A screenshot of the page, as evidence.'),
  })
  .meta({ id: 'SelftestPageReport', description: 'What happened with one page of a self-test.' });
export type SelftestPageReport = z.infer<typeof SelftestPageReportSchema>;

/** Response of `POST /v1/contexts/{id}/selftest`. */
export const SelftestResponseSchema = z
  .strictObject({
    passed: z.boolean().describe('No hard failure, and the score is at least `min_score`.'),
    complete: z
      .boolean()
      .describe(
        'False when a page could not be reached or read: the verdict then rests on fewer checks.',
      ),
    score: z
      .number()
      .min(0)
      .max(100)
      .describe(
        '100 × (1 − (weight of fails + half the weight of warns) / weight of all known checks); 100 when nothing is known.',
      ),
    min_score: z.number().describe('The threshold that was applied.'),
    findings: z.array(FindingSchema).describe('Every check, in page order.'),
    pages: z.array(SelftestPageReportSchema).describe('Every page, in request order.'),
    started_at: timestamp('When the self-test started.'),
    finished_at: timestamp('When it finished.'),
  })
  .meta({ id: 'SelftestResponse', description: 'Findings and score of a self-test.' });
export type SelftestResponse = z.infer<typeof SelftestResponseSchema>;
