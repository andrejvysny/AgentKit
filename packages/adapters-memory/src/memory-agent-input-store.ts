import {
  AgentInputConflictError,
  assertAgentInputTransition,
  RecordNotFoundError,
  validateCreateAgentInput,
  type AgentInputPatch,
  type AgentInputQuery,
  type AgentInputRecord,
  type AgentInputStatus,
  type AgentInputStore,
  type Clock,
  type CreateAgentInput,
} from "@agentkit/host";

interface InputState {
  records: Map<string, AgentInputRecord>;
  lastOrder: number;
}

export interface MemoryAgentInputStoreOptions {
  clock: Clock;
  /** Root and transaction views inject the aggregate's matching owner gate. */
  withGate<T>(operation: () => T): Promise<T>;
}

function copy(record: AgentInputRecord): AgentInputRecord {
  return structuredClone(record);
}

/** Map state shares the memory adapter's serialization, without rollback. */
export class MemoryAgentInputStore implements AgentInputStore {
  constructor(
    private readonly options: MemoryAgentInputStoreOptions,
    private readonly state: InputState = { records: new Map(), lastOrder: 0 },
  ) {}

  withGate(
    withGate: MemoryAgentInputStoreOptions["withGate"],
  ): MemoryAgentInputStore {
    return new MemoryAgentInputStore({ ...this.options, withGate }, this.state);
  }

  async createInput(
    input: CreateAgentInput,
  ): Promise<{ record: AgentInputRecord; created: boolean }> {
    validateCreateAgentInput(input);
    return this.options.withGate(() => {
      const existing = this.state.records.get(input.inputId);
      if (existing) {
        if (
          existing.kind !== input.kind ||
          existing.sessionId !== input.sessionId ||
          existing.contentHash !== input.contentHash
        ) {
          throw new AgentInputConflictError(
            `Input ${input.inputId} already exists with a different kind, session or content.`,
            { inputId: input.inputId },
          );
        }
        return { record: copy(existing), created: false };
      }
      const now = this.options.clock.nowIso();
      this.state.lastOrder += 1;
      const record: AgentInputRecord = {
        inputId: input.inputId,
        sessionId: input.sessionId,
        kind: input.kind,
        status: "pending",
        content: structuredClone(input.content),
        contentHash: input.contentHash,
        order: this.state.lastOrder,
        createdAt: now,
        updatedAt: now,
      };
      if (input.targetTaskId !== undefined)
        record.targetTaskId = input.targetTaskId;
      if (input.onSettled !== undefined) record.onSettled = input.onSettled;
      if (input.attemptId !== undefined) record.attemptId = input.attemptId;
      if (input.toolCallId !== undefined) record.toolCallId = input.toolCallId;
      if (input.expiresAt !== undefined) record.expiresAt = input.expiresAt;
      this.state.records.set(record.inputId, record);
      return { record: copy(record), created: true };
    });
  }

  async getInput(inputId: string): Promise<AgentInputRecord | undefined> {
    return this.options.withGate(() => {
      const record = this.state.records.get(inputId);
      return record ? copy(record) : undefined;
    });
  }

  async listInputs(query: AgentInputQuery): Promise<AgentInputRecord[]> {
    return this.options.withGate(() =>
      [...this.state.records.values()]
        .filter(
          (record) =>
            record.sessionId === query.sessionId &&
            (query.kinds === undefined || query.kinds.includes(record.kind)) &&
            (query.statuses === undefined ||
              query.statuses.includes(record.status)) &&
            (query.targetTaskId === undefined ||
              record.targetTaskId === query.targetTaskId),
        )
        .sort((a, b) => a.order - b.order)
        .map(copy),
    );
  }

  async transitionInput(
    inputId: string,
    from: readonly AgentInputStatus[],
    to: AgentInputStatus,
    patch?: AgentInputPatch,
  ): Promise<AgentInputRecord> {
    return this.options.withGate(() => {
      const record = this.state.records.get(inputId);
      if (!record) throw new RecordNotFoundError(`Input not found: ${inputId}`);
      assertAgentInputTransition(record, from, to);
      record.status = to;
      if (patch?.appliedTaskId !== undefined)
        record.appliedTaskId = patch.appliedTaskId;
      if (patch?.appliedMessageId !== undefined)
        record.appliedMessageId = patch.appliedMessageId;
      if (patch?.answerId !== undefined) record.answerId = patch.answerId;
      record.updatedAt = this.options.clock.nowIso();
      return copy(record);
    });
  }
}
