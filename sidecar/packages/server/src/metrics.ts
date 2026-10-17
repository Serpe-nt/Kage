import type { EngineName } from '@kage/contract';
import { Counter, Gauge, Histogram, Registry, collectDefaultMetrics } from '@prometheus-io/client';

import type { CloseReason, ContextEvents } from './context-manager.js';

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
    if (processMetrics) {
      // Under the engine they are of: Kage merges both sidecars' metrics into
      // one exposition, where a series without it would be there twice.
      collectDefaultMetrics({ register: this.registry, labels: { engine } });
    }
  }

  opened(result: 'opened' | 'reused' | 'refused' | 'failed'): void {
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
  flowed(result: 'ok' | 'failed' | 'error'): void {
    this.flows.inc({ engine: this.engine, result });
  }

  /** Records one self-test. */
  selftested(result: 'passed' | 'failed' | 'incomplete'): void {
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
