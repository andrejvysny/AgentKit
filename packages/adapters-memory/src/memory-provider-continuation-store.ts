import {
  decodeProviderContinuation,
  encodeProviderContinuation,
  LeaseLostError,
  providerContinuationScopeMismatch,
  providerContinuationScopesEqual,
  providerContinuationStateMissing,
  validateProviderContinuationAnchors,
  validateProviderContinuationScope,
  type ProviderContinuationRecord,
  type ProviderContinuationScope,
  type ProviderContinuationStore,
  type RunWriteFence,
} from "@agentkit/host";

interface ContinuationMaps {
  scopes: Map<string, ProviderContinuationScope>;
  records: Map<string, ProviderContinuationRecord>;
  required: Set<string>;
}

export interface MemoryProviderContinuationStoreOptions {
  /** Root and transaction views inject the aggregate's matching owner gate. */
  withGate<T>(operation: () => T): Promise<T>;
  assertFence(fence: RunWriteFence): void;
  taskScope(runId: string): string | null;
  taskChat?(runId: string): unknown;
  chatExists(chatId: string): boolean;
  anchorChat(anchorMessageId: string): string | null;
  anchorRun(anchorMessageId: string): string | null;
}

/** Private Map state shares the memory adapter's serialization, without rollback. */
export class MemoryProviderContinuationStore
  implements ProviderContinuationStore
{
  constructor(
    private readonly options: MemoryProviderContinuationStoreOptions,
    private readonly maps: ContinuationMaps = {
      scopes: new Map(),
      records: new Map(),
      required: new Set(),
    },
  ) {}

  withGate(
    withGate: MemoryProviderContinuationStoreOptions["withGate"],
  ): MemoryProviderContinuationStore {
    return new MemoryProviderContinuationStore(
      { ...this.options, withGate },
      this.maps,
    );
  }

  async bindRun(
    runId: string,
    scope: ProviderContinuationScope,
    fence: RunWriteFence,
  ): Promise<void> {
    const validated = validateProviderContinuationScope(scope);
    return this.options.withGate(() => {
      this.assertFence(runId, validated.chatId, fence);
      const prior = this.maps.scopes.get(runId);
      if (
        prior !== undefined &&
        !providerContinuationScopesEqual(prior, validated)
      )
        providerContinuationScopeMismatch();
      if (prior === undefined) this.maps.scopes.set(runId, validated);
    });
  }

  async getRunScope(runId: string): Promise<ProviderContinuationScope | null> {
    return this.options.withGate(() => this.selectScope(runId));
  }

  async put(
    input: ProviderContinuationRecord,
    fence: RunWriteFence,
  ): Promise<ProviderContinuationRecord> {
    const state = decodeProviderContinuation(
      encodeProviderContinuation(input.state),
    );
    const runId = input.runId;
    const anchorMessageId = input.anchorMessageId;
    return this.options.withGate(() => {
      this.assertFence(runId, state.scope.chatId, fence);
      const bound = this.selectScope(runId);
      if (
        bound === null ||
        !providerContinuationScopesEqual(bound, state.scope) ||
        this.options.anchorChat(anchorMessageId) !== state.scope.chatId ||
        this.options.anchorRun(anchorMessageId) !== runId
      )
        providerContinuationScopeMismatch();
      const record = { runId, anchorMessageId, state };
      this.maps.records.set(runId, record);
      this.maps.required.add(runId);
      return structuredClone(record);
    });
  }

  async getByRun(runId: string): Promise<ProviderContinuationRecord | null> {
    return this.options.withGate(() => this.selectRecord(runId));
  }

  async findByAnchors(
    chatId: string,
    anchorMessageIds: readonly string[],
  ): Promise<ProviderContinuationRecord[]> {
    const anchors = validateProviderContinuationAnchors(
      chatId,
      anchorMessageIds,
    );
    return this.options.withGate(() =>
      anchors.flatMap((anchor) => {
        const candidates = [...this.maps.records.values()]
          .filter(
            (record) =>
              record.anchorMessageId === anchor &&
              record.state.scope.chatId === chatId,
          )
          .sort((left, right) =>
            left.runId < right.runId ? -1 : left.runId === right.runId ? 0 : 1,
          );
        return candidates.flatMap((record) => {
          const selected = this.selectRecord(record.runId);
          return selected === null ? [] : [selected];
        });
      }),
    );
  }

  /** Internal cascade hooks; aggregate deletion owns their serialization. */
  deleteByChat(chatId: string): void {
    for (const [runId, record] of this.maps.records) {
      if (record.state.scope.chatId === chatId) this.maps.records.delete(runId);
    }
  }

  pruneDeletedRuns(): void {
    for (const runId of this.maps.scopes.keys()) {
      if (this.options.taskScope(runId) === null) {
        this.maps.scopes.delete(runId);
        this.maps.records.delete(runId);
        this.maps.required.delete(runId);
      }
    }
  }

  private selectScope(runId: string): ProviderContinuationScope | null {
    if (this.options.taskScope(runId) === null) {
      this.maps.scopes.delete(runId);
      this.maps.records.delete(runId);
      this.maps.required.delete(runId);
      return null;
    }
    const scope = this.maps.scopes.get(runId);
    return scope === undefined ? null : structuredClone(scope);
  }

  private selectRecord(runId: string): ProviderContinuationRecord | null {
    const bound = this.selectScope(runId);
    const record = this.maps.records.get(runId);
    if (record === undefined) {
      if (bound !== null && this.maps.required.has(runId))
        providerContinuationStateMissing();
      return null;
    }
    if (
      bound === null ||
      !this.options.chatExists(record.state.scope.chatId) ||
      this.options.anchorChat(record.anchorMessageId) === null
    ) {
      this.maps.records.delete(runId);
      if (bound !== null && this.maps.required.has(runId))
        providerContinuationStateMissing();
      return null;
    }
    if (
      !providerContinuationScopesEqual(bound, record.state.scope) ||
      this.options.anchorChat(record.anchorMessageId) !==
        record.state.scope.chatId ||
      this.options.anchorRun(record.anchorMessageId) !== runId
    )
      providerContinuationScopeMismatch();
    return structuredClone(record);
  }

  private assertFence(
    runId: string,
    chatId: string,
    fence: RunWriteFence,
  ): void {
    if (!fence || fence.taskId !== runId)
      throw new LeaseLostError(
        "Provider continuation does not belong to the fenced run.",
      );
    this.options.assertFence(fence);
    const taskChat = this.options.taskChat?.(runId);
    if (
      this.options.taskScope(runId) === null ||
      !this.options.chatExists(chatId) ||
      (taskChat !== undefined && taskChat !== chatId)
    )
      providerContinuationScopeMismatch();
  }
}
