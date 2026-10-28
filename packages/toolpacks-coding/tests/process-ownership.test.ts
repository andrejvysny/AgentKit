import { afterEach, expect, test } from "bun:test";
import { type ChildProcess, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { createLockDirWorkspaceCoordinator } from "../src/coordination-lockdir.js";
import {
  createInProcessWorkspaceCoordinator,
  isExecutionLease,
  type ExecutionLease,
} from "../src/coordination.js";
import { createCodingToolPack } from "../src/pack.js";
import { terminate } from "../src/process.js";
import { cleanup, harness, tempDir } from "./support.js";

const children: ChildProcess[] = [];
const pids: number[] = [];
const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));
const holder = {
  runId: "run",
  toolCallId: "call",
  tool: "run_command",
  label: "fixture",
};
const intent = {
  runId: "run",
  toolCallId: "call",
  executionAttemptId: "attempt",
  execId: `${process.pid}:attempt`,
};
const marker = (dir: string, root: string): string =>
  join(dir, createHash("sha256").update(root).digest("hex"), "exclusive.lock");

afterEach(() => {
  for (const child of children.splice(0)) child.kill("SIGKILL");
  for (const pid of pids.splice(0)) {
    try {
      process.kill(pid, "SIGKILL");
    } catch {
      /* gone */
    }
  }
  cleanup();
});

async function leaseFor(dir: string, root: string): Promise<ExecutionLease> {
  const lease = await createLockDirWorkspaceCoordinator({
    dir,
    pollMs: 10,
  }).acquireWorkspace({ root, holder });
  if (!isExecutionLease(lease)) throw new Error("missing execution lease");
  return lease;
}

async function owner(
  dir: string,
  root: string,
  mode: "intent" | "process",
): Promise<ChildProcess> {
  const child = spawn(
    process.execPath,
    [join(import.meta.dir, "fixtures/execution-owner.ts"), dir, root, mode],
    { stdio: ["ignore", "pipe", "inherit"] },
  );
  children.push(child);
  await new Promise<void>((resolve, reject) => {
    child.once("error", reject);
    child.once("exit", () =>
      reject(new Error("fixture owner exited before ready")),
    );
    child.stdout?.on("data", (data: Buffer) => {
      const match = /ready:(\d+)/.exec(data.toString());
      if (match) {
        if (Number(match[1])) pids.push(Number(match[1]));
        resolve();
      }
    });
  });
  return child;
}

test("execution intent is durable under the blocking marker before spawn", async () => {
  const root = tempDir(),
    dir = tempDir("locks-");
  const lease = await leaseFor(dir, root);
  await lease.beginExecution(intent);
  expect(JSON.parse(readFileSync(marker(dir, root), "utf8")).execution).toEqual(
    intent,
  );
  await lease.finishExecution({
    executionAttemptId: "attempt",
    state: "stopped",
    survivors: [],
  });
  await lease.release();
  expect(existsSync(marker(dir, root))).toBe(false);
});

test.each(["survivors", "unknown"] as const)(
  "%s persists exclusion without release gap",
  async (state) => {
    const root = tempDir(),
      dir = tempDir("locks-");
    const coordinator = createLockDirWorkspaceCoordinator({ dir, pollMs: 10 });
    const lease = await leaseFor(dir, root);
    await lease.beginExecution(intent);
    await lease.finishExecution({
      executionAttemptId: "attempt",
      state,
      survivors: state === "survivors" ? [12345] : [],
    });
    await expect(lease.release()).rejects.toMatchObject({
      code: "workspace_quarantined",
    });
    expect(existsSync(marker(dir, root))).toBe(true);
    await expect(
      coordinator.acquireFile({
        root,
        path: join(root, "file"),
        holder,
        maxWaitMs: 50,
      }),
    ).rejects.toMatchObject({ code: "workspace_busy" });
    expect((await coordinator.reconcileWorkspace?.(root))?.[0]?.state).toBe(
      state,
    );
  },
);

test.each(["intent", "process"] as const)(
  "dead owner cannot reclaim unresolved %s (real process)",
  async (mode) => {
    const root = tempDir(),
      dir = tempDir("locks-");
    const child = await owner(dir, root, mode);
    const exited = new Promise<void>((resolve) =>
      child.once("exit", () => resolve()),
    );
    child.kill("SIGKILL");
    await exited;
    const coordinator = createLockDirWorkspaceCoordinator({ dir, pollMs: 10 });
    expect((await coordinator.reconcileWorkspace?.(root))?.[0]?.state).toBe(
      "unknown",
    );
    await expect(
      coordinator.acquireWorkspace({ root, holder, maxWaitMs: 80 }),
    ).rejects.toMatchObject({ code: "workspace_busy" });
    expect(existsSync(marker(dir, root))).toBe(true);
  },
);

test("inspection failure is unknown even with zero discovered pids", async () => {
  expect(await terminate({ find: async () => null, graceMs: 1 })).toMatchObject(
    { state: "unknown", survivors: [], scan: "unsupported" },
  );
  expect(
    await terminate({
      find: async () => {
        throw new Error("permission denied");
      },
      graceMs: 1,
    }),
  ).toMatchObject({ state: "unknown" });
});

