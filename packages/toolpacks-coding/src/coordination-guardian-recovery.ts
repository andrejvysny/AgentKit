import { createHash } from "node:crypto";
import { open, readFile } from "node:fs/promises";
import { dirname } from "node:path";
import type {
  ExecutionCleanup,
  WorkspaceRecoveryReason,
} from "./coordination.js";
import { code, type LockRecord } from "./coordination-record.js";
import { persistJsonRecord } from "./coordination-storage.js";
import { discoverExecution } from "./process-discovery.js";
import {
  identityKey,
  inspectProcess,
  signalIdentity,
  type ProcessIdentity,
  type ProcessDiscovery,
} from "./process-identity.js";

interface FrozenProof {
  version: 1;
  recordDigest: string;
  executionAttemptId: string;
  execId: string;
  identities: ProcessIdentity[];
  complete: true;
  sealed: true;
}

export interface GuardianRecoveryDependencies {
  inspect?: typeof inspectProcess;
  discover?: typeof discoverExecution;
  afterSeal?(): Promise<void>;
  afterKill?(index: number, total: number): Promise<void>;
}

interface GuardianRecoveryResult {
  state: ExecutionCleanup["state"];
  survivors: number[];
  discovery: ProcessDiscovery & { sealed: boolean; closed: boolean };
  reason: WorkspaceRecoveryReason;
}

function result(
  identities: ProcessIdentity[],
  state: ExecutionCleanup["state"],
  reason: WorkspaceRecoveryReason,
  survivors: number[] = [],
  sealed = true,
): GuardianRecoveryResult {
  return {
    state,
    reason,
    survivors,
    discovery: {
      identities,
      complete: sealed,
      sealed,
      closed: state === "stopped",
    },
  };
}

async function allFrozen(
  identities: ProcessIdentity[],
  inspect: typeof inspectProcess,
): Promise<boolean> {
  const states = await Promise.all(
    identities.map((identity) => inspect(identity)),
  );
  return !states.some(
    (state) =>
      state.state === "unknown" ||
      (state.state === "same-live" && !state.observation?.stopped),
  );
}

function validProof(
  value: unknown,
  digest: string,
  record: LockRecord,
  root: ProcessIdentity,
): value is FrozenProof {
  if (!value || typeof value !== "object") return false;
  const proof = value as FrozenProof;
  if (
    proof.version !== 1 ||
    proof.recordDigest !== digest ||
    proof.complete !== true ||
    proof.sealed !== true ||
    proof.executionAttemptId !== record.execution!.executionAttemptId ||
    proof.execId !== record.execution!.execId ||
    !Array.isArray(proof.identities) ||
    !proof.identities.every(
      (identity) =>
        Number.isSafeInteger(identity?.pid) &&
        identity.pid > 0 &&
        typeof identity.startIdentity === "string" &&
        !!identity.startIdentity,
    )
  )
    return false;
  const known = new Set(proof.identities.map(identityKey));
  return [root, ...(record.execution!.discovery?.identities ?? [])].every(
    (identity) => known.has(identityKey(identity)),
  );
}

async function frozenCensus(
  record: LockRecord,
  root: ProcessIdentity,
  dependencies: GuardianRecoveryDependencies,
): Promise<ProcessDiscovery | undefined> {
  const inspect = dependencies.inspect ?? inspectProcess;
  const known = new Map(
    [root, ...(record.execution!.discovery?.identities ?? [])].map(
      (identity) => [identityKey(identity), identity],
    ),
  );
  const find = async (): Promise<boolean> => {
    const found = await (dependencies.discover ?? discoverExecution)({
      execId: record.execution!.execId,
      rootIdentity: root,
      known: [...known.values()],
    });
    for (const identity of found.identities)
      known.set(identityKey(identity), identity);
    return found.complete;
  };
  for (let attempt = 0; attempt < 4; attempt++) {
    if (!(await find())) return undefined;
    for (const identity of known.values())
      if (
        (await signalIdentity(identity, "SIGSTOP", inspect)).state === "unknown"
      )
        return undefined;
    const count = known.size;
    if (!(await find())) return undefined;
    if (known.size !== count) continue;
    if (!(await allFrozen([...known.values()], inspect))) return undefined;
    const guardian = await inspect(root);
    if (guardian.state !== "same-live" || !guardian.observation?.stopped)
      return undefined;
    return { identities: [...known.values()], complete: true };
  }
  return undefined;
}

