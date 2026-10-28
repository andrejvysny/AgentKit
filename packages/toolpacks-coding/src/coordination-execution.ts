import type {
  ExecutionCleanup,
  ExecutionIntent,
  ExecutionLease,
  WorkspaceLease,
} from "./coordination.js";
import { readRecord, type LockRecord } from "./coordination-record.js";
import { CodingError } from "./errors.js";
import { persistRecord } from "./coordination-storage.js";

class DurableExecutionLease implements ExecutionLease {
  private released = false;

  constructor(
    private readonly inner: WorkspaceLease,
    private readonly marker: string,
    private readonly lockId: string,
  ) {}

  private async current(): Promise<LockRecord> {
    const record = await readRecord(this.marker);
    if (
      record === "gone" ||
      record === "corrupt" ||
      record.lockId !== this.lockId
    )
      throw new CodingError(
        "cleanup_unknown",
        "workspace ownership changed or is unreadable",
      );
    return record;
  }

  private async update(
    change: (record: LockRecord) => LockRecord,
  ): Promise<void> {
    if (this.released)
      throw new CodingError(
        "cleanup_unknown",
        "workspace lease is already released",
      );
    await persistRecord(this.marker, change(await this.current()));
  }

  async beginExecution(intent: ExecutionIntent): Promise<void> {
    await this.update((record) => {
      if (record.execution)
        throw new CodingError(
          "cleanup_unknown",
          "workspace lease already owns an execution",
        );
      return { ...record, execution: intent };
    });
  }

  async spawned(identity: {
    pid: number;
    processStart?: string;
  }): Promise<void> {
    await this.update((record) => {
      if (!record.execution)
        throw new CodingError("cleanup_unknown", "execution intent missing");
      return {
        ...record,
        execution: { ...record.execution, process: identity },
      };
    });
  }

  async finishExecution(cleanup: ExecutionCleanup): Promise<void> {
    await this.update((record) => {
      if (
        !record.execution ||
        record.execution.executionAttemptId !== cleanup.executionAttemptId
      )
        throw new CodingError(
          "cleanup_unknown",
          "execution identity does not match workspace ownership",
        );
      return { ...record, execution: { ...record.execution, cleanup } };
    });
  }

  async release(): Promise<void> {
    if (this.released) return;
    const record = await this.current();
    if (record.execution && record.execution.cleanup?.state !== "stopped")
      throw new CodingError(
        "workspace_quarantined",
        "command cleanup is unresolved; workspace mutations remain blocked",
      );
    await this.inner.release();
    this.released = true;
  }
}

export async function executionLease(
  inner: WorkspaceLease,
  marker: string,
): Promise<ExecutionLease> {
  const initial = await readRecord(marker);
  if (initial === "gone" || initial === "corrupt" || !initial.lockId)
    throw new CodingError(
      "cleanup_unknown",
      "workspace ownership cannot be verified",
    );
  return new DurableExecutionLease(inner, marker, initial.lockId);
}