test("damaged ownership remains blocked irrespective of file age", async () => {
  const root = tempDir(),
    dir = tempDir("locks-");
  mkdirSync(join(marker(dir, root), ".."), { recursive: true });
  writeFileSync(marker(dir, root), "not json");
  const coordinator = createLockDirWorkspaceCoordinator({ dir, pollMs: 10 });
  expect((await coordinator.reconcileWorkspace?.(root))?.[0]?.state).toBe(
    "unknown",
  );
  await expect(
    coordinator.acquireWorkspace({ root, holder, maxWaitMs: 40 }),
  ).rejects.toMatchObject({ code: "workspace_busy" });
  expect(readFileSync(marker(dir, root), "utf8")).toBe("not json");
});

test("legacy profile ownership becomes durable shared quarantine", async () => {
  const root = tempDir(),
    legacy = tempDir("legacy-"),
    dir = tempDir("locks-");
  mkdirSync(join(marker(legacy, root), ".."), { recursive: true });
  writeFileSync(
    marker(legacy, root),
    JSON.stringify({
      pid: 999999,
      tool: "run_command",
      label: "old",
      createdAt: 1,
    }),
  );
  const coordinator = createLockDirWorkspaceCoordinator({
    dir,
    legacyDirs: [legacy],
    pollMs: 10,
  });
  expect((await coordinator.reconcileWorkspace?.(root))?.[0]).toMatchObject({
    state: "unknown",
  });
  const restarted = createLockDirWorkspaceCoordinator({ dir, pollMs: 10 });
  await expect(
    restarted.acquireWorkspace({ root, holder, maxWaitMs: 50 }),
  ).rejects.toMatchObject({ code: "workspace_busy" });
  expect(existsSync(marker(legacy, root))).toBe(true);
});

test("contribution awaits startup reconciliation", async () => {
  const root = tempDir(),
    dir = tempDir("locks-");
  const durable = createLockDirWorkspaceCoordinator({ dir });
  let finish: (() => void) | undefined;
  const barrier = new Promise<void>((resolve) => {
    finish = resolve;
  });
  const pack = createCodingToolPack({
    workspace: { root },
    sandbox: "off",
    coordinator: {
      ...durable,
      reconcileWorkspace: async () => {
        await barrier;
        return [];
      },
    },
  });
  let ready = false;
  const tools = pack
    .contribute({
      bindings: [],
      limits: { profile: "small", maxBytes: 1 << 20 },
    })
    .then((result) => {
      ready = true;
      return result;
    });
  await sleep(40);
  expect(ready).toBe(false);
  finish?.();
  await tools;
  await pack.dispose?.();
});

test("disposal drains cancelled outer command before scratch deletion", async () => {
  const h = await harness({ limits: { commandKillGraceMs: 100 } });
  const running = h.run("run_command", {
    command: "echo ready > started; trap '' TERM; sleep 300 & wait",
  });
  for (
    let tries = 0;
    tries < 200 && !existsSync(join(h.root, "started"));
    tries++
  )
    await sleep(10);
  expect(existsSync(join(h.root, "started"))).toBe(true);
  await h.dispose();
  const result = await running;
  expect((result.data as { teardown: { state: string } }).teardown.state).toBe(
    "stopped",
  );
  expect(result.ok).toBe(false);
  expect(
    h.text(await h.run("run_command", { command: "touch late" })),
  ).toContain("coding_disposed");
  expect(existsSync(join(h.root, "late"))).toBe(false);
}, 15_000);

test("in-process shell coordinator refuses spawn", async () => {
  const h = await harness({
    coordinator: createInProcessWorkspaceCoordinator(),
  });
  const result = await h.run("run_command", { command: "touch spawned" });
  expect(h.text(result)).toContain("durable_coordination_required");
  expect(existsSync(join(h.root, "spawned"))).toBe(false);
  await h.dispose();
});

test("host execution identity reaches durable process output", async () => {
  const root = tempDir();
  const pack = createCodingToolPack({ workspace: { root }, sandbox: "off" });
  const limits = { profile: "small", maxBytes: 1 << 20 } as const;
  const command = (await pack.contribute({ bindings: [], limits })).find(
    (tool) => tool.definition.name === "run_command",
  );
  if (!command) throw new Error("command missing");
  const executionAttemptId = "a".repeat(64);
  const result = await command.execute(
    {
      runId: "run",
      toolCallId: "call",
      bindings: [],
      limits,
      metadata: { executionAttemptId },
    },
    { command: "echo ok" },
  );
  expect(result.ok).toBe(true);
  expect(result.data).toMatchObject({ executionAttemptId });
  expect(await pack.drain()).toEqual([
    { executionAttemptId, state: "stopped", survivors: [] },
  ]);
  await pack.dispose?.();
});

test("full log reports spill loss independently of retained tail", async () => {
  const h = await harness({
    limits: { commandTailMaxBytes: 16, commandMaxLogBytes: 32 },
  });
  const result = await h.run("run_command", { command: "printf '%0100d' 1" });
  expect(result.ok).toBe(true);
  expect(result.data).toMatchObject({ fullLogTruncated: true });
  expect(
    readFileSync((result.data as { fullLog: string }).fullLog).length,
  ).toBe(32);
  await h.dispose();
});
