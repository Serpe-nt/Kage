import {
  DEFAULT_MIN_SCORE,
  type ArtifactRef,
  type Finding,
  type NavigationError,
  type NavigationErrorCode,
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

/**
 * The pause before a page that did not arrive is loaded a second time.
 * Fixed, not drawn: a self-test is a measurement, and the same run must
 * take the same path every time.
 */
export const SELFTEST_RETRY_PAUSE_MS = 5000;

/** How many times a page is loaded at most (F95). */
export const SELFTEST_LOAD_ATTEMPTS = 2;

/**
 * The navigation failures after which a page is loaded once more: the
 * budget ran out, the proxy did not carry the connection, the network did
 * not answer. A page that turned into a download or whose browser is gone
 * (`aborted`) would fail the same way again.
 */
const RETRIED_FAILURES: ReadonlySet<NavigationErrorCode> = new Set([
  'timeout',
  'proxy_failed',
  'navigation_failed',
]);

/** Id of the element the probe page publishes its report in. */
export const PROBE_REPORT_SELECTOR = '#kage-probe-report[data-done]';

/**
 * The navigation to a self-test page failed: the browser reported a
 * timeout, a proxy or a network error, and never showed the page. A driver
 * throws it from its navigation alone, never from reading a page that
 * arrived, so that only a page that did not arrive is loaded once more.
 */
export class NavigationFailed extends Error {
  /** The contract's code for what went wrong. */
  readonly code: NavigationErrorCode;

  constructor(error: NavigationError, cause: unknown) {
    super(error.message, { cause });
    this.name = 'NavigationFailed';
    this.code = error.code;
  }

  /** Whether the page is loaded once more after this failure. */
  get retried(): boolean {
    return RETRIED_FAILURES.has(this.code);
  }
}

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
   * @throws {NavigationFailed} when the page did not arrive; the run loads it
   *   once more after a pause, and it counts as unreachable when that fails too.
   * @throws when a page that arrived cannot be read; the page then counts as
   *   unreachable at once.
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
  /** Waits this long; injectable for tests. */
  readonly sleep: (ms: number) => Promise<void>;
}

/** What loading a page came to: the page, or what the last attempt threw. */
interface Loading {
  readonly loaded: LoadedPage | null;
  readonly failure: unknown;
  readonly attempts: number;
}

/**
 * Loads a page, and once more after {@link SELFTEST_RETRY_PAUSE_MS} when the
 * first load was a navigation failure worth a second try (F95). A page that
 * arrived is never loaded again, whatever it shows; a failure in reading a
 * page that arrived is not tried again either.
 */
async function loadWithRetry(
  driver: SelftestDriver,
  page: SelftestPage,
  options: { settleMs: number; timeoutMs: number; publishedSelector: string | null },
  env: SelftestEnv,
): Promise<Loading> {
  let attempts = 0;
  let failure: unknown = null;
  while (attempts < SELFTEST_LOAD_ATTEMPTS) {
    if (attempts > 0) {
      await env.sleep(SELFTEST_RETRY_PAUSE_MS);
    }
    attempts += 1;
    try {
      return { loaded: await driver.load(page, options), failure: null, attempts };
    } catch (error) {
      failure = error;
      if (!(error instanceof NavigationFailed && error.retried)) {
        break;
      }
    }
  }
  return { loaded: null, failure, attempts };
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
 * A page whose navigation fails is loaded once more after a short pause,
 * and the page report says so (`attempts: 2`); a page that arrived is judged
 * as it is, never loaded again. A page that cannot be loaded or read yields
 * one `unknown` finding. It lowers no score and fails nothing, but the
 * result says `complete: false`: an outage of a test site must never cool a
 * persona down.
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
    const loading = await loadWithRetry(
      driver,
      page,
      {
        settleMs: page.settle_ms ?? DEFAULT_SETTLE_MS[page.kind],
        timeoutMs,
        publishedSelector: page.kind === 'probe' ? PROBE_REPORT_SELECTOR : null,
      },
      env,
    );
    try {
      if (loading.loaded === null) {
        throw loading.failure;
      }
      const loaded = loading.loaded;
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
      attempts: loading.attempts,
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
