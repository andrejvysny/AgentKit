import { afterEach, expect, test } from "bun:test";
import { spawn, type ChildProcess } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { createEnv } from "../src/context.js";
import { createLockDirWorkspaceCoordinator } from "../src/coordination-lockdir.js";
import { readRecord } from "../src/coordination-record.js";
import {
  isExecutionLease,
  type WorkspaceCoordinator,
} from "../src/coordination.js";
import { DEFAULT_CODING_LIMITS } from "../src/limits.js";
import { createCodingToolPack } from "../src/pack.js";
import { discoverExecution } from "../src/process-discovery.js";
import type { ProcessIdentity } from "../src/process-identity.js";
import { terminate } from "../src/process-teardown.js";
import { cleanup, harness, tempDir, type Harness } from "./support.js";

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
  cleanup();
});

function marker(dir: string, root: string): string {
  return join(
    dir,
    createHash("sha256").update(root).digest("hex"),
    "exclusive.lock",
  );
}

async function foreignOwner(
  dir: string,
  root: string,
  guardian: boolean,
): Promise<void> {
  const file = guardian ? "guardian-owner.ts" : "execution-owner.ts";
  const child = spawn(
    process.execPath,
    [join(import.meta.dir, "fixtures", file), dir, root, "intent"],
    { stdio: ["ignore", "pipe", "inherit"] },
  );
  children.push(child);
  await new Promise<void>((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", () =>
      reject(new Error("Owner exited before persistence barrier")),
    );
    child.stdout!.on("data", (data) => {
      if (String(data).includes("ready")) resolve();
    });
  });
  const record = await readRecord(marker(dir, root));
  if (
    guardian &&
    record !== "gone" &&
    record !== "corrupt" &&
    record.execution?.process?.startIdentity
  )
    guardians.push({
      execId: record.execution.execId,
      identity: {
        pid: record.execution.process.pid,
        startIdentity: record.execution.process.startIdentity,
      },
    });
  const exited = new Promise<void>((resolve) => {
    child.once("exit", () => resolve());
  });
  child.kill("SIGKILL");
  await exited;
}

async function blockedWaiter(
  root: string,
  coordinator: WorkspaceCoordinator,
  waiting: Promise<void>,
): Promise<Harness> {
  const pack = await harness({ root, coordinator });
  const cancel = new AbortController();
  const result = pack.run(
    "run_command",
    { command: "printf UNEXPECTED > retry-effect" },
    cancel.signal,
  );
  await waiting;
  cancel.abort();
  expect((await result).ok).toBe(false);
  expect(existsSync(join(root, "retry-effect"))).toBe(false);
  return pack;
}

test("inherited quarantine blocks a cancelled waiter without becoming current pack cleanup", async () => {
  const root = tempDir(),
    dir = tempDir("lifecycle-locks-");
  await foreignOwner(dir, root, false);
  const original = await readFile(marker(dir, root), "utf8");
  const waiting = Promise.withResolvers<void>();
  const coordinator = createLockDirWorkspaceCoordinator({
    dir,
    pollMs: 10,
    observer: { onWaiting: () => waiting.resolve() },
  });
  const pack = await blockedWaiter(root, coordinator, waiting.promise);
  await expect(pack.dispose()).resolves.toBeUndefined();
  expect(await readFile(marker(dir, root), "utf8")).toBe(original);
});

test("verified foreign recovery permits the observing pack to dispose without replay", async () => {
  const root = tempDir(),
    dir = tempDir("lifecycle-recovery-");
  await foreignOwner(dir, root, true);
  const waiting = Promise.withResolvers<void>();
  const coordinator = createLockDirWorkspaceCoordinator({
    dir,
    pollMs: 10,
    observer: { onWaiting: () => waiting.resolve() },
  });
  const pack = await blockedWaiter(root, coordinator, waiting.promise);
  expect((await coordinator.recoverWorkspace!(root)).state).toBe("recovered");
  expect((await coordinator.recoverWorkspace!(root)).state).toBe("clear");
  await expect(pack.dispose()).resolves.toBeUndefined();
  expect(existsSync(marker(dir, root))).toBe(false);
  expect(await readFile(join(root, "sentinel"), "utf8")).toBe("START\n");
  expect(existsSync(join(root, "retry-effect"))).toBe(false);
}, 10_000);

test("a current pack's own uncertain cleanup still blocks disposal", async () => {
  const root = tempDir(),
    dir = tempDir("lifecycle-owned-");
  const durable = createLockDirWorkspaceCoordinator({ dir });
  const coordinator: WorkspaceCoordinator = {
    ...durable,
    async acquireWorkspace(input) {
      const lease = await durable.acquireWorkspace(input);
      if (!isExecutionLease(lease))
        throw new Error("Execution lease unavailable");
      return {
        beginExecution: lease.beginExecution.bind(lease),
        spawned: lease.spawned.bind(lease),
        observed: lease.observed?.bind(lease),
        release: lease.release.bind(lease),
        finishExecution: async () => {
          throw new Error("terminal cleanup persistence failed");
        },
      };
    },
  };
  const pack = await harness({ root, coordinator });
  const result = await pack.run("run_command", {
    command: "printf OWNED > owned-effect",
  });
  expect(result.ok).toBe(false);
  await expect(pack.dispose()).rejects.toMatchObject({
    code: "workspace_quarantined",
    cleanup: [{ state: "unknown" }],
  });
  expect(existsSync(marker(dir, root))).toBe(true);
});

test("startup cleanup adopted by the current pack still blocks disposal", async () => {
  const root = tempDir(),
    dir = tempDir("lifecycle-startup-");
  const coordinator = createLockDirWorkspaceCoordinator({ dir });
  const env = createEnv({
    workspaceRoot: root,
    limits: { ...DEFAULT_CODING_LIMITS },
    allowSensitive: false,
    clock: Date.now,
    sandbox: "off",
    network: false,
    allowGitWrites: false,
    testCommands: [],
    platform: process.platform,
    sandboxExecPath: "/unused",
    coordinator,
    coordinationWaitMs: 100,
    startupCleanup: Promise.resolve([
      {
        executionAttemptId: "adopted-startup",
        state: "unknown",
        survivors: [],
      },
    ]),
  });
  await env.root();
  await expect(env.dispose()).rejects.toMatchObject({
    code: "workspace_quarantined",
    cleanup: [{ executionAttemptId: "adopted-startup", state: "unknown" }],
  });
  expect((await coordinator.reconcileWorkspace!(root))[0]?.state).toBe(
    "unknown",
  );
});

test("workspace reconciliation inspection errors remain fail closed", async () => {
  const root = tempDir(),
    dir = tempDir("lifecycle-inspection-");
  const pack = createCodingToolPack({
    workspace: { root },
    sandbox: "off",
    coordinator: {
      ...createLockDirWorkspaceCoordinator({ dir }),
      reconcileWorkspace: async () => {
        throw new Error("workspace inspection denied");
      },
    },
  });
  await expect(
    pack.contribute({
      bindings: [],
      limits: { profile: "small", maxBytes: 1024 },
    }),
  ).rejects.toThrow("workspace inspection denied");
  await pack.dispose?.();
});
