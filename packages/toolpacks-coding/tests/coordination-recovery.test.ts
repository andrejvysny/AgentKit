import { afterEach, expect, test } from "bun:test";
import { spawn, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createLockDirWorkspaceCoordinator } from "../src/coordination-lockdir.js";
import { readRecord, type LockRecord } from "../src/coordination-record.js";
import { recoverWorkspaceMarker } from "../src/coordination-recovery.js";
import {
  captureProcessIdentity,
  inspectProcess,
  type ProcessIdentity,
} from "../src/process-identity.js";
import { discoverExecution } from "../src/process-discovery.js";
import { terminate } from "../src/process-teardown.js";
import type { WorkspaceRecoveryReason } from "../src/coordination.js";

const directories: string[] = [];
const children: ChildProcess[] = [];
const guardians: { identity: ProcessIdentity; execId: string }[] = [];
afterEach(async () => {
  for (const child of children.splice(0)) child.kill("SIGKILL");
  for (const guardian of guardians.splice(0))
    await terminate({
      initial: [guardian.identity],
      graceMs: 50,
      find: () =>
        discoverExecution({
          execId: guardian.execId,
          rootIdentity: guardian.identity,
        }),
    });
  await Promise.all(
    directories
      .splice(0)
      .map((dir) => rm(dir, { recursive: true, force: true })),
  );
});

async function pausedChild(args: string[]): Promise<ChildProcess> {
  const child = spawn(process.execPath, args, {
    stdio: ["pipe", "pipe", "inherit"],
  });
  children.push(child);
  await new Promise<void>((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", () =>
      reject(new Error("Fixture exited before barrier")),
    );
    child.stdout!.on("data", (data) => {
      if (String(data).includes("ready")) resolve();
    });
  });
  return child;
}

async function crash(child: ChildProcess): Promise<void> {
  const exited = new Promise<void>((resolve) => {
    child.once("exit", () => resolve());
  });
  child.kill("SIGKILL");
  await exited;
}

async function fixture(): Promise<{
  path: string;
  root: string;
  dir: string;
  record: LockRecord;
  identity: ProcessIdentity;
}> {
  const root = await mkdtemp(join(tmpdir(), "coding-recovery-"));
  directories.push(root);
  const dir = join(root, "locks");
  const workspace = join(
    dir,
    createHash("sha256")
      .update(await realpath(root))
      .digest("hex"),
  );
  await mkdir(workspace, { recursive: true });
  const child = await pausedChild([
    "-e",
    'console.log("ready");setInterval(()=>{},60000)',
  ]);
  const identity = await captureProcessIdentity(child.pid!);
  if (!identity) throw new Error("Fixture process identity unavailable");
  await crash(child);
  const record: LockRecord = {
    lockId: "recovery-generation",
    pid: identity.pid,
    startIdentity: identity.startIdentity,
    tool: "run_command",
    label: "fixture",
    createdAt: 1,
    execution: {
      runId: "run",
      execId: "exec",
      executionAttemptId: "attempt",
      process: identity,
      discovery: { identities: [identity], complete: true, closed: true },
      cleanup: {
        executionAttemptId: "attempt",
        state: "unknown",
        survivors: [],
      },
    },
  };
  const path = join(workspace, "exclusive.lock");
  await writeFile(path, JSON.stringify(record));
  return { path, root, dir, record, identity };
}

test("explicit recovery resolves a complete closed census after owner exit", async () => {
  const { path, root, dir } = await fixture();
  const coordinator = createLockDirWorkspaceCoordinator({ dir });
  const result = await coordinator.recoverWorkspace!(root);
  expect(result.state).toBe("recovered");
  expect(result.records).toEqual([
    {
      marker: "exclusive",
      executionAttemptId: "attempt",
      status: "recovered",
      reason: "verified_stopped",
    },
  ]);
  expect(await readRecord(path)).toBe("gone");
  expect((await coordinator.recoverWorkspace!(root)).state).toBe("clear");
});

