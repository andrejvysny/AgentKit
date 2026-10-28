import type {
  ExecutionCleanup,
  ExecutionIntent,
  ExecutionLease,
  WorkspaceLease,
} from "./coordination.js";
import { readRecord, type LockRecord } from "./coordination-record.js";
import { CodingError } from "./errors.js";
import { persistRecord } from "./coordination-storage.js";
import type { ProcessIdentity } from "./process-identity.js";

class DurableExecutionLease implements ExecutionLease {
  private released = false;
  private writes: Promise<void> = Promise.resolve();

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

  private update(change: (record: LockRecord) => LockRecord): Promise<void> {
    const write = this.writes.then(async () => {
      if (this.released)
        throw new CodingError(
          "cleanup_unknown",
          "workspace lease is already released",
        );
      await persistRecord(this.marker, change(await this.current()));
    });
    this.writes = write.catch(() => {});
    return write;
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
    startIdentity?: string;
    processStart?: string;
    guardianProtocol?: "agentkit-command-guardian-v1";
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

  async observed(
    identities: ProcessIdentity[],
    complete: boolean,
    closed = false,
    sealed = false,
  ): Promise<void> {
    await this.update((record) => {
      if (!record.execution)
        throw new CodingError("cleanup_unknown", "execution intent missing");
      const known = new Map(
        (record.execution.discovery?.identities ?? []).map((identity) => [
          `${identity.pid}:${identity.startIdentity}`,
          identity,
        ]),
      );
      for (const identity of identities)
        known.set(`${identity.pid}:${identity.startIdentity}`, identity);
      return {
        ...record,
        execution: {
          ...record.execution,
          discovery: {
            identities: [...known.values()],
            complete,
            closed,
            sealed,
          },
        },
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
    await this.writes;
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
