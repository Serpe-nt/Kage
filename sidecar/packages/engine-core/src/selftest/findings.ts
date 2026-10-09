import type { Finding, FindingStatus } from '@kage/contract';

/** Builds the findings of one page, so that every one carries the page id and weight. */
export class FindingList {
  readonly findings: Finding[] = [];

  /**
   * @param page id of the page the checks run on.
   * @param weight weight of each of its checks in the score.
   */
  constructor(
    private readonly page: string,
    private readonly weight: number,
  ) {}

  /** Records a check. */
  add(
    check: string,
    status: FindingStatus,
    options: {
      hard?: boolean;
      observed?: string | null;
      expected?: string | null;
      weight?: number;
    } = {},
  ): void {
    this.findings.push({
      page: this.page,
      check,
      status,
      hard: options.hard ?? false,
      weight: options.weight ?? this.weight,
      observed: options.observed ?? null,
      expected: options.expected ?? null,
    });
  }

  /** Records a check that passes when the two values are equal. */
  equal(check: string, observed: string | null, expected: string, hard: boolean): void {
    this.add(check, observed === expected ? 'pass' : 'fail', { hard, observed, expected });
  }

  /** Records a check from a condition. */
  check(
    check: string,
    ok: boolean,
    hard: boolean,
    observed: string | null,
    expected: string | null = null,
  ): void {
    this.add(check, ok ? 'pass' : 'fail', { hard, observed, expected });
  }
}

/** Score and verdict of a self-test (spec §7.4). */
export interface Verdict {
  readonly score: number;
  readonly passed: boolean;
  readonly complete: boolean;
}

/**
 * The score of a list of findings:
 * `100 × (1 − (Σ weight of fails + ½ Σ weight of warns) / Σ weight of known checks)`,
 * rounded to one decimal. `unknown` findings count for nothing, so a test
 * site that is down or changed its markup never lowers a score; with no
 * known check at all the score is 100.
 */
export function scoreFindings(findings: readonly Finding[]): number {
  let known = 0;
  let lost = 0;
  for (const finding of findings) {
    if (finding.status === 'unknown') {
      continue;
    }
    known += finding.weight;
    if (finding.status === 'fail') {
      lost += finding.weight;
    } else if (finding.status === 'warn') {
      lost += finding.weight / 2;
    }
  }
  return known === 0 ? 100 : Math.round((1 - lost / known) * 1000) / 10;
}

/**
 * The verdict: passed means no hard failure and a score of at least
 * `minScore`; complete means every page could be read.
 */
export function verdict(
  findings: readonly Finding[],
  minScore: number,
  everyPageRead: boolean,
): Verdict {
  const score = scoreFindings(findings);
  const hardFailure = findings.some((finding) => finding.hard && finding.status === 'fail');
  return {
    score,
    passed: !hardFailure && score >= minScore,
    complete: everyPageRead && !findings.some((finding) => finding.status === 'unknown'),
  };
}
