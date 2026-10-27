/**
 * The host bridge for `runAgentLoop` hooks (ADR 0015 §6): `TurnRunnerDeps.
 * loopHooks` reach the loop, a hook can only take authority away (its deny
 * stops a tool a guard would allow), never add it (its allow cannot override a
 * guard's refusal), and what hooks emit lands in the durable log as one
 * unbroken sequence.
 */
import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import type { AgentLoopHooks, AiTool } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  TOOL_GUARD_REFUSED_CODE,
  TurnRunner,
  type MessageRecord,
  type ToolGuard,
  type ToolSetContributor,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

interface Fixture extends TestHarness {
  runner: TurnRunner;
  mock: MockProviderClient;
  chatId: string;
  executed: string[];
}

async function setup(options: {
  loopHooks?: readonly AgentLoopHooks[];
  loopHookTimeoutMs?: number;
  toolGuards?: ToolGuard[];
}): Promise<Fixture> {
  const harness = createHarness();
  const mock = new MockProviderClient();
  const executed: string[] = [];
  const echo: AiTool<{ text?: string }, { echoed: string }> = {
    definition: {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "Echo the input.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
      },
    },
    async execute(ctx, input) {
      executed.push(input.text ?? "");
      return {
        ok: true,
        data: { echoed: input.text ?? "" },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
  const contributor: ToolSetContributor = {
    namespace: "test",
    contribute: async () => [echo as AiTool],
  };
  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => mock,
    contributors: [contributor],
    clock: harness.clock,
    ids: harness.ids,
    ...(options.toolGuards === undefined
      ? {}
      : { toolGuards: options.toolGuards }),
    ...(options.loopHooks === undefined
      ? {}
      : { loopHooks: options.loopHooks }),
    ...(options.loopHookTimeoutMs === undefined
      ? {}
      : { loopHookTimeoutMs: options.loopHookTimeoutMs }),
  });
  return { ...harness, runner, mock, chatId: chat.id, executed };
}

/** Create the attempt, take the lease and execute — the task runner's job. */
async function drive(f: Fixture, taskId: string): Promise<void> {
  const attempt = await f.store.tasks.createAttempt({
    attemptId: f.ids.attemptId(),
    taskId,
    ownerId: "worker-1",
  });
  const lease = await f.store.tasks.acquireLease({
    taskId,
    attemptId: attempt.attemptId,
    ownerId: "worker-1",
    ttlMs: 60_000,
  });
  await f.runner.execute({
    taskId,
    attemptId: attempt.attemptId,
    leaseToken: lease.leaseToken,
    signal: new AbortController().signal,
  });
}

function scriptOneToolCall(f: Fixture): void {
  f.mock.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-1",
          name: "echo",
          argumentsJson: '{"text":"hi"}',
        },
      ],
    },
    { steps: [{ kind: "text", content: "understood" }] },
  ]);
}

async function runTurn(f: Fixture): Promise<{
  runId: string;
  events: AiRunEvent[];
}> {
  const submitted = await f.runner.submitMessage({
    chatId: f.chatId,
    content: "use the tool",
  });
  await drive(f, submitted.runId);
  const events = (await f.store.tasks.listEvents(
    submitted.runId,
  )) as AiRunEvent[];
  return { runId: submitted.runId, events };
}

function toolFailure(events: AiRunEvent[]):
  | {
      errorCode?: string;
      errorMessage: string;
    }
  | undefined {
  const failed = events.find((e) => e.type === "run.tool.failed");
  return failed?.type === "run.tool.failed" ? failed.data : undefined;
}

function toolRecords(f: Fixture): MessageRecord[] {
  return f.store.conversations.messages.filter((m) => m.role === "tool");
}

describe("TurnRunner loop hooks bridge", () => {
  it("passes loop hooks through to the run", async () => {
    const seen: string[] = [];
    const f = await setup({
      loopHooks: [
        {
          onRunStart: () => {
            seen.push("start");
          },
          onToolResult: (ctx) => {
            seen.push(`tool:${ctx.toolName}:${ctx.status}`);
          },
          onSettled: (ctx) => {
            seen.push(`settled:${ctx.terminal}`);
          },
        },
      ],
    });
    scriptOneToolCall(f);
    const { runId } = await runTurn(f);
    expect(seen).toEqual(["start", "tool:echo:succeeded", "settled:completed"]);
    expect(f.executed).toEqual(["hi"]);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
  });

  it("a hook deny stops the tool even when every guard allows", async () => {
    let guardCalls = 0;
    const f = await setup({
      toolGuards: [
        {
          canExecute: () => {
            guardCalls += 1;
            return { allowed: true };
          },
        },
      ],
      loopHooks: [
        {
          beforeToolCall: () => ({
            action: "deny",
            reason: "frozen by policy",
          }),
        },
      ],
    });
    scriptOneToolCall(f);
    const { runId, events } = await runTurn(f);
    expect(f.executed).toEqual([]);
    // The deny short-circuits before execute, so the guard wrapper never runs.
    expect(guardCalls).toBe(0);
    expect(toolFailure(events)).toMatchObject({
      errorCode: "tool_hook_denied",
      errorMessage: "frozen by policy",
    });
    // The call is still answered, and the turn completes.
    expect(toolRecords(f).map((m) => m.toolCallId)).toEqual(["call-1"]);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
  });

  it("a hook allow cannot override a guard refusal", async () => {
    const f = await setup({
      toolGuards: [
        { canExecute: () => ({ allowed: false, reason: "writes are paused" }) },
      ],
      loopHooks: [{ beforeToolCall: () => ({ action: "allow" }) }],
    });
    scriptOneToolCall(f);
    const { events } = await runTurn(f);
    expect(f.executed).toEqual([]);
    expect(toolFailure(events)?.errorMessage).toBe("writes are paused");
    const envelope = JSON.parse(toolRecords(f)[0]?.content as string) as {
      data: { errorCode: string };
    };
    expect(envelope.data.errorCode).toBe(TOOL_GUARD_REFUSED_CODE);
  });

  it("hook warnings land in the durable log as one unbroken sequence", async () => {
    const f = await setup({
      loopHooks: [
        {
          onStepStart: () => {
            throw new Error("secret-token-123");
          },
        },
      ],
    });
    scriptOneToolCall(f);
    const { runId, events } = await runTurn(f);
    const warnings = events.filter(
      (e) => e.type === "run.warning" && e.data.code === "hook_failed",
    );
    // One per step: the tool step and the answer step.
    expect(warnings).toHaveLength(2);
    expect(JSON.stringify(warnings)).not.toContain("secret-token-123");
    events.forEach((event, index) => {
      expect(event.seq).toBe(index);
      expect(event.runId).toBe(runId);
    });
    expect(f.executed).toEqual(["hi"]);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
  });

  it("a hook that outlives loopHookTimeoutMs degrades, not fails", async () => {
    const f = await setup({
      loopHookTimeoutMs: 20,
      loopHooks: [{ onRunStart: () => new Promise<void>(() => {}) }],
    });
    scriptOneToolCall(f);
    const { runId, events } = await runTurn(f);
    expect(
      events.some(
        (e) => e.type === "run.warning" && e.data.code === "hook_timeout",
      ),
    ).toBe(true);
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
  });
});
