import { afterEach, expect, test } from "bun:test";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { DEFAULT_CODING_LIMITS } from "../src/limits.js";
import { discoverExecution } from "../src/process-discovery.js";
import { captureProcessIdentity } from "../src/process-identity.js";
import { GUARDIAN_PROTOCOL, launchGuardian } from "../src/process-launcher.js";
import { runBounded, terminate } from "../src/process.js";
import { cleanup, tempDir } from "./support.js";

afterEach(cleanup);
const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

function request(command: string) {
  return {
    file: "/bin/bash",
    args: ["-c", command],
    cwd: tempDir(),
    scratch: tempDir("coding-scratch-"),
    logName: "log",
    limits: DEFAULT_CODING_LIMITS,
    timeoutMs: 5_000,
    env: { ...process.env },
  };
}

test("command and startup hooks cannot run before guardian identity persistence", async () => {
  const spec = request("printf effect > effect");
  const effect = join(spec.cwd, "effect"),
    startup = join(spec.cwd, "startup");
  const hook = join(spec.cwd, "startup.sh");
  writeFileSync(hook, `printf startup > '${startup}'\n`);
  let ready: (() => void) | undefined, release: (() => void) | undefined;
  const entered = new Promise<void>((resolve) => {
    ready = resolve;
  });
  const barrier = new Promise<void>((resolve) => {
    release = resolve;
  });
  const running = runBounded({
    ...spec,
    env: { ...spec.env, BASH_ENV: hook },
    onSpawn: async (_pid, identity, protocol) => {
      expect(identity?.startIdentity).toBeTruthy();
      expect(protocol).toBe(GUARDIAN_PROTOCOL);
      ready?.();
      await barrier;
    },
  });
  await entered;
  expect(existsSync(effect)).toBe(false);
  expect(existsSync(startup)).toBe(false);
  release?.();
  const result = await running;
  expect(result.exitCode).toBe(0);
  expect(result.teardown.state).toBe("stopped");
  expect(existsSync(effect)).toBe(true);
  expect(existsSync(startup)).toBe(true);
});

test("failed guardian identity persistence admits no command effect", async () => {
  const spec = request("touch effect");
  const result = await runBounded({
    ...spec,
    onSpawn: async () => {
      throw new Error("durability failed");
    },
  });
  expect(result.spawnError).toContain("durability failed");
  expect(existsSync(join(spec.cwd, "effect"))).toBe(false);
  expect(result.teardown.state).toBe("stopped");
});

test("guardian preserves argv literals and POSIX high exit status without inventing signals", async () => {
  const spec = request('printf "%s\\n" "$1" "$LITERAL"; exit 143');
  const literal = '$(touch injected); "quoted"';
  const result = await runBounded({
    ...spec,
    args: [...spec.args, "fixture", literal],
    env: { ...spec.env, LITERAL: literal },
  });
  expect(result.output).toContain(`${literal}\n${literal}`);
  expect(result.exitCode).toBe(143);
  expect(result.signal).toBeNull();
  expect(existsSync(join(spec.cwd, "injected"))).toBe(false);
});

test("parent control EOF freezes the guardian's current process group", async () => {
  const spec = request(
    "printf ready > started; while :; do printf tick >> counter; sleep 0.03; done",
  );
  const execId = `${process.pid}:guardian-eof-fixture`;
  const guardian = launchGuardian(spec.scratch, execId, {
    ...spec,
    env: { ...spec.env, AGENTKIT_EXEC_ID: execId },
  });
  const identity = await captureProcessIdentity(guardian.child.pid as number);
  if (!identity) throw new Error("guardian identity unavailable");
  try {
    guardian.release();
    for (
      let tries = 0;
      tries < 200 && !existsSync(join(spec.cwd, "started"));
      tries++
    )
      await sleep(10);
    expect(existsSync(join(spec.cwd, "started"))).toBe(true);
    guardian.dispose();
    await sleep(100);
    const before = readFileSync(join(spec.cwd, "counter"), "utf8");
    await sleep(150);
    expect(readFileSync(join(spec.cwd, "counter"), "utf8")).toBe(before);
  } finally {
    const result = await terminate({
      initial: [identity],
      find: () => discoverExecution({ execId, rootIdentity: identity }),
      graceMs: 100,
    });
    expect(result.state).toBe("stopped");
    guardian.dispose();
  }
}, 15_000);

test("admission is persisted pending at spawn and released only after onAdmit", async () => {
  const { createLockDirWorkspaceCoordinator } =
    await import("../src/coordination-lockdir.js");
  const { readRecord } = await import("../src/coordination-record.js");
  const { isExecutionLease } = await import("../src/coordination.js");
  const { readdirSync } = await import("node:fs");
  const spec = request("touch effect");
  const dir = tempDir("coding-locks-");
  const coordinator = createLockDirWorkspaceCoordinator({ dir });
  const lease = await coordinator.acquireWorkspace({
    root: spec.cwd,
    holder: { runId: "r", tool: "run_command", label: "l" },
    maxWaitMs: 1000,
  });
  if (!isExecutionLease(lease) || !lease.admitted)
    throw new Error("execution lease expected");
  const marker = (): string => {
    const sub = readdirSync(dir)[0]!;
    return join(dir, sub, "exclusive.lock");
  };
  const admission = async (): Promise<string | undefined> => {
    const record = await readRecord(marker());
    if (record === "gone" || record === "corrupt") return undefined;
    return record.execution?.process?.admission;
  };
  await lease.beginExecution({
    runId: "r",
    executionAttemptId: "a",
    execId: `${process.pid}:admission-order`,
  });
  const effect = join(spec.cwd, "effect");
  const seen: { spawn?: string; admit?: string; effect?: boolean } = {};
  const result = await runBounded({
    ...spec,
    execId: `${process.pid}:admission-order`,
    onSpawn: async (pid, identity, protocol) => {
      await lease.spawned({
        pid,
        ...(identity ? { startIdentity: identity.startIdentity } : {}),
        ...(protocol ? { guardianProtocol: protocol } : {}),
        admission: "pending",
      });
      seen.spawn = await admission();
    },
    onAdmit: async () => {
      seen.admit = await admission();
      seen.effect = existsSync(effect);
      await lease.admitted!();
    },
  });
  expect(result.exitCode).toBe(0);
  expect(seen).toEqual({ spawn: "pending", admit: "pending", effect: false });
  expect(await admission()).toBe("released");
  expect(existsSync(effect)).toBe(true);
  await lease.finishExecution({
    executionAttemptId: "a",
    state: "stopped",
    survivors: [],
  });
  await lease.release();
});
