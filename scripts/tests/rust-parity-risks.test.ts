/** These tests characterize baseline risks; passing does not mean safe parity. */
import { describe, expect, it } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiTool } from "@agentkit/core";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import {
  ProposalService,
  SessionWritePolicy,
  TurnRunner,
} from "@agentkit/host";
import { SingleProcessTaskRunner } from "@agentkit/runner-local";
import { MockProviderClient } from "@agentkit/testing";
import { dedupeToolCallIds } from "../../packages/core/src/tools/tool-calls.js";
import {
  createHarness,
  createTestClock,
} from "../../packages/host/tests/fakes.js";

describe("TypeScript baseline parity risks (characterization, not acceptance)", () => {
  it("creates colliding derived call IDs when an earlier source ID owns the suffix", () => {
    const calls = ["x", "x#2", "x"].map((id) => ({
      id,
      name: "echo",
      argumentsJson: "{}",
    }));
    expect(dedupeToolCallIds(calls).calls.map((call) => call.id)).toEqual([
      "x",
      "x#2",
      "x#2",
    ]);
  });
  it("replays an operation outcome for a nonexistent proposal before identity validation", async () => {
    const harness = createHarness();
    const service = new ProposalService({
      ...harness,
      policy: new SessionWritePolicy({ clock: harness.clock }),
    });
    const proposal = await service.stage({
      chatId: "chat-1",
      scopeKey: "scope-1",
      toolName: "write",
      kind: "write",
      risk: "low",
      operations: [{ op: "add" }],
    });
    await service.approve({ proposalId: proposal.id, actor: "user" });
    const outcome = await service.apply({
      proposalId: proposal.id,
      operationId: "operation-shared",
    });
    let authorizationCalls = 0;
    const wrongReplay = await service.apply({
      proposalId: "does-not-exist",
      operationId: "operation-shared",
      authorize: () => {
        authorizationCalls++;
        return false;
      },
    });
    expect(wrongReplay).toEqual(outcome);
    expect(authorizationCalls).toBe(0);
    expect(harness.applier.calls).toHaveLength(1);
  });

  it("loses terminal tool evidence when cancellation lands after tool side effect", async () => {
    const harness = createHarness();
    const controller = new AbortController();
    let executed = 0;
    const tool: AiTool = {
      definition: {
        name: "echo",
        version: "1",
        effect: "read",
        capability: "test",
        description: "Cancellation boundary probe.",
        inputSchema: { type: "object" },
      },
      async execute(context) {
        executed++;
        controller.abort();
        return {
          ok: true,
          data: { completed: true },
          sources: [],
          warnings: [],
          truncated: false,
          limits: context.limits,
        };
      },
    };
    const mock = new MockProviderClient();
    mock.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "call-1",
            name: "echo",
            argumentsJson: "{}",
          },
        ],
      },
    ]);
    await harness.store.providers.upsertProvider({
      id: "p1",
      label: "Mock",
      kind: "openai-compatible",
      baseUrl: "http://localhost:1234",
      defaultModel: "m1",
      enabled: true,
    });
    await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
    await harness.store.conversations.createChat({ id: "chat-1" });
    const runner = new TurnRunner({
      ...harness,
      providerFactory: () => mock,
      contributors: [{ namespace: "probe", contribute: async () => [tool] }],
    });
    const submitted = await runner.submitMessage({
      chatId: "chat-1",
      content: "Run",
    });
    const attempt = await harness.store.tasks.createAttempt({
      taskId: submitted.runId,
      attemptId: "attempt-1",
      ownerId: "owner-1",
    });
    const lease = await harness.store.tasks.acquireLease({
      taskId: submitted.runId,
      attemptId: attempt.attemptId,
      ownerId: "owner-1",
      ttlMs: 60_000,
    });
    await runner.execute({
      taskId: submitted.runId,
      attemptId: attempt.attemptId,
      leaseToken: lease.leaseToken,
      signal: controller.signal,
    });
    const events = await harness.store.tasks.listEvents(submitted.runId);
    expect(executed).toBe(1);
    expect(events.some((event) => event.type === "run.tool.running")).toBe(
      true,
    );
    expect(
      events.filter((event) =>
        ["run.tool.succeeded", "run.tool.failed"].includes(event.type),
      ),
    ).toHaveLength(0);
    expect(events.at(-1)?.type).toBe("run.cancelled");
    expect((await harness.store.tasks.getTask(submitted.runId))?.status).toBe(
      "cancelled",
    );
  });

  it("cannot rediscover automatic redispatch after SQLite reopen between recover and worker start", async () => {
    const directory = await mkdtemp(join(tmpdir(), "agentkit-rust-recovery-"));
    const path = join(directory, "store.sqlite");
    const clock = createTestClock();
    let store = new SqliteAssistantStore(path, { clock, leaseTtlMs: 100 });
    try {
      await store.tasks.createTask({
        taskId: "task-1",
        kind: "probe",
        scopeId: "scope-1",
        payload: {},
      });
      await store.tasks.claimNext({
        ownerId: "dead-owner",
        now: clock.now(),
        scopesBusy: [],
      });
      clock.advance(101);
      const first = new SingleProcessTaskRunner({ store, clock });
      expect(await first.recoverWithReport()).toEqual({
        expired: 1,
        redispatched: 0,
        deadLettered: 0,
      });
      store.close();
      store = new SqliteAssistantStore(path, { clock, leaseTtlMs: 100 });
      const restarted = new SingleProcessTaskRunner({ store, clock });
      expect(await restarted.recoverWithReport()).toEqual({
        expired: 0,
        redispatched: 0,
        deadLettered: 0,
      });
      expect((await store.tasks.getTask("task-1"))?.status).toBe("running");
      expect(
        await store.tasks.claimNext({
          ownerId: "new-owner",
          now: clock.now(),
          scopesBusy: [],
        }),
      ).toBeNull();
    } finally {
      store.close();
      await rm(directory, { recursive: true, force: true });
    }
  });
});
