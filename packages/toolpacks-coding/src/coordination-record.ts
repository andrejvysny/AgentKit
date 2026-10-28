import { execFile } from "node:child_process";
import { readFile, unlink } from "node:fs/promises";
import { promisify } from "node:util";
import type {
  ExecutionCleanup,
  ExecutionIntent,
  LeaseHolder,
} from "./coordination.js";
import { withRecordClaim } from "./coordination-claim.js";
import { inspectProcess, type ProcessIdentity } from "./process-identity.js";

const runPs = promisify(execFile);

export interface LockRecord {
  lockId?: string;
  pid: number;
  /** `ps -o lstart=` of the owner when it took the lock; absent if unknown. */
  processStart?: string;
  startIdentity?: string;
  tool: string;
  /** sha256(label).slice(0, 12). */
  label: string;
  createdAt: number;
  execution?: ExecutionIntent & {
    process?: {
      pid: number;
      startIdentity?: string;
      processStart?: string;
      guardianProtocol?: "agentkit-command-guardian-v1";
    };
    discovery?: {
      identities: ProcessIdentity[];
      complete: boolean;
      closed?: boolean;
      sealed?: boolean;
    };
    cleanup?: ExecutionCleanup;
  };
}

export type OnError = (error: unknown) => void;

const START_TTL_MS = 1000;

export function code(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String(error.code)
    : undefined;
}

function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return code(error) !== "ESRCH";
  }
}

const starts = new Map<number, { value: string | undefined; at: number }>();

/** Process start time as printed by ps; undefined when it cannot be read. */
export async function processStart(
  pid: number,
  fresh = false,
): Promise<string | undefined> {
  const cached = starts.get(pid);
  const ttl = pid === process.pid ? Number.POSITIVE_INFINITY : START_TTL_MS;
  if (!fresh && cached && Date.now() - cached.at < ttl) return cached.value;
  let value: string | undefined;
  try {
    const { stdout } = await runPs("ps", ["-o", "lstart=", "-p", String(pid)], {
      env: { ...process.env, LC_ALL: "C" },
    });
    value = stdout.trim() || undefined;
  } catch {
    value = undefined; // ps missing or pid gone: callers fall back to liveness
  }
  starts.set(pid, { value, at: Date.now() });
  return value;
}

async function isStale(record: LockRecord): Promise<boolean> {
  // A dead host says nothing about detached descendants or pre-spawn intent.
  if (record.execution && record.execution.cleanup?.state !== "stopped")
    return false;
  return ownerIsDead(record);
}

export async function ownerIsDead(record: LockRecord): Promise<boolean> {
  if (record.startIdentity) {
    const inspection = await inspectProcess({
      pid: record.pid,
      startIdentity: record.startIdentity,
    });
    return ["gone", "reused", "zombie"].includes(inspection.state);
  }
  if (!pidAlive(record.pid)) return true;
  if (!record.processStart) return false;
  const now = await processStart(record.pid, true);
  return now !== undefined && now !== record.processStart;
}

export function holderOf(record: LockRecord): LeaseHolder {
  return {
    runId: record.execution?.runId ?? `pid-${record.pid}`,
    ...(record.execution?.toolCallId
      ? { toolCallId: record.execution.toolCallId }
      : {}),
    tool: record.tool,
    label: record.label,
  };
}

const UNKNOWN: LockRecord = {
  pid: -1,
  tool: "unknown",
  label: "unreadable lock file",
  createdAt: 0,
};

function validIdentity(value: unknown): boolean {
  if (!value || typeof value !== "object") return false;
  const identity = value as ProcessIdentity;
  return (
    Number.isSafeInteger(identity.pid) &&
    identity.pid > 0 &&
    typeof identity.startIdentity === "string" &&
    identity.startIdentity.length > 0
  );
}

function isRecord(value: unknown): value is LockRecord {
  if (typeof value !== "object" || value === null) return false;
  const record = value as LockRecord;
  if (
    !Number.isSafeInteger(record.pid) ||
    record.pid <= 0 ||
    typeof record.tool !== "string" ||
    (record.startIdentity !== undefined &&
      (typeof record.startIdentity !== "string" || !record.startIdentity))
  )
    return false;
  if (!("execution" in record)) return true;
  const execution = record.execution;
  if (
    !execution ||
    typeof execution !== "object" ||
    typeof execution.runId !== "string" ||
    typeof execution.executionAttemptId !== "string" ||
    !execution.executionAttemptId ||
    typeof execution.execId !== "string"
  )
    return false;
  if (
    execution.process &&
    (!Number.isSafeInteger(execution.process.pid) ||
      execution.process.pid <= 0 ||
      (execution.process.startIdentity !== undefined &&
        !validIdentity(execution.process)) ||
      (execution.process.guardianProtocol !== undefined &&
        execution.process.guardianProtocol !== "agentkit-command-guardian-v1"))
  )
    return false;
  const discovery = execution.discovery;
  if (
    discovery &&
    (!Array.isArray(discovery.identities) ||
      !discovery.identities.every(validIdentity) ||
      typeof discovery.complete !== "boolean" ||
      (discovery.closed !== undefined &&
        typeof discovery.closed !== "boolean") ||
      (discovery.sealed !== undefined &&
        typeof discovery.sealed !== "boolean") ||
      ((discovery.closed || discovery.sealed) && !discovery.complete))
  )
    return false;
  if (!("cleanup" in execution)) return true;
  const cleanup = execution.cleanup;
  return (
    !!cleanup &&
    typeof cleanup === "object" &&
    cleanup.executionAttemptId === execution.executionAttemptId &&
    ["stopped", "survivors", "unknown"].includes(cleanup.state) &&
    Array.isArray(cleanup.survivors) &&
    cleanup.survivors.every((pid) => Number.isSafeInteger(pid) && pid > 0) &&
    (cleanup.state !== "stopped" || cleanup.survivors.length === 0)
  );
}

export async function remove(path: string): Promise<void> {
  try {
    await unlink(path);
  } catch (error) {
    if (code(error) !== "ENOENT") throw error;
  }
}

export async function readRecord(
  path: string,
): Promise<LockRecord | "gone" | "corrupt"> {
  let text: string;
  try {
    text = await readFile(path, "utf8");
  } catch (error) {
    if (code(error) === "ENOENT") return "gone";
    throw error;
  }
  try {
    const parsed: unknown = JSON.parse(text);
    return isRecord(parsed) ? parsed : "corrupt";
  } catch {
    return "corrupt";
  }
}

async function reclaim(
  path: string,
  judged: LockRecord,
): Promise<LockRecord | undefined> {
  await withRecordClaim(path, judged, async () => {
    if (await isStale(judged)) await remove(path);
  });
  const current = await readRecord(path);
  return current === "gone"
    ? undefined
    : current === "corrupt"
      ? UNKNOWN
      : current;
}

/** The live owner of a lock file, or undefined (also after reclaiming a stale one). */
export async function liveOwner(
  path: string,
  _onError: OnError,
): Promise<LockRecord | undefined> {
  const record = await readRecord(path);
  if (record === "gone") return undefined;
  if (record === "corrupt") {
    // It may contain damaged execution ownership. Age is not cleanup proof.
    return UNKNOWN;
  }
  if (!(await isStale(record))) return record;
  return reclaim(path, record);
}