for (const boundary of ["after-verification", "after-persistence"]) {
  test(`recovery crash ${boundary} preserves proof for successor`, async () => {
    const { path, root, dir } = await fixture();
    await crash(
      await pausedChild([
        join(import.meta.dir, "fixtures/recover-lock.ts"),
        path,
        boundary,
      ]),
    );
    const before = await readRecord(path);
    expect(before).not.toBe("gone");
    const coordinator = createLockDirWorkspaceCoordinator({ dir });
    expect((await coordinator.recoverWorkspace!(root)).state).not.toBe(
      "blocked",
    );
    expect(await readRecord(path)).toBe("gone");
  });
}

test("open discovery, missing identities, live identities and unknown inspection never clear quarantine", async () => {
  const { path, record, identity } = await fixture();
  const cases: { change: LockRecord; reason: WorkspaceRecoveryReason }[] = [
    {
      change: {
        ...record,
        execution: {
          ...record.execution!,
          discovery: { identities: [identity], complete: true, closed: false },
        },
      },
      reason: "discovery_open",
    },
    {
      change: {
        ...record,
        execution: { ...record.execution!, process: { pid: identity.pid } },
      },
      reason: "process_identity_missing",
    },
    {
      change: {
        ...record,
        execution: {
          ...record.execution!,
          discovery: { identities: [identity], complete: false, closed: false },
        },
      },
      reason: "discovery_incomplete",
    },
  ];
  for (const { change, reason } of cases) {
    await writeFile(path, JSON.stringify(change));
    expect(
      (await recoverWorkspaceMarker(path, "exclusive")).record?.reason,
    ).toBe(reason);
    expect(await readRecord(path)).toEqual(change);
  }
  const self = await captureProcessIdentity(process.pid);
  if (!self) throw new Error("Test identity unavailable");
  await writeFile(
    path,
    JSON.stringify({
      ...record,
      execution: {
        ...record.execution!,
        discovery: {
          identities: [identity, self],
          complete: true,
          closed: true,
        },
      },
    }),
  );
  expect((await recoverWorkspaceMarker(path, "exclusive")).record?.reason).toBe(
    "process_live",
  );
  await writeFile(
    path,
    JSON.stringify({ ...record, lockId: "inspection-generation" }),
  );
  expect(
    (
      await recoverWorkspaceMarker(path, "exclusive", {
        inspect: async () => ({ state: "unknown" }),
      })
    ).record?.reason,
  ).toBe("inspection_unknown");
  expect(await readRecord(path)).not.toBe("gone");
});

test("dead host guardian is recovered by identity without replaying its command", async () => {
  const root = await mkdtemp(join(tmpdir(), "coding-guardian-recovery-"));
  directories.push(root);
  const dir = join(root, "locks");
  const owner = await pausedChild([
    join(import.meta.dir, "fixtures/guardian-owner.ts"),
    dir,
    root,
  ]);
  const path = join(
    dir,
    createHash("sha256")
      .update(await realpath(root))
      .digest("hex"),
    "exclusive.lock",
  );
  const record = await readRecord(path);
  if (
    record === "gone" ||
    record === "corrupt" ||
    !record.execution?.process?.startIdentity
  )
    throw new Error("Persisted guardian identity missing");
  const identity = {
    pid: record.execution.process.pid,
    startIdentity: record.execution.process.startIdentity,
  };
  guardians.push({ identity, execId: record.execution.execId });
  expect(record.execution.process.guardianProtocol).toBe(
    "agentkit-command-guardian-v1",
  );
  await crash(owner);
  const scanner = spawn(
    process.execPath,
    [
      join(import.meta.dir, "fixtures/scan-guardian.ts"),
      record.execution.execId,
    ],
    { stdio: ["ignore", "pipe", "inherit"] },
  );
  children.push(scanner);
  let scanned = "";
  scanner.stdout!.on("data", (data: Buffer) => {
    scanned += data.toString();
  });
  const scanExit = await new Promise<number | null>((resolve, reject) => {
    scanner.once("error", reject);
    scanner.once("exit", resolve);
  });
  expect(scanExit).toBe(0);
  expect(record.execution.execId).toBe(`${owner.pid}:guardian-attempt`);
  expect(JSON.parse(scanned)).toMatchObject({
    complete: true,
    ownerPids: [owner.pid],
    guardians: expect.arrayContaining([identity.pid]),
  });
  expect(await inspectProcess(identity)).toMatchObject({
    state: "same-live",
    observation: { stopped: true },
  });
  const coordinator = createLockDirWorkspaceCoordinator({ dir });
  expect((await coordinator.recoverWorkspace!(root)).state).toBe("recovered");
  expect(await readFile(join(root, "sentinel"), "utf8")).toBe("START\n");
  const next = await coordinator.acquireWorkspace({
    root,
    holder: { runId: "next", tool: "write_file", label: "next" },
    maxWaitMs: 1000,
  });
  await next.release();
}, 10_000);

