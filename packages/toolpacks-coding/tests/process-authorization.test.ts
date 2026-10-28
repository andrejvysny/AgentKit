import { afterEach, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { createLockDirWorkspaceCoordinator } from "../src/coordination-lockdir.js";
import { isExecutionLease } from "../src/coordination.js";
import { createCodingToolPack } from "../src/pack.js";
import { cleanup, tempDir } from "./support.js";

afterEach(cleanup);
const holder = {
  runId: "holder",
  tool: "write_file",
  label: "update bound input",
};
const limits = { profile: "small", maxBytes: 1 << 20 } as const;

test.each(["changed", "exception"] as const)(
  "remembered authorization %s while waiting refuses spawn without intent",
  async (mode) => {
    const root = tempDir(),
      dir = tempDir("locks-");
    let notify: (() => void) | undefined;
    const waiting = new Promise<void>((resolve) => {
      notify = resolve;
    });
    const base = createLockDirWorkspaceCoordinator({
      dir,
      observer: { onWaiting: () => notify?.() },
    });
    const held = await base.acquireWorkspace({ root, holder });
    let admitted = 0,
      checks = 0,
      valid = true;
    const coordinator = {
      ...base,
      async acquireWorkspace(
        input: Parameters<typeof base.acquireWorkspace>[0],
      ) {
        const lease = await base.acquireWorkspace(input);
        if (!isExecutionLease(lease)) throw new Error("missing durable lease");
        return {
          release: () => lease.release(),
          spawned: (identity: Parameters<typeof lease.spawned>[0]) =>
            lease.spawned(identity),
          finishExecution: (
            result: Parameters<typeof lease.finishExecution>[0],
          ) => lease.finishExecution(result),
          async beginExecution(
            intent: Parameters<typeof lease.beginExecution>[0],
          ) {
            admitted++;
            await lease.beginExecution(intent);
          },
        };
      },
    };
    const pack = createCodingToolPack({
      workspace: { root },
      sandbox: "off",
      coordinator,
    });
    const command = (await pack.contribute({ bindings: [], limits })).find(
      (tool) => tool.definition.name === "run_command",
    );
    if (!command) throw new Error("command missing");
    const result = command.execute(
      {
        runId: "run",
        bindings: [],
        limits,
        metadata: {
          revalidateExecutionAuthorization: async () => {
            checks++;
            if (mode === "exception")
              throw new Error("fingerprint unavailable");
            return valid;
          },
        },
      },
      { command: "touch spawned" },
    );
    await waiting;
    expect(checks).toBe(0);
    valid = false;
    await held.release();
    expect((await result).modelData).toContain("approval_context_changed");
    expect(checks).toBe(1);
    expect(admitted).toBe(0);
    expect(existsSync(join(root, "spawned"))).toBe(false);
    const next = await base.acquireWorkspace({ root, holder, maxWaitMs: 100 });
    await next.release();
    expect(await pack.drain()).toEqual([]);
    await pack.dispose?.();
  },
);

test("unchanged remembered authorization proceeds after exclusive admission", async () => {
  const root = tempDir();
  const pack = createCodingToolPack({ workspace: { root }, sandbox: "off" });
  const command = (await pack.contribute({ bindings: [], limits })).find(
    (tool) => tool.definition.name === "run_command",
  );
  if (!command) throw new Error("command missing");
  let checks = 0;
  const result = await command.execute(
    {
      runId: "run",
      bindings: [],
      limits,
      metadata: {
        revalidateExecutionAuthorization: async () => {
          checks++;
          return true;
        },
      },
    },
    { command: "echo authorized" },
  );
  expect(result.ok).toBe(true);
  expect(checks).toBe(1);
  expect(result.modelData).toContain("authorized");
  await pack.dispose?.();
});
