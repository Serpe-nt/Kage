import {
  DEFAULT_MIN_SCORE,
  type ArtifactRef,
  type Finding,
  type SelftestPage,
  type SelftestPageReport,
  type SelftestRequest,
  type SelftestResponse,
} from '@kage/contract';

import type { ArtifactWriter } from '../artifacts.js';

import { verdict } from './findings.js';
import {
  DEFAULT_SETTLE_MS,
  judgeCreepjs,
  judgeEcho,
  judgeGeneric,
  judgeSannysoft,
  type TableRow,
} from './pages.js';
import { judgeProbe, parseProbeReport } from './probe.js';

/** Default budget per page. */
export const DEFAULT_SELFTEST_TIMEOUT_MS = 45_000;

/** Id of the element the probe page publishes its report in. */
export const PROBE_REPORT_SELECTOR = '#kage-probe-report[data-done]';

/** What an engine read from one self-test page. */
export interface LoadedPage {
  /** Status of the page's main response; `null` without one. */
  readonly httpStatus: number | null;
  /** The visible text of the document. */
  readonly text: string;
  /** Every table row of the document. */
  readonly rows: readonly TableRow[];
  /** Text content of the element named by `publishedSelector`; `null` when it never appeared. */
  readonly published: string | null;
  /** A PNG of the viewport; `null` when none could be taken. */
  readonly screenshot: Uint8Array | null;
}

/** How an engine loads a self-test page. */
export interface SelftestDriver {
  /**
   * Loads a page and reads it once its own tests had time to finish.
   *
   * @throws when the page cannot be loaded; the page then counts as unreachable.
   */
  load(
    page: SelftestPage,
    options: { settleMs: number; timeoutMs: number; publishedSelector: string | null },
  ): Promise<LoadedPage>;
}

/** What a self-test run needs besides the driver and the request. */
export interface SelftestEnv {
  readonly artifacts: ArtifactWriter;
  /** Directory below the artifacts volume for the evidence files. */
  readonly artifactPrefix: string;
  /** The page set used when the request names none: Kage's own pages only. */
  readonly defaultPages: readonly SelftestPage[];
  /** The clock; injectable for tests. */
  readonly now: () => Date;
}

function judge(
  page: SelftestPage,
  loaded: LoadedPage,
  request: SelftestRequest,
  weight: number,
): Finding[] | null {
  switch (page.kind) {
    case 'probe': {
      const report = loaded.published === null ? null : parseProbeReport(loaded.published);
      return report === null ? null : judgeProbe(report, request.expected, page.id, weight);
    }
    case 'echo':
      return judgeEcho(loaded.text, request.expected, request.echo ?? null, page.id, weight);
    case 'sannysoft':
      return judgeSannysoft(loaded.rows, page.id, weight);
    case 'creepjs':
      return judgeCreepjs(loaded.text, page.id, weight);
    case 'generic':
      return judgeGeneric(loaded.text, page.rules ?? [], page.id, weight);
  }
}

const unknown = (page: SelftestPage, weight: number, observed: string): Finding => ({
  page: page.id,
  check: 'page',
  status: 'unknown',
  hard: false,
  weight,
  observed,
  expected: null,
});

/**
 * Runs a self-test (spec §7.4): loads every page, judges what it shows
 * against the persona's claim, stores the evidence, and scores the findings.
 *
 * A page that cannot be loaded or read yields one `unknown` finding. It
 * lowers no score and fails nothing, but the result says `complete: false`:
 * an outage of a test site must never cool a persona down.
 */
export async function runSelftest(
  driver: SelftestDriver,
  request: SelftestRequest,
  env: SelftestEnv,
): Promise<SelftestResponse> {
  const startedAt = env.now();
  const pages = request.pages ?? env.defaultPages;
  const timeoutMs = request.timeout_ms ?? DEFAULT_SELFTEST_TIMEOUT_MS;
  const findings: Finding[] = [];
  const reports: SelftestPageReport[] = [];
  let everyPageRead = pages.length > 0;

  for (const page of pages) {
    const weight = page.weight ?? 1;
    const began = env.now().getTime();
    let status: SelftestPageReport['status'] = 'ok';
    let httpStatus: number | null = null;
    let text: ArtifactRef | null = null;
    let screenshot: ArtifactRef | null = null;
    try {
      const loaded = await driver.load(page, {
        settleMs: page.settle_ms ?? DEFAULT_SETTLE_MS[page.kind],
        timeoutMs,
        publishedSelector: page.kind === 'probe' ? PROBE_REPORT_SELECTOR : null,
      });
      httpStatus = loaded.httpStatus;
      const evidence = loaded.published ?? loaded.text;
      text = await env.artifacts.write(
        `${env.artifactPrefix}/${page.id}.txt`,
        Buffer.from(evidence, 'utf8'),
        'text/plain; charset=utf-8',
      );
      if (loaded.screenshot !== null) {
        screenshot = await env.artifacts.write(
          `${env.artifactPrefix}/${page.id}.png`,
          loaded.screenshot,
          'image/png',
        );
      }
      const judged = judge(page, loaded, request, weight);
      if (judged === null) {
        status = 'unparsed';
        findings.push(unknown(page, weight, 'the page did not look as its kind expects'));
      } else {
        findings.push(...judged);
      }
    } catch (error) {
      status = 'unreachable';
      findings.push(
        unknown(
          page,
          weight,
          error instanceof Error ? (error.message.split('\n')[0] ?? '') : String(error),
        ),
      );
    }
    if (status !== 'ok') {
      everyPageRead = false;
    }
    reports.push({
      id: page.id,
      url: page.url,
      kind: page.kind,
      status,
      http_status: httpStatus,
      duration_ms: Math.max(0, env.now().getTime() - began),
      text,
      screenshot,
    });
  }

  const minScore = request.min_score ?? DEFAULT_MIN_SCORE;
  const result = verdict(findings, minScore, everyPageRead);
  return {
    passed: result.passed,
    complete: result.complete,
    score: result.score,
    min_score: minScore,
    findings,
    pages: reports,
    started_at: startedAt.toISOString(),
    finished_at: env.now().toISOString(),
  };
}