test("sealed guardian recovery survives SIGKILL after every kill and before terminal persistence", async () => {
  const root = await mkdtemp(join(tmpdir(), "coding-sealed-recovery-"));
  directories.push(root);
  const dir = join(root, "locks");
  const owner = await pausedChild([
    join(import.meta.dir, "fixtures/guardian-owner.ts"),
    dir,
    root,
  ]);
  const workspace = join(
    dir,
    createHash("sha256")
      .update(await realpath(root))
      .digest("hex"),
  );
  const path = join(workspace, "exclusive.lock");
  const original = await readRecord(path);
  if (
    original === "gone" ||
    original === "corrupt" ||
    !original.execution?.process?.startIdentity
  )
    throw new Error("Guardian identity missing");
  guardians.push({
    execId: original.execution.execId,
    identity: {
      pid: original.execution.process.pid,
      startIdentity: original.execution.process.startIdentity,
    },
  });
  await crash(owner);
  const recoverer = (boundary: string) =>
    pausedChild([
      join(import.meta.dir, "fixtures/recover-lock.ts"),
      path,
      boundary,
    ]);
  await crash(await recoverer("after-seal"));
  expect(await readRecord(path)).toEqual(original);
  const proofName = (await readdir(workspace)).find((name) =>
    name.startsWith("exclusive.lock.sealed-v1-"),
  );
  if (!proofName) throw new Error("Durable frozen census missing");
  const proof = JSON.parse(
    await readFile(join(workspace, proofName), "utf8"),
  ) as { identities: ProcessIdentity[] };
  for (let index = 1; index <= proof.identities.length; index++) {
    await crash(await recoverer(`after-kill-${index}`));
    expect(await readRecord(path)).toEqual(original);
  }
  await crash(await recoverer("after-verification"));
  const coordinator = createLockDirWorkspaceCoordinator({ dir });
  expect((await coordinator.recoverWorkspace!(root)).state).toBe("recovered");
  expect(await readRecord(path)).toBe("gone");
  expect(await readFile(join(root, "sentinel"), "utf8")).toBe("START\n");
}, 15_000);

test("malformed frozen-census booleans cannot authorize recovery", async () => {
  const { path, record, identity } = await fixture();
  const guardian: LockRecord = {
    ...record,
    execution: {
      ...record.execution!,
      process: {
        ...identity,
        guardianProtocol: "agentkit-command-guardian-v1",
      },
      discovery: { identities: [identity], complete: true, closed: false },
    },
  };
  await writeFile(path, JSON.stringify(guardian));
  const digest = createHash("sha256")
    .update(JSON.stringify(guardian))
    .digest("hex");
  await writeFile(
    `${path}.sealed-v1-${digest}`,
    JSON.stringify({
      version: 1,
      recordDigest: digest,
      executionAttemptId: "attempt",
      execId: "exec",
      identities: [identity],
      complete: 1,
      sealed: "yes",
    }),
  );
  expect((await recoverWorkspaceMarker(path, "exclusive")).record?.reason).toBe(
    "ownership_unreadable",
  );
  expect(await readRecord(path)).not.toBe("gone");
});