async function killFrozen(
  identities: ProcessIdentity[],
  dependencies: GuardianRecoveryDependencies,
): Promise<GuardianRecoveryResult> {
  const inspect = dependencies.inspect ?? inspectProcess;
  const states = await Promise.all(
    identities.map((identity) => inspect(identity)),
  );
  if (states.some((state) => state.state === "unknown"))
    return result(identities, "unknown", "inspection_unknown");
  if (
    states.some(
      (state) => state.state === "same-live" && !state.observation?.stopped,
    )
  )
    return result(identities, "unknown", "process_live", [], false);
  const inspectFrozen: typeof inspectProcess = async (identity) => {
    const state = await inspect(identity);
    return state.state === "same-live" && !state.observation?.stopped
      ? { state: "unknown" }
      : state;
  };
  for (let index = 0; index < identities.length; index++) {
    if (
      (await signalIdentity(identities[index]!, "SIGKILL", inspectFrozen))
        .state === "unknown"
    )
      return result(identities, "unknown", "inspection_unknown");
    await dependencies.afterKill?.(index + 1, identities.length);
  }
  return waitForFrozenExit(identities, inspect);
}

async function waitForFrozenExit(
  identities: ProcessIdentity[],
  inspect: typeof inspectProcess,
): Promise<GuardianRecoveryResult> {
  const deadline = Date.now() + 500;
  for (;;) {
    const final = await Promise.all(
      identities.map((identity) => inspect(identity)),
    );
    if (final.some((state) => state.state === "unknown"))
      return result(identities, "unknown", "inspection_unknown");
    const live = identities.filter(
      (_, index) => final[index]!.state === "same-live",
    );
    if (!live.length) return result(identities, "stopped", "verified_stopped");
    if (Date.now() >= deadline)
      return result(
        identities,
        "survivors",
        "process_live",
        live.map((identity) => identity.pid),
      );
    await new Promise<void>((resolve) => setTimeout(resolve, 25));
  }
}

async function readProof(
  path: string,
  digest: string,
  record: LockRecord,
): Promise<unknown> {
  const execution = record.execution!;
  if (execution.discovery?.sealed && execution.discovery.complete) {
    return {
      version: 1,
      recordDigest: digest,
      executionAttemptId: execution.executionAttemptId,
      execId: execution.execId,
      ...execution.discovery,
    };
  }
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    return code(error) === "ENOENT" ? undefined : null;
  }
}

async function prepareProof(
  path: string,
  digest: string,
  record: LockRecord,
  root: ProcessIdentity,
  dependencies: GuardianRecoveryDependencies,
): Promise<unknown> {
  const execution = record.execution!;
  let proof = await readProof(path, digest, record);
  if (proof === undefined) {
    const guardian = await (dependencies.inspect ?? inspectProcess)(root);
    const census =
      guardian.state === "same-live"
        ? await frozenCensus(record, root, dependencies)
        : undefined;
    if (census) {
      proof = {
        version: 1,
        recordDigest: digest,
        executionAttemptId: execution.executionAttemptId,
        execId: execution.execId,
        identities: census.identities,
        complete: true,
        sealed: true,
      };
      await persistJsonRecord(path, proof, true);
      proof = JSON.parse(await readFile(path, "utf8"));
    }
  }
  return proof;
}

async function syncEvidence(path: string): Promise<void> {
  const evidence = await open(path, "r");
  try {
    await evidence.sync();
  } finally {
    await evidence.close();
  }
  const directory = await open(dirname(path), "r");
  try {
    await directory.sync();
  } finally {
    await directory.close();
  }
}

/** A durable frozen census precedes irreversible signals; recovery never resumes these identities. */
export async function recoverGuardianExecution(
  path: string,
  record: LockRecord,
  dependencies: GuardianRecoveryDependencies,
): Promise<GuardianRecoveryResult> {
  const execution = record.execution!;
  const root = {
    pid: execution.process!.pid,
    startIdentity: execution.process!.startIdentity!,
  };
  const known = execution.discovery?.identities ?? [];
  if (
    execution.process!.admission === "pending" &&
    known.every(
      (identity) =>
        identity.pid === root.pid &&
        identity.startIdentity === root.startIdentity,
    )
  ) {
    // Admission is persisted before release, so "pending" proves the command never started.
    const state = (await (dependencies.inspect ?? inspectProcess)(root)).state;
    if (state === "gone" || state === "reused" || state === "zombie")
      return result([root], "stopped", "verified_stopped");
  }
  const digest = createHash("sha256")
    .update(JSON.stringify(record))
    .digest("hex");
  const proofPath = `${path}.sealed-v1-${digest}`;
  const proof = await prepareProof(
    proofPath,
    digest,
    record,
    root,
    dependencies,
  );
  if (!validProof(proof, digest, record, root))
    return {
      state: "unknown",
      survivors: [],
      reason: proof === undefined ? "discovery_open" : "ownership_unreadable",
      discovery: {
        identities: execution.discovery?.identities ?? [root],
        complete: false,
        sealed: false,
        closed: false,
      },
    };
  await syncEvidence(execution.discovery?.sealed ? path : proofPath);
  await dependencies.afterSeal?.();
  return killFrozen(proof.identities, dependencies);
}
