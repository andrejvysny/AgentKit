import type { ContextSummaryRecord, ContextSummaryStore } from "@agentkit/host";

export interface MemoryContextSummaryStoreOptions {
  /** Root and transaction views inject the aggregate's matching owner gate. */
  withGate<T>(operation: () => T): Promise<T>;
}

function copy(record: ContextSummaryRecord): ContextSummaryRecord {
  return structuredClone(record);
}

/** Map state shares the memory adapter's serialization, without rollback. */
export class MemoryContextSummaryStore implements ContextSummaryStore {
  constructor(
    private readonly options: MemoryContextSummaryStoreOptions,
    private readonly records: Map<string, ContextSummaryRecord> = new Map(),
  ) {}

  withGate(
    withGate: MemoryContextSummaryStoreOptions["withGate"],
  ): MemoryContextSummaryStore {
    return new MemoryContextSummaryStore({ withGate }, this.records);
  }

  async saveSummary(record: ContextSummaryRecord): Promise<void> {
    return this.options.withGate(() => {
      if (!this.records.has(record.summaryId))
        this.records.set(record.summaryId, copy(record));
    });
  }

  async listSummaries(sessionId: string): Promise<ContextSummaryRecord[]> {
    return this.options.withGate(() =>
      // Reversed first: the stable sort then breaks equal timestamps newest-saved-first.
      [...this.records.values()]
        .filter((record) => record.sessionId === sessionId)
        .reverse()
        .sort((a, b) => b.createdAt.localeCompare(a.createdAt))
        .map(copy),
    );
  }

  async deleteSessionSummaries(sessionId: string): Promise<number> {
    return this.options.withGate(() => {
      let deleted = 0;
      for (const [summaryId, record] of this.records) {
        if (record.sessionId !== sessionId) continue;
        this.records.delete(summaryId);
        deleted += 1;
      }
      return deleted;
    });
  }
}
