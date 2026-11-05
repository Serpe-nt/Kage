import type { EngineName } from '@kage/contract';
import { Counter, Gauge, Histogram, Registry, collectDefaultMetrics } from '@prometheus-io/client';

import type { CloseReason, ContextEvents } from './context-manager.js';

/** The results of a context open request, as the context manager reports them. */
export const OPEN_RESULTS = ['opened', 'reused', 'refused', 'failed'] as const;
/** Why a context was closed. */
export const CLOSE_REASONS: readonly CloseReason[] = ['requested', 'idle', 'failed', 'shutdown'];
/** The results of a self-test. */
export const SELFTEST_RESULTS = ['passed', 'failed', 'incomplete'] as const;
/** The results of a flow. */
export const FLOW_RESULTS = ['ok', 'failed', 'error'] as const;

/** Outcome label of a navigation: `ok`, or the contract's navigation error code. */
export type NavigationOutcome =
  'ok' | 'timeout' | 'navigation_failed' | 'proxy_failed' | 'aborted' | 'error';

/**
 * The sidecar's Prometheus metrics (spec §20): `sidecar_contexts_open` and
 * `sidecar_navigation_duration_seconds` as the spec names them, counters
 * for what the context manager does, and the process metrics.
 *
 * No label carries a persona, a URL or a host: the label sets stay small,
 * and a metrics scrape never tells which sites a persona visits.
 */
export class SidecarMetrics implements ContextEvents {
  readonly registry = new Registry();
  private readonly navigations: Histogram<'engine' | 'outcome'>;
  private readonly opens: Counter<'engine' | 'result'>;
  private readonly closes: Counter<'engine' | 'reason'>;
  private readonly recycles: Counter<'engine'>;
  private readonly selftests: Counter<'engine' | 'result'>;
  private readonly flows: Counter<'engine' | 'result'>;

  /**
   * @param contextsOpen reads how many contexts are open when a scrape asks.
   * @param processMetrics whether to collect the process metrics; off in tests that build many apps.
   */
  constructor(
    private readonly engine: EngineName,
    contextsOpen: () => number,
    processMetrics = true,
  ) {
    const registers = [this.registry];
    new Gauge({
      name: 'sidecar_contexts_open',
      help: 'Persona contexts currently open.',
      labelNames: ['engine'],
      registers,
      collect() {
        this.set({ engine }, contextsOpen());
      },
    });
    this.navigations = new Histogram({
      name: 'sidecar_navigation_duration_seconds',
      help: 'Duration of navigate calls, from the request to the finished capture.',
      labelNames: ['engine', 'outcome'],
      buckets: [0.25, 0.5, 1, 2, 4, 8, 15, 30, 60, 120, 300],
      registers,
    });
    this.opens = new Counter({
      name: 'sidecar_context_opens_total',
      help: 'Context open requests by result.',
      labelNames: ['engine', 'result'],
      registers,
    });
    this.closes = new Counter({
      name: 'sidecar_context_closes_total',
      help: 'Closed contexts by reason.',
      labelNames: ['engine', 'reason'],
      registers,
    });
    this.recycles = new Counter({
      name: 'sidecar_browser_recycles_total',
      help: 'Browsers restarted after their navigation limit.',
      labelNames: ['engine'],
      registers,
    });
    this.selftests = new Counter({
      name: 'sidecar_selftests_total',
      help: 'Self-tests by result.',
      labelNames: ['engine', 'result'],
      registers,
    });
    this.flows = new Counter({
      name: 'sidecar_flows_total',
      help: 'Flows by result.',
      labelNames: ['engine', 'result'],
      registers,
    });
    // Every counter exists from zero for the label values known at the
    // start, as Kage's do (F110, F150): its first event is then a change
    // that `increase()` counts, not the birth of a series.
    for (const result of OPEN_RESULTS) {
      this.opens.inc({ engine, result }, 0);
    }
    for (const reason of CLOSE_REASONS) {
      this.closes.inc({ engine, reason }, 0);
    }
    this.recycles.inc({ engine }, 0);
    for (const result of SELFTEST_RESULTS) {
      this.selftests.inc({ engine, result }, 0);
    }
    for (const result of FLOW_RESULTS) {
      this.flows.inc({ engine, result }, 0);
    }
    // Every series under the engine it is of: Kage merges both sidecars'
    // metrics into one exposition, where a series without it would be there
    // twice. The event loop's utilization takes no label of
    // `collectDefaultMetrics`; the registry's default fills it in (F144).
    this.registry.setDefaultLabels({ engine });
    if (processMetrics) {
      collectDefaultMetrics({ register: this.registry, labels: { engine } });
    }
  }

  opened(result: (typeof OPEN_RESULTS)[number]): void {
    this.opens.inc({ engine: this.engine, result });
  }

  closed(reason: CloseReason): void {
    this.closes.inc({ engine: this.engine, reason });
  }

  recycled(): void {
    this.recycles.inc({ engine: this.engine });
  }

  /** Records one navigate call. */
  navigated(outcome: NavigationOutcome, seconds: number): void {
    this.navigations.observe({ engine: this.engine, outcome }, seconds);
  }

  /** Records one flow: every step succeeded, a step failed, or the flow itself broke. */
  flowed(result: (typeof FLOW_RESULTS)[number]): void {
    this.flows.inc({ engine: this.engine, result });
  }

  /** Records one self-test. */
  selftested(result: (typeof SELFTEST_RESULTS)[number]): void {
    this.selftests.inc({ engine: this.engine, result });
  }

  /** The metrics in the text exposition format. */
  render(): Promise<string> {
    return this.registry.metrics();
  }

  /** The `Content-Type` of {@link render}'s output. */
  get contentType(): string {
    return this.registry.contentType;
  }
}
