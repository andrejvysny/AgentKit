import { afterEach, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Database } from "bun:sqlite";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import type { AssistantStore, ToolExecutionAdmission } from "@agentkit/host";
import { resolveToolLimits } from "@agentkit/core";
import { SqliteAssistantStore } from "../src/index.js";

const dirs: string[] = [];
afterEach(() => {
  for (const dir of dirs.splice(0))
    rmSync(dir, { recursive: true, force: true });
});
function path(): string {
  const dir = mkdtempSync(join(tmpdir(), "execution-store-"));
  dirs.push(dir);
  return join(dir, "store.db");
}

async function claim(store: AssistantStore) {
  await store.conversations.createChat({
    id: "chat",
    metadata: { grants: ["preserved"] },
  });
  await store.conversations.appendMessage({
    chatId: "chat",
    role: "user",
    content: "preserved history",
  });
  await store.tasks.createTask({
    taskId: "run",
    scopeId: "chat",
    kind: "test",
    payload: {},
  });
  const claimed = (await store.tasks.claimNext({
    ownerId: "worker",
    now: new Date(),
    scopesBusy: [],
  }))!;
  const fence = { taskId: "run", leaseToken: claimed.lease.leaseToken };
  const admission: ToolExecutionAdmission = {
    executionId: "execution",
    runId: "run",
    toolCallId: "call",
    toolName: "write",
    argsSha256: "a".repeat(64),
    contextSha256: "b".repeat(64),
    attemptId: claimed.attempt.attemptId,
    approvalRequestId: "approval",
  };
  return { fence, admission };
}

for (const adapter of ["memory", "sqlite"] as const) {
  it(`${adapter}: atomic claims, unique approval consumption, stale-fence rejection`, async () => {
    const store =
      adapter === "memory"
        ? new MemoryAssistantStore()
        : new SqliteAssistantStore(":memory:");
    try {
      const { fence, admission } = await claim(store);
      expect(
        await Promise.all([
          store.toolExecutions!.admit(admission, fence),
          store.toolExecutions!.admit(admission, fence),
        ]),
      ).toEqual([true, false]);
      expect(
        await store.toolExecutions!.admit(
          { ...admission, executionId: "another-execution" },
          fence,
        ),
      ).toBe(false);
      const result = {
        ok: true,
        data: { marker: 1 },
        sources: [],
        warnings: [],
        truncated: false,
        limits: resolveToolLimits({ preference: "small" }),
      };
      await store.toolExecutions!.complete(
        admission.executionId,
        result,
        fence,
      );
      expect((await store.toolExecutions!.listByRun("run"))[0]?.result).toEqual(
        result,
      );
      await store.tasks.interruptTask!("run", "stop");
      await expect(
        store.toolExecutions!.admit(
          { ...admission, executionId: "stale", approvalRequestId: "stale" },
          fence,
        ),
      ).rejects.toMatchObject({ code: "lease_lost" });
      await expect(
        store.toolExecutions!.complete(admission.executionId, result, fence),
      ).rejects.toMatchObject({ code: "lease_lost" });
      expect(await store.toolExecutions!.listByRun("run")).toHaveLength(1);
      await store.tasks.transitionTask("run", ["interrupted"], "cancelled");
      await store.tasks.deleteByScope("chat");
      expect(await store.toolExecutions!.listByRun("run")).toEqual([]);
    } finally {
      if (store instanceof SqliteAssistantStore) store.close();
    }
  });
}

it("SQLite: two handles cannot consume one authorization twice", async () => {
  const file = path();
  const first = new SqliteAssistantStore(file);
  const { fence, admission } = await claim(first);
  const second = new SqliteAssistantStore(file);
  try {
    const results = await Promise.all([
      first.toolExecutions.admit(admission, fence),
      second.toolExecutions.admit(
        { ...admission, executionId: "other" },
        fence,
      ),
    ]);
    expect(results.filter(Boolean)).toHaveLength(1);
    expect(await first.toolExecutions.listByRun("run")).toHaveLength(1);
  } finally {
    first.close();
    second.close();
  }
});

it("SQLite: admission joins its transaction and rolls back on failure", async () => {
  const store = new SqliteAssistantStore(":memory:");
  try {
    const { fence, admission } = await claim(store);
    await expect(
      store.transaction(async (tx) => {
        expect(await tx.toolExecutions!.admit(admission, fence)).toBe(true);
        throw new Error("rollback");
      }),
    ).rejects.toThrow("rollback");
    expect(await store.toolExecutions.listByRun("run")).toEqual([]);
    expect(await store.toolExecutions.admit(admission, fence)).toBe(true);
  } finally {
    store.close();
  }
});

it("SQLite: additive .14 table creation preserves history/settings/grants", async () => {
  const file = path();
  const store = new SqliteAssistantStore(file);
  await claim(store);
  await store.settings.updateSettings({ defaultModel: "preserved-model" });
  store.close();
  const raw = new Database(file);
  raw.exec("DROP TABLE tool_executions");
  raw.close();
  const reopened = new SqliteAssistantStore(file);
  try {
    expect(await reopened.toolExecutions.listByRun("run")).toEqual([]);
    expect((await reopened.conversations.getChat("chat"))?.metadata).toEqual({
      grants: ["preserved"],
    });
    expect(
      (await reopened.conversations.listMessages("chat"))[0]?.content,
    ).toBe("preserved history");
    expect((await reopened.settings.getSettings()).defaultModel).toBe(
      "preserved-model",
    );
    expect(
      Number(
        (
          reopened.database.query("PRAGMA user_version").get() as {
            user_version: number | bigint;
          }
        ).user_version,
      ),
    ).toBe(10);
  } finally {
    reopened.close();
  }
});

for (const corruption of [
  "{}",
  '{"status":"unknown"}',
  "not-json",
  "missing-result",
  "wrong-run",
  "wrong-execution",
  "wrong-approval",
] as const) {
  it(`SQLite: ${corruption} receipt blocks recovery explicitly`, async () => {
    const store = new SqliteAssistantStore(":memory:");
    try {
      const { fence, admission } = await claim(store);
      await store.toolExecutions.admit(admission, fence);
      const record =
        corruption === "missing-result"
          ? { ...admission, status: "completed" }
          : corruption === "wrong-run"
            ? { ...admission, status: "admitted", runId: "different" }
            : corruption === "wrong-execution"
              ? { ...admission, status: "admitted", executionId: "different" }
              : corruption === "wrong-approval"
                ? {
                    ...admission,
                    status: "admitted",
                    approvalRequestId: "different",
                  }
                : undefined;
      store.database.run("UPDATE tool_executions SET record = $record", {
        $record: record === undefined ? corruption : JSON.stringify(record),
      });
      await expect(store.toolExecutions.listByRun("run")).rejects.toMatchObject(
        { code: "recovery_history_unreadable" },
      );
    } finally {
      store.close();
    }
  });
}
