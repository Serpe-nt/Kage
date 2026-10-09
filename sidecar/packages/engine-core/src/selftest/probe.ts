import type { ExpectedIdentity, Finding } from '@kage/contract';
import { z } from 'zod';

import {
  browserMajorOfUserAgent,
  majorOfVersion,
  osOfHintPlatform,
  osOfPlatform,
  osOfUserAgent,
} from '../identity.js';

import { FindingList } from './findings.js';

/**
 * Kage's coherence probe (spec §6.2).
 *
 * The probe page (`tests/fixtures/site/probe/`) only observes: it reads the
 * same values in the window, three kinds of iframes and two kinds of
 * workers, and publishes them as JSON. The judging happens here, against
 * what the persona claims, so the page holds no expectation that a site
 * could read.
 */

const text = z.string().nullable().catch(null);

const HighEntropySchema = z.object({
  platform: text,
  platform_version: text,
  architecture: text,
  bitness: text,
  ua_full_version: text,
});

const ScopeSchema = z.object({
  user_agent: text,
  platform: text,
  languages: z.array(z.string()).nullable().catch(null),
  hardware_concurrency: z.number().nullable().catch(null),
  device_memory: z.number().nullable().catch(null),
  timezone: text,
  uad_platform: text,
  high_entropy: HighEntropySchema.nullable().catch(null),
  webgl: z.object({ vendor: text, renderer: text }).nullable().catch(null),
  error: text,
});
export type ProbeScope = z.infer<typeof ScopeSchema>;

const RequestHeadersSchema = z
  .array(z.tuple([z.string(), z.string()]))
  .nullable()
  .catch(null);

/** What the probe page publishes. */
export const ProbeReportSchema = z.object({
  kage_probe: z.literal(1),
  scopes: z.object({
    window: ScopeSchema,
    iframe_same_origin: ScopeSchema.nullable().catch(null),
    iframe_srcdoc: ScopeSchema.nullable().catch(null),
    iframe_blank: ScopeSchema.nullable().catch(null),
    worker: ScopeSchema.nullable().catch(null),
    blob_worker: ScopeSchema.nullable().catch(null),
  }),
  automation: z.object({
    webdriver: z.boolean().nullable().catch(null),
    /** How often the browser read the `stack` of an error that was only logged. */
    console_stack_reads: z.number(),
    getters_native: z.boolean(),
    illegal_invocation_throws: z.boolean(),
    navigator_own_properties: z.array(z.string()),
  }),
  /**
   * Request headers as the server saw them, in order; `null` where the host
   * of the probe cannot tell (a static host tells nothing).
   */
  requests: z.object({
    navigation: RequestHeadersSchema,
    iframe: RequestHeadersSchema,
    fetch: RequestHeadersSchema,
    worker_script: RequestHeadersSchema,
    worker_fetch: RequestHeadersSchema,
  }),
});
export type ProbeReport = z.infer<typeof ProbeReportSchema>;

/** The scopes of a report, in a fixed order. */
export const PROBE_SCOPES = [
  'window',
  'iframe_same_origin',
  'iframe_srcdoc',
  'iframe_blank',
  'worker',
  'blob_worker',
] as const;
export type ProbeScopeName = (typeof PROBE_SCOPES)[number];

/** Reads the JSON the probe page published; `null` when it is not a probe report. */
export function parseProbeReport(json: string): ProbeReport | null {
  try {
    const parsed = ProbeReportSchema.safeParse(JSON.parse(json));
    return parsed.success ? parsed.data : null;
  } catch {
    return null;
  }
}

const header = (headers: readonly (readonly [string, string])[], name: string): string | null =>
  headers.find(([key]) => key.toLowerCase() === name)?.[1] ?? null;

function agreement(
  list: FindingList,
  check: string,
  scopes: readonly (readonly [ProbeScopeName, ProbeScope])[],
  read: (scope: ProbeScope) => string | null,
): void {
  const values = scopes
    .map(([name, scope]) => [name, read(scope)] as const)
    .filter((entry): entry is readonly [ProbeScopeName, string] => entry[1] !== null);
  const distinct = new Set(values.map(([, value]) => value));
  if (values.length < 2) {
    return;
  }
  list.check(
    check,
    distinct.size === 1,
    true,
    distinct.size === 1
      ? (values[0]?.[1] ?? null)
      : values.map(([name, value]) => `${name}=${value}`).join(' | '),
    'the same value in every scope',
  );
}

/**
 * Judges a probe report against the identity a persona claims.
 *
 * Hard failures are the ones spec §6.2 names: an automation flag, a scope
 * whose operating system, browser major, time zone or languages are not the
 * claimed ones, scopes that disagree with each other, and request headers
 * that disagree with the window. A scope that did not report is a warning:
 * a page may forbid workers, and that is not the persona's fault.
 */
