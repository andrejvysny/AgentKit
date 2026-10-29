import type {
  ExecutionCleanup,
  WorkspaceRecoveryReason,
  WorkspaceRecoveryRecord,
} from "./coordination.js";
import { withRecordClaim } from "./coordination-claim.js";
import {
  liveOwner,
  readRecord,
  type LockRecord,
} from "./coordination-record.js";
import { persistRecord } from "./coordination-storage.js";
import {
  inspectProcess,
  type ProcessDiscovery,
  type ProcessIdentity,
} from "./process-identity.js";
import {
  recoverGuardianExecution,
  type GuardianRecoveryDependencies,
} from "./coordination-guardian-recovery.js";

interface RecoveryDependencies extends GuardianRecoveryDependencies {
  afterVerification?(): Promise<void>;
  afterPersistence?(): Promise<void>;
}

export interface MarkerRecovery {
  record?: WorkspaceRecoveryRecord;
  cleanup?: ExecutionCleanup;
}

function rootIdentity(record: LockRecord): ProcessIdentity | undefined {
  const root = record.execution?.process;
  return root?.startIdentity
    ? { pid: root.pid, startIdentity: root.startIdentity }
    : undefined;
}

function blocked(
  record: LockRecord | "corrupt",
  marker: WorkspaceRecoveryRecord["marker"],
  reason: WorkspaceRecoveryReason,
): MarkerRecovery {
  const id =
    record === "corrupt"
      ? "unreadable-workspace-owner"
      : (record.execution?.executionAttemptId ??
        record.lockId ??
        "plain-owner");
  return {
    record: { marker, executionAttemptId: id, status: "blocked", reason },
    cleanup:
      record !== "corrupt" && record.execution?.cleanup
        ? record.execution.cleanup
        : { executionAttemptId: id, state: "unknown", survivors: [], reason },
  };
}

async function closedProof(
  record: LockRecord,
  inspect: typeof inspectProcess,
): Promise<WorkspaceRecoveryReason> {
  const root = rootIdentity(record);
  if (!root) return "process_identity_missing";
  const discovery = record.execution?.discovery;
  if (!discovery?.complete) return "discovery_incomplete";
  if (!discovery.closed) return "discovery_open";
  if (
    !discovery.identities.some(
      (identity) =>
        identity.pid === root.pid &&
        identity.startIdentity === root.startIdentity,
    )
  )
    return "process_identity_missing";
  const states = await Promise.all(
    discovery.identities.map((identity) => inspect(identity)),
  );
  if (states.some(({ state }) => state === "unknown"))
    return "inspection_unknown";
  if (states.some(({ state }) => state === "same-live")) return "process_live";
  return "verified_stopped";
}

async function ownerReason(
  record: LockRecord,
  inspect: typeof inspectProcess,
): Promise<WorkspaceRecoveryReason | undefined> {
  if (!record.startIdentity) return "owner_identity_missing";
  const owner = await inspect({
    pid: record.pid,
    startIdentity: record.startIdentity,
  });
  if (owner.state === "same-live") return "owner_active";
  if (owner.state === "unknown") return "inspection_unknown";
  return undefined;
}

async function recoverClaimed(
  path: string,
  record: LockRecord,
  dependencies: RecoveryDependencies,
): Promise<WorkspaceRecoveryReason> {
  const inspect = dependencies.inspect ?? inspectProcess;
  const owner = await ownerReason(record, inspect);
  if (owner) return owner;
  const execution = record.execution!;
  const root = rootIdentity(record);
  const persist = async (
    discovery: ProcessDiscovery & { closed?: boolean; sealed?: boolean },
    cleanup: ExecutionCleanup,
  ): Promise<void> => {
    if (cleanup.state === "stopped") await dependencies.afterVerification?.();
    await persistRecord(path, {
      ...record,
      execution: { ...execution, discovery, cleanup },
    });
    await dependencies.afterPersistence?.();
  };
  if (
    root &&
    execution.process?.guardianProtocol === "agentkit-command-guardian-v1" &&
    !execution.discovery?.closed
  ) {
    const result = await recoverGuardianExecution(path, record, dependencies);
    await persist(result.discovery, {
      executionAttemptId: execution.executionAttemptId,
      state: result.state,
      survivors: result.survivors,
      reason: `guardian recovery: ${result.reason}`,
    });
    return result.reason;
  }
  const reason = await closedProof(record, inspect);
  if (reason !== "verified_stopped") return reason;
  await persist(execution.discovery!, {
    executionAttemptId: execution.executionAttemptId,
    state: "stopped",
    survivors: [],
    reason: "verified closed process census after owner exit",
  });
  return "verified_stopped";
}

/** No authority input: only durable process evidence can resolve an abandoned execution. */
export async function recoverWorkspaceMarker(
  path: string,
  marker: WorkspaceRecoveryRecord["marker"],
  dependencies: RecoveryDependencies = {},
): Promise<MarkerRecovery> {
  const record = await readRecord(path);
  if (record === "gone") return {};
  if (record === "corrupt")
    return blocked(record, marker, "ownership_unreadable");
  if (!record.execution || record.execution.cleanup?.state === "stopped") {
    const owner = await liveOwner(path, () => {});
    return owner ? blocked(owner, marker, "owner_active") : {};
  }
  const owner = await ownerReason(
    record,
    dependencies.inspect ?? inspectProcess,
  );
  if (owner) return blocked(record, marker, owner);
  const outcome: { reason: WorkspaceRecoveryReason } = {
    reason: "recovery_busy",
  };
  const acted = await withRecordClaim(path, record, async () => {
    outcome.reason = await recoverClaimed(path, record, dependencies);
  });
  if (!acted) {
    const current = await readRecord(path);
    if (JSON.stringify(current) !== JSON.stringify(record))
      outcome.reason = "generation_changed";
  }
  const reason = outcome.reason;
  if (reason !== "verified_stopped") return blocked(record, marker, reason);
  // The terminal record has a new digest. Its own claim chain exclusively owns any removal.
  await liveOwner(path, () => {});
  return {
    record: {
      marker,
      executionAttemptId: record.execution.executionAttemptId,
      status: "recovered",
      reason,
    },
    cleanup: {
      executionAttemptId: record.execution.executionAttemptId,
      state: "stopped",
      survivors: [],
    },
  };
}