function guardianRecord(
  record: LockRecord,
  identity: ProcessIdentity,
  admission: "pending" | "released" | undefined,
  identities: ProcessIdentity[] = [identity],
): LockRecord {
  return {
    ...record,
    execution: {
      ...record.execution!,
      process: {
        ...identity,
        guardianProtocol: "agentkit-command-guardian-v1",
        ...(admission ? { admission } : {}),
      },
      discovery: { identities, complete: true, closed: false },
      cleanup: undefined,
    },
  };
}

for (const state of ["gone", "reused", "zombie"] as const) {
  test(`pending admission with ${state} guardian proves the command never started`, async () => {
    const { path, root, dir, record, identity } = await fixture();
    await writeFile(
      path,
      JSON.stringify(guardianRecord(record, identity, "pending")),
    );
    const marker = await recoverWorkspaceMarker(path, "exclusive", {
      inspect: async () => ({ state }),
      discover: async () => {
        throw new Error("census must not run");
      },
    });
    expect(marker.record).toMatchObject({
      status: "recovered",
      reason: "verified_stopped",
    });
    expect(marker.cleanup?.state).toBe("stopped");
    expect(await readRecord(path)).toBe("gone");
    const coordinator = createLockDirWorkspaceCoordinator({ dir });
    expect((await coordinator.recoverWorkspace!(root)).state).not.toBe(
      "blocked",
    );
  });
}

test("pending admission with live guardian does not take the shortcut", async () => {
  const { path, record, identity } = await fixture();
  await writeFile(
    path,
    JSON.stringify(guardianRecord(record, identity, "pending")),
  );
  const marker = await recoverWorkspaceMarker(path, "exclusive", {
    inspect: async () => ({ state: "same-live" }),
    discover: async () => ({ identities: [identity], complete: false }),
  });
  expect(marker.record?.reason).not.toBe("verified_stopped");
  expect(marker.record?.status).toBe("blocked");
});

test("pending admission with unknown inspection stays blocked", async () => {
  const { path, record, identity } = await fixture();
  await writeFile(
    path,
    JSON.stringify(guardianRecord(record, identity, "pending")),
  );
  const marker = await recoverWorkspaceMarker(path, "exclusive", {
    inspect: async () => ({ state: "unknown" }),
    discover: async () => ({ identities: [identity], complete: false }),
  });
  expect(marker.record?.status).toBe("blocked");
  expect(await readRecord(path)).not.toBe("gone");
});

for (const admission of ["released", undefined] as const) {
  test(`${admission ?? "absent"} admission with gone guardian stays discovery_open`, async () => {
    const { path, record, identity } = await fixture();
    await writeFile(
      path,
      JSON.stringify(guardianRecord(record, identity, admission)),
    );
    const marker = await recoverWorkspaceMarker(path, "exclusive", {
      inspect: async () => ({ state: "gone" }),
      discover: async () => ({ identities: [identity], complete: false }),
    });
    expect(marker.record?.status).toBe("blocked");
    expect(marker.record?.reason).toBe("discovery_open");
  });
}

test("pending admission with non-root discovery identity gets no shortcut", async () => {
  const { path, record, identity } = await fixture();
  const other = { pid: identity.pid + 1, startIdentity: "other" };
  await writeFile(
    path,
    JSON.stringify(
      guardianRecord(record, identity, "pending", [identity, other]),
    ),
  );
  const marker = await recoverWorkspaceMarker(path, "exclusive", {
    inspect: async () => ({ state: "gone" }),
    discover: async () => ({ identities: [identity], complete: false }),
  });
  expect(marker.record?.status).toBe("blocked");
});

test("record validation rejects unknown admission values", async () => {
  const { path, record, identity } = await fixture();
  const bad = guardianRecord(record, identity, "pending");
  bad.execution!.process!.admission = "bogus" as "pending";
  await writeFile(path, JSON.stringify(bad));
  expect(await readRecord(path)).toBe("corrupt");
  await writeFile(
    path,
    JSON.stringify(guardianRecord(record, identity, "pending")),
  );
  expect(await readRecord(path)).not.toBe("corrupt");
});
