import { describe, expect, it } from "bun:test";
import { MemoryAssistantStore } from "../../adapters-memory/src/index.js";
import {
  createEventStamper,
  ExecutionBudgetError,
  type ExecutionBudgets,
  type AiTool,
} from "../../core/src/index.js";
import { MockProviderClient } from "../../testing/src/index.js";
import {
  TurnRunner,
  defaultClock,
  defaultIds,
  type AssistantStore,
  type TaskRunner,
  type VerificationHook,
} from "@agentkit/host";
import { SqliteAssistantStore } from "../src/index.js";

const queue: TaskRunner = {
  async enqueue() {},
  async requestCancel() {},
  async recover() {},
  async startWorker() {
    return { async stop() {} };
  },
};
const tool: AiTool = {
  definition: {
    name: "read",
    version: "1",
    effect: "read",
    capability: "read",
    description: "Read",
    inputSchema: { type: "object" },
  },
  async execute(context) {
    return {
      ok: true,
      data: {},
      summary: "read",
      sources: [],
      warnings: [],
      truncated: false,
      limits: context.limits,
    };
  },
};

for (const adapter of ["memory", "sqlite"] as const) {
  describe(`${adapter} turn terminal fencing`, () => {
    async function fixture(
      verification?: VerificationHook,
      budgets?: ExecutionBudgets,
    ) {
      const store: AssistantStore =
        adapter === "memory"
          ? new MemoryAssistantStore()
          : new SqliteAssistantStore(":memory:");
      const client = new MockProviderClient();
      client.setScript([
        {
          steps: [
            {
              kind: "tool_call",
              toolCallId: "call",
              name: "read",
              argumentsJson: "{}",
            },
          ],
        },
        { steps: [{ kind: "text", content: "answer" }] },
      ]);
      await store.providers.upsertProvider({
        id: "p",
        label: "Provider",
        enabled: true,
        kind: "openai-compatible",
        baseUrl: "http://localhost",
        defaultModel: "model",
      });
      await store.settings.updateSettings({ defaultProviderId: "p" });
      const chat = await store.conversations.createChat({});
      const runner = new TurnRunner({
        store,
        taskRunner: queue,
        providerFactory: () => client,
        contributors: [
          {
            namespace: "read",
            async contribute() {
              return [tool];
            },
          },
        ],
        clock: defaultClock,
        ids: defaultIds,
        verification,
        executionBudgets: budgets,
      });
      const submitted = await runner.submitMessage({
        chatId: chat.id,
        content: "read",
      });
      const claim = (await store.tasks.claimNext({
        ownerId: "worker",
        now: new Date(),
        scopesBusy: [],
      }))!;
      const controller = new AbortController();
      const execute = () =>
        runner.execute({
          taskId: claim.task.taskId,
          attemptId: claim.attempt.attemptId,
          leaseToken: claim.lease.leaseToken,
          signal: controller.signal,
        });
      return {
        store,
        runner,
        client,
        chat,
        submitted,
        claim,
        controller,
        execute,
      };
    }

    it("keeps task running during verification, then publishes finalized message and verdict", async () => {
      let release!: () => void;
      let entered!: () => void;
      const waiting = new Promise<void>((resolve) => {
        release = resolve;
      });
      const started = new Promise<void>((resolve) => {
        entered = resolve;
      });
      const f = await fixture({
        async verify() {
          entered();
          await waiting;
          return { status: "partial", checks: [], deficiencies: ["missing"] };
        },
      });
      const run = f.execute();
      await started;
      expect((await f.store.tasks.getTask(f.submitted.runId))?.status).toBe(
        "running",
      );
      expect(
        (await f.store.conversations.getMessage(f.submitted.assistantMessageId))
          ?.metadata.placeholder,
      ).toBe(true);
      release();
      await run;
      expect((await f.store.tasks.getTask(f.submitted.runId))?.status).toBe(
        "completed",
      );
      expect(
        (await f.store.conversations.getMessage(f.submitted.assistantMessageId))
          ?.metadata.placeholder,
      ).toBe(false);
      expect(
        (await f.store.conversations.listMessages(f.chat.id)).some(
          (message) => message.metadata.banner === "verification",
        ),
      ).toBe(true);
    });

    it("cancels during verification and rejects its late success", async () => {
      let release!: () => void;
      let entered!: () => void;
      const waiting = new Promise<void>((resolve) => {
        release = resolve;
      });
      const started = new Promise<void>((resolve) => {
        entered = resolve;
      });
      const f = await fixture({
        async verify() {
          entered();
          await waiting;
          return { status: "partial", checks: [], deficiencies: ["late"] };
        },
      });
      const run = f.execute();
      await started;
      f.controller.abort();
      await run;
      expect((await f.store.tasks.getTask(f.submitted.runId))?.status).toBe(
        "cancelled",
      );
      release();
      await new Promise((resolve) => setTimeout(resolve, 5));
      expect(
        (await f.store.conversations.listMessages(f.chat.id)).some(
          (message) => message.metadata.banner === "verification",
        ),
      ).toBe(false);
    });

    it("bounds verification by overall deadline and preserves failed timeout", async () => {
      const f = await fixture(
        {
          async verify() {
            return new Promise(() => {});
          },
        },
        { overallMs: 20 },
      );
      await expect(f.execute()).rejects.toBeInstanceOf(ExecutionBudgetError);
      expect((await f.store.tasks.getTask(f.submitted.runId))?.status).toBe(
        "failed",
      );
      expect(
        (await f.store.conversations.getMessage(f.submitted.assistantMessageId))
          ?.metadata.placeholder,
      ).toBe(false);
      const events = await f.store.tasks.listEvents(f.submitted.runId);
      expect(events.at(-1)).toMatchObject({
        type: "run.failed",
        data: { errorCode: "execution_timeout" },
      });
    });

    it("persists provider counters and refuses a tool continuation past budget", async () => {
      const f = await fixture(undefined, { providerRequests: 1, retries: 0 });
      await expect(f.execute()).rejects.toBeInstanceOf(ExecutionBudgetError);
      const task = await f.store.tasks.getTask(f.submitted.runId);
      expect(task?.status).toBe("failed");
      expect(task?.progress?.executionBudget).toMatchObject({
        providerRequests: 1,
        toolCalls: 1,
        retries: 0,
      });
      expect(f.client.callCount).toBe(1);
    });

    it("bounds public terminal diagnostics to 4096 UTF-8 bytes", async () => {
      const f = await fixture();
      const runner = new TurnRunner({
        store: f.store,
        taskRunner: queue,
        providerFactory() {
          throw new Error(
            `Authorization: Bearer terminal-secret {"api_key":"terminal-key"} ${"🙂".repeat(5000)}`,
          );
        },
        contributors: [],
        clock: defaultClock,
        ids: defaultIds,
      });
      await expect(
        runner.execute({
          taskId: f.claim.task.taskId,
          attemptId: f.claim.attempt.attemptId,
          leaseToken: f.claim.lease.leaseToken,
          signal: f.controller.signal,
        }),
      ).rejects.toThrow();
      const task = await f.store.tasks.getTask(f.submitted.runId);
      expect(
        new TextEncoder().encode(task?.error).byteLength,
      ).toBeLessThanOrEqual(4096);
      expect(task?.error?.includes("�")).toBe(false);
      expect(task?.error).not.toContain("terminal-secret");
      expect(task?.error).not.toContain("terminal-key");
      const terminal = (await f.store.tasks.listEvents(f.submitted.runId)).at(
        -1,
      );
      expect(JSON.stringify(terminal)).not.toContain("executionBudget");
      expect(JSON.stringify(terminal)).not.toContain("terminal-secret");
      expect(JSON.stringify(terminal)).not.toContain("terminal-key");
    });

    it("repairs a crashed projection before assembling resumed provider history", async () => {
      const f = await fixture();
      const stamp = createEventStamper();
      const runId = f.submitted.runId;
      await f.store.tasks.appendEvents(
        runId,
        [
          stamp({
            type: "run.message.completed",
            runId,
            timestamp: defaultClock.nowIso(),
            data: {
              content: "",
              toolCallCount: 1,
              toolCalls: [
                { id: "crashed-call", name: "read", argumentsJson: "{}" },
              ],
            },
          }),
          stamp({
            type: "run.tool.succeeded",
            runId,
            timestamp: defaultClock.nowIso(),
            data: {
              toolCallId: "crashed-call",
              toolName: "read",
              resultJson: '{"ok":true}',
              sources: [],
              warnings: [],
              truncated: false,
            },
          }),
        ],
        { leaseToken: f.claim.lease.leaseToken },
      );
      const attempt = await f.store.tasks.createAttempt({
        attemptId: "resumed",
        taskId: runId,
        ownerId: "new",
      });
      const lease = await f.store.tasks.acquireLease({
        taskId: runId,
        attemptId: attempt.attemptId,
        ownerId: "new",
        ttlMs: 30_000,
      });
      f.client.setScript([{ steps: [{ kind: "text", content: "recovered" }] }]);
      const stream = f.client.streamChat.bind(f.client);
      let sawResult = false;
      f.client.streamChat = (input) => {
        sawResult = input.messages.some(
          (message) =>
            message.role === "tool" &&
            message.toolCallId === "crashed-call" &&
            message.content === '{"ok":true}',
        );
        return stream(input);
      };
      await f.runner.execute({
        taskId: runId,
        attemptId: attempt.attemptId,
        leaseToken: lease.leaseToken,
        signal: f.controller.signal,
      });
      expect(sawResult).toBe(true);
      const records = await f.store.conversations.listMessages(f.chat.id);
      expect(
        records.filter((message) => message.toolCallId === "crashed-call"),
      ).toHaveLength(1);
      expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
    });
  });
}
