import { createHash, randomUUID } from 'node:crypto';

import type { OpenContextRequest } from '@kage/contract';
import { EngineError, type Engine, type EngineContext, type EngineLog } from '@kage/engine-core';

/** Why a context was closed; a metric label. */
export type CloseReason = 'requested' | 'idle' | 'failed' | 'shutdown';

/** What the manager reports about itself; the server turns these into metrics. */
export interface ContextEvents {
  opened(result: 'opened' | 'reused' | 'refused' | 'failed'): void;
  closed(reason: CloseReason): void;
  recycled(): void;
}

/** Limits of the manager (spec §10.1, §10.6). */
export interface ContextLimits {
  /** Most contexts open at once. */
  readonly maxContexts: number;
  /** A context unused for this long is closed. */
  readonly idleMs: number;
  /** A context's browser is restarted after this many navigations. */
  readonly maxPagesPerBrowser: number;
}

/** One open context as the manager keeps it. */
export interface ManagedContext {
  readonly id: string;
  readonly personaId: string;
  readonly openedAt: Date;
  /** The engine's context; replaced when the browser is recycled. */
  context: EngineContext;
}

interface Entry extends ManagedContext {
  /** The request it was opened with: needed to restart its browser. Holds the proxy password; never logged. */
  readonly request: OpenContextRequest;
  readonly requestHash: string;
  lastUsed: number;
  busy: boolean;
  navigations: number;
}

/** A canonical form of a value: object keys sorted, so equal requests hash equally. */
function canonical(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(canonical);
  }
  if (typeof value === 'object' && value !== null) {
    return Object.fromEntries(
      Object.entries(value)
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([key, inner]) => [key, canonical(inner)]),
    );
  }
  return value;
}

const hashRequest = (request: OpenContextRequest): string =>
  createHash('sha256')
    .update(JSON.stringify(canonical(request)))
    .digest('hex');

/**
 * Keeps the persona contexts of one sidecar container (spec §10.1, §10.6).
 *
 * - At most `maxContexts` are open or opening.
 * - A persona has at most one context. Opening it again with the same
 *   parameters returns the open one; with other parameters it is a conflict.
 * - One operation runs on a context at a time; a second one is refused as
 *   busy and not queued, because Kage paces a persona itself.
 * - A context that was not used for `idleMs` is closed; its profile stays
 *   on disk and Kage opens it again on demand.
 * - After `maxPagesPerBrowser` navigations a context's browser is restarted
 *   before the next one, with the same profile and under the same id.
 */
export class ContextManager {
  private readonly entries = new Map<string, Entry>();
  private readonly byPersona = new Map<string, string>();
  private readonly opening = new Map<string, Promise<Entry>>();
  private timer: NodeJS.Timeout | null = null;

  constructor(
    private readonly engine: Engine,
    private readonly limits: ContextLimits,
    private readonly events: ContextEvents,
    private readonly log: EngineLog,
    private readonly now: () => number = Date.now,
  ) {}

  /** Contexts open or being opened. */
  get count(): number {
    return this.entries.size + this.opening.size;
  }

  /** Starts closing idle contexts on a timer. The timer never keeps the process alive. */
  start(): void {
    if (this.timer === null) {
      const interval = Math.max(250, Math.min(15_000, Math.floor(this.limits.idleMs / 4)));
      this.timer = setInterval(() => {
        void this.sweep();
      }, interval);
      this.timer.unref();
    }
  }

