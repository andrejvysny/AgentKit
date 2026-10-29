import {
  RecordNotFoundError,
  readToolExecutionRecord,
  type RunWriteFence,
  type ToolExecutionAdmission,
  type ToolExecutionRecord,
  type ToolExecutionStore,
} from "@agentkit/host";
import type { AiToolResult } from "@agentkit/contracts";

interface Options {
  withGate<T>(operation: () => T): Promise<T>;
  assertFence(fence: RunWriteFence): void;
  taskExists?: (runId: string) => boolean;
}

export class MemoryToolExecutionStore implements ToolExecutionStore {
  constructor(
    private readonly options: Options,
    private readonly records = new Map<string, ToolExecutionRecord>(),
  ) {}

  withGate(withGate: Options["withGate"]): MemoryToolExecutionStore {
    return new MemoryToolExecutionStore(
      { ...this.options, withGate },
      this.records,
    );
  }

  pruneDeletedRuns(): void {
    for (const [id, record] of this.records)
      if (this.options.taskExists?.(record.runId) === false)
        this.records.delete(id);
  }

  async admit(
    input: ToolExecutionAdmission,
    fence: RunWriteFence,
  ): Promise<boolean> {
    readToolExecutionRecord({ ...input, status: "admitted" });
    return this.options.withGate(() => {
      this.options.assertFence(fence);
      if (input.runId !== fence.taskId)
        throw new RecordNotFoundError(
          "Execution does not belong to the fenced run.",
        );
      if (
        this.records.has(input.executionId) ||
        [...this.records.values()].some(
          (record) =>
            input.approvalRequestId !== undefined &&
            record.approvalRequestId === input.approvalRequestId,
        )
      )
        return false;
      const previous = [...this.records.values()]
        .filter(
          (record) =>
            record.runId === input.runId &&
            record.toolName === input.toolName &&
            record.argsSha256 === input.argsSha256 &&
            record.contextSha256 === input.contextSha256,
        )
        .at(-1);
      if (
        previous?.status === "admitted" &&
        input.approvalGeneration !== previous.executionId
      )
        return false;
      this.records.set(
        input.executionId,
        structuredClone({ ...input, status: "admitted" }),
      );
      return true;
    });
  }

  async complete(
    executionId: string,
    result: AiToolResult<unknown>,
    fence: RunWriteFence,
  ): Promise<void> {
    const serialized = JSON.stringify(result);
    return this.options.withGate(() => {
      this.options.assertFence(fence);
      const record = this.records.get(executionId);
      if (!record || record.runId !== fence.taskId)
        throw new RecordNotFoundError(
          "Execution does not belong to the fenced run.",
        );
      if (record.status === "completed") return;
      readToolExecutionRecord({
        ...record,
        result: JSON.parse(serialized) as unknown,
        status: "completed",
      });
      record.result = JSON.parse(serialized) as AiToolResult<unknown>;
      record.status = "completed";
    });
  }

  async listByRun(runId: string): Promise<ToolExecutionRecord[]> {
    return this.options.withGate(() =>
      [...this.records.values()]
        .filter((record) => record.runId === runId)
        .map((record) =>
          structuredClone(readToolExecutionRecord(record, { runId })),
        ),
    );
  }
}