export function judgeProbe(
  report: ProbeReport,
  expected: ExpectedIdentity,
  page: string,
  weight: number,
): Finding[] {
  const list = new FindingList(page, weight);
  const { automation, requests } = report;

  list.check(
    'automation.webdriver',
    automation.webdriver === false,
    true,
    String(automation.webdriver),
    'false',
  );
  list.check(
    'automation.console_getter',
    automation.console_stack_reads === 0,
    true,
    String(automation.console_stack_reads),
    '0',
  );
  const native =
    automation.getters_native &&
    automation.illegal_invocation_throws &&
    automation.navigator_own_properties.length === 0;
  list.check(
    'automation.native_getters',
    native,
    true,
    `getters_native=${automation.getters_native} illegal_invocation_throws=${automation.illegal_invocation_throws} own=[${automation.navigator_own_properties.join(',')}]`,
    'native getters, no own properties on navigator',
  );

  const reporting: (readonly [ProbeScopeName, ProbeScope])[] = [];
  for (const name of PROBE_SCOPES) {
    const scope = report.scopes[name];
    if (scope?.user_agent == null || scope.error !== null) {
      list.add(`scope.${name}`, 'warn', { observed: scope?.error ?? 'did not report' });
      continue;
    }
    reporting.push([name, scope]);
    const userAgent = scope.user_agent;
    const marks = [
      osOfUserAgent(userAgent),
      scope.platform === null ? null : osOfPlatform(scope.platform),
      scope.uad_platform === null ? expected.claimed_os : osOfHintPlatform(scope.uad_platform),
      scope.high_entropy?.platform == null
        ? expected.claimed_os
        : osOfHintPlatform(scope.high_entropy.platform),
    ];
    list.check(
      `os.${name}`,
      marks.every((os) => os === expected.claimed_os),
      true,
      `user_agent=${userAgent} | platform=${scope.platform ?? 'null'} | hint=${scope.uad_platform ?? 'null'} | high_entropy=${scope.high_entropy?.platform ?? 'null'}`,
      expected.claimed_os,
    );
    const majors = [
      browserMajorOfUserAgent(userAgent),
      scope.high_entropy?.ua_full_version == null
        ? expected.browser_major
        : majorOfVersion(scope.high_entropy.ua_full_version),
    ];
    list.check(
      `browser_major.${name}`,
      majors.every((major) => major === expected.browser_major),
      true,
      majors.join(' | '),
      String(expected.browser_major),
    );
    list.equal(`timezone.${name}`, scope.timezone, expected.timezone, true);
    list.equal(
      `languages.${name}`,
      scope.languages?.join(',') ?? null,
      expected.languages.join(','),
      true,
    );
  }

  agreement(list, 'coherence.user_agent', reporting, (scope) => scope.user_agent);
  agreement(list, 'coherence.platform', reporting, (scope) => scope.platform);
  agreement(list, 'coherence.hint_platform', reporting, (scope) => scope.uad_platform);
  agreement(list, 'coherence.hardware_concurrency', reporting, (scope) =>
    scope.hardware_concurrency === null ? null : String(scope.hardware_concurrency),
  );
  agreement(list, 'coherence.device_memory', reporting, (scope) =>
    scope.device_memory === null ? null : String(scope.device_memory),
  );
  agreement(list, 'coherence.webgl', reporting, (scope) =>
    scope.webgl === null ? null : `${scope.webgl.vendor ?? ''} / ${scope.webgl.renderer ?? ''}`,
  );

  const window = report.scopes.window;
  list.add('webgl.available', window.webgl?.renderer == null ? 'warn' : 'pass', {
    observed: window.webgl?.renderer ?? 'no WebGL',
  });

  for (const [name, headers] of Object.entries(requests)) {
    if (headers === null) {
      continue;
    }
    list.equal(
      `headers.${name}.user_agent`,
      header(headers, 'user-agent'),
      window.user_agent ?? '',
      true,
    );
    const hint = header(headers, 'sec-ch-ua-platform');
    if (hint !== null) {
      list.check(
        `headers.${name}.platform`,
        osOfHintPlatform(hint) === expected.claimed_os,
        true,
        hint,
        expected.claimed_os,
      );
    }
  }
  if (requests.navigation !== null) {
    // A browser driven through request interception with the cache switched
    // off sends these two on every request; a person's browser sends neither
    // on a plain navigation.
    const forced = ['pragma', 'cache-control'].filter(
      (name) => header(requests.navigation ?? [], name) !== null,
    );
    list.check(
      'headers.navigation.cache',
      forced.length === 0,
      true,
      forced.length === 0 ? 'none' : forced.join(','),
      'no pragma, no cache-control',
    );
  }
  return list.findings;
}