  /**
   * Opens the persona's context, or returns the one that is open.
   *
   * @throws {EngineError} `context_limit`, `context_conflict`, or what the engine throws.
   */
  async open(request: OpenContextRequest): Promise<{ managed: ManagedContext; reused: boolean }> {
    const requestHash = hashRequest(request);
    const inFlight = this.opening.get(request.persona_id);
    if (inFlight !== undefined) {
      // A second open for the same persona while the first is starting its
      // browser: wait for that one instead of starting another.
      await inFlight.catch(() => undefined);
    }
    const existingId = this.byPersona.get(request.persona_id);
    const existing = existingId === undefined ? undefined : this.entries.get(existingId);
    if (existing !== undefined) {
      if (existing.requestHash !== requestHash) {
        this.events.opened('refused');
        throw new EngineError(
          'context_conflict',
          `persona ${request.persona_id} has an open context with other parameters: close it first`,
        );
      }
      existing.lastUsed = this.now();
      this.events.opened('reused');
      return { managed: existing, reused: true };
    }
    if (this.count >= this.limits.maxContexts) {
      this.events.opened('refused');
      throw new EngineError(
        'context_limit',
        `this container already runs ${this.limits.maxContexts} contexts`,
      );
    }
    const started = this.engine.openContext(request).then((context): Entry => {
      const entry: Entry = {
        id: randomUUID(),
        personaId: request.persona_id,
        openedAt: new Date(this.now()),
        context,
        request,
        requestHash,
        lastUsed: this.now(),
        busy: false,
        navigations: 0,
      };
      this.entries.set(entry.id, entry);
      this.byPersona.set(entry.personaId, entry.id);
      return entry;
    });
    this.opening.set(request.persona_id, started);
    try {
      const entry = await started;
      this.events.opened('opened');
      return { managed: entry, reused: false };
    } catch (error) {
      this.events.opened('failed');
      throw error;
    } finally {
      this.opening.delete(request.persona_id);
    }
  }

  private entry(id: string): Entry {
    const entry = this.entries.get(id);
    if (entry === undefined) {
      throw new EngineError(
        'context_not_found',
        `no open context has the id ${id}: it was closed, or closed itself after being idle`,
      );
    }
    return entry;
  }

  /**
   * Runs one operation on a context.
   *
   * A failure that is not an answer of the contract means the browser is in
   * an unknown state: the context is closed, and Kage opens it again.
   *
   * @param navigation whether the operation loads pages and counts toward the recycle limit.
   * @throws {EngineError} `context_not_found`, `context_busy`, or what the operation throws.
   */
  async run<T>(
    id: string,
    navigation: boolean,
    operation: (context: EngineContext) => Promise<T>,
  ): Promise<T> {
    const entry = this.entry(id);
    if (entry.busy) {
      throw new EngineError('context_busy', `context ${id} is running another operation`);
    }
    entry.busy = true;
    entry.lastUsed = this.now();
    try {
      if (navigation && entry.navigations >= this.limits.maxPagesPerBrowser) {
        await entry.context.close();
        entry.context = await this.engine.openContext(entry.request);
        entry.navigations = 0;
        this.events.recycled();
        this.log.info({ context_id: id, persona_id: entry.personaId }, 'browser recycled');
      }
      if (navigation) {
        entry.navigations += 1;
      }
      return await operation(entry.context);
    } catch (error) {
      if (!(error instanceof EngineError)) {
        this.log.error(
          { err: error, context_id: id, persona_id: entry.personaId },
          'operation failed; closing the context',
        );
        await this.remove(entry, 'failed');
      }
      throw error;
    } finally {
      entry.busy = false;
      entry.lastUsed = this.now();
    }
  }

  private async remove(entry: Entry, reason: CloseReason): Promise<void> {
    if (this.entries.delete(entry.id)) {
      this.byPersona.delete(entry.personaId);
      this.events.closed(reason);
      await entry.context.close().catch((error: unknown) => {
        this.log.warn({ err: error, context_id: entry.id }, 'closing a context failed');
      });
    }
  }

  /**
   * Closes a context and flushes its profile.
   *
   * @throws {EngineError} `context_not_found`, or `context_busy` while an operation runs on it.
   */
  async close(id: string): Promise<void> {
    const entry = this.entry(id);
    if (entry.busy) {
      throw new EngineError('context_busy', `context ${id} is running an operation`);
    }
    await this.remove(entry, 'requested');
  }

  /** Closes every context that was idle for too long. Returns how many. */
  async sweep(): Promise<number> {
    const cutoff = this.now() - this.limits.idleMs;
    const idle = [...this.entries.values()].filter(
      (entry) => !entry.busy && entry.lastUsed <= cutoff,
    );
    await Promise.all(
      idle.map((entry) => {
        this.log.info(
          { context_id: entry.id, persona_id: entry.personaId },
          'closing an idle context',
        );
        return this.remove(entry, 'idle');
      }),
    );
    return idle.length;
  }

  /** Stops the timer and closes everything. For shutdown. */
  async closeAll(): Promise<void> {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
    await Promise.allSettled([...this.opening.values()]);
    await Promise.all([...this.entries.values()].map((entry) => this.remove(entry, 'shutdown')));
  }
}
