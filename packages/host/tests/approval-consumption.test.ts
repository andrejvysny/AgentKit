import { expect, it } from "bun:test";
import { resolveToolLimits, type AiTool } from "@agentkit/core";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import { SqliteAssistantStore } from "../../adapters-sqlite/src/index.js";
import {
  argsSha256Of,
  stageRegistry,
  type AssistantStore,
  type TaskExecutionContext,
} from "../src/index.js";

async function fixture(store: AssistantStore) {
  await store.conversations.createChat({ id: "chat" });
  await store.tasks.createTask({
    taskId: "run",
    kind: "test",
    scopeId: "chat",
    payload: {},
  });
  const claim = (await store.tasks.claimNext({
    ownerId: "worker",
    kinds: ["test"],
    scopesBusy: [],
    now: new Date(),
  }))!;
  const execution: TaskExecutionContext = {
    task: claim.task,
    attemptId: claim.attempt.attemptId,
    leaseToken: claim.lease.leaseToken,
    signal: new AbortController().signal,
  };
  const hash = await argsSha256Of({});
  await store.inputs!.createInput({
    inputId: "approval",
    sessionId: "chat",
    kind: "human_request",
    targetTaskId: "run",
    toolCallId: "call",
    attemptId: "original",
    contentHash: "h",
    content: { kind: "approval", toolName: "write", argsSha256: hash },
  });
  await store.inputs!.transitionInput("approval", ["pending"], "answered", {
    appliedMessageId: "approval:answer",
  });
  const limits = resolveToolLimits({ preference: "small" });
  const ctx = {
    runId: "run",
    scopeId: "chat",
    chatId: "chat",
    toolCallId: "call",
    bindings: [],
    limits,
    metadata: {
      approvedCalls: new Map([["call", hash]]),
      approvedRequests: new Map([["call", "approval"]]),
    },
  };
  return { execution, ctx, limits };
}

for (const adapter of ["memory", "sqlite"] as const) {
  it(`${adapter}: never replays Allow once through reconstructed/concurrent registries`, async () => {
    const store =
      adapter === "memory"
        ? new MemoryAssistantStore()
        : new SqliteAssistantStore(":memory:");
    try {
      const f = await fixture(store);
      let effects = 0;
      let enter!: () => void;
      let release!: () => void;
      const entered = new Promise<void>((resolve) => {
        enter = resolve;
      });
      const blocked = new Promise<void>((resolve) => {
        release = resolve;
      });
      const tool: AiTool = {
        definition: {
          name: "write",
          version: "1",
          effect: "write",
          capability: "write",
          description: "write",
          inputSchema: { type: "object" },
        },
        async execute(ctx) {
          effects += 1;
          enter();
          await blocked;
          return {
            ok: true,
            data: { effects },
            sources: [],
            warnings: [],
            truncated: false,
            limits: ctx.limits,
          };
        },
      };
      const stage = () =>
        stageRegistry({
          contributors: [{ namespace: "test", contribute: async () => [tool] }],
          ctx: { chatId: "chat", runId: "run", bindings: [], limits: f.limits },
          hasPrimaryBinding: true,
          guards: [
            {
              canExecute: () => ({
                allowed: false,
                reason: "approval required",
                approval: {},
              }),
            },
          ],
          durable: { store, ctx: f.execution },
        });
      const first = (await stage()).registry.get("write")!.execute(f.ctx, {});
      await entered;
      const replay = await (await stage()).registry
        .get("write")!
        .execute(f.ctx, {});
      expect(replay.ok).toBe(false);
      expect(effects).toBe(1);
      release();
      const completed = await first;
      const restored = await (await stage()).registry
        .get("write")!
        .execute(f.ctx, {});
      expect(restored).toEqual(completed);
      expect(effects).toBe(1);
      expect((await store.toolExecutions!.listByRun("run"))[0]?.status).toBe(
        "completed",
      );
      expect(await store.conversations.listMessages("chat")).toEqual([]);
    } finally {
      if (store instanceof SqliteAssistantStore) store.close();
    }
  });
}

it("fails closed for mutating hosts without durable receipts", async () => {
  const store = new MemoryAssistantStore();
  const f = await fixture(store);
  const legacy: AssistantStore = {
    ...store,
    transaction: store.transaction.bind(store),
    toolExecutions: undefined,
  };
  let executed = false;
  const staged = await stageRegistry({
    contributors: [
      {
        namespace: "test",
        contribute: async () => [
          {
            definition: {
              name: "write",
              version: "1",
              effect: "write",
              capability: "write",
              description: "write",
              inputSchema: {},
            },
            execute: async () => {
              executed = true;
              throw new Error("must not run");
            },
          },
        ],
      },
    ],
    ctx: { bindings: [], limits: f.limits },
    hasPrimaryBinding: true,
    durable: { store: legacy, ctx: f.execution },
  });
  await expect(
    staged.registry.get("write")!.execute(f.ctx, {}),
  ).rejects.toMatchObject({ code: "durable_execution_unavailable" });
  expect(executed).toBe(false);
});
