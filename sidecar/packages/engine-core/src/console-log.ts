import type { ConsoleEntry } from '@kage/contract';

/** Most entries kept; older ones are dropped. */
const LIMIT = 4000;

/**
 * The log entries of one context. An operation notes a mark when it starts
 * and later asks for everything since: entries are counted over the whole
 * life of the context, so dropping old ones never shifts a mark.
 */
export class ConsoleLog {
  private readonly entries: ConsoleEntry[] = [];
  private dropped = 0;

  add(entry: ConsoleEntry): void {
    this.entries.push(entry);
    if (this.entries.length > LIMIT) {
      const excess = this.entries.length - LIMIT / 2;
      this.entries.splice(0, excess);
      this.dropped += excess;
    }
  }

  /** How many entries were ever added. */
  mark(): number {
    return this.dropped + this.entries.length;
  }

  /** The entries added after a mark that are still kept. */
  since(mark: number): ConsoleEntry[] {
    return this.entries.slice(Math.max(0, mark - this.dropped));
  }
}
