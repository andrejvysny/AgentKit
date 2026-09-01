import { describe, expect, it } from "bun:test";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { messageContentToText } from "../src/messages/content.js";
import { MockProviderClient, type MockScriptStep } from "@agentkit/testing";
import { collectRun } from "./helpers.js";
import type { AiTool } from "../src/tools/tool.js";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";

const TERMINAL = new Set(["run.completed", "run.failed", "run.cancelled"]);

type FailedEvent = AiRunEvent & {
  data: { toolCallId: string; toolName: string; errorCode?: string };
};

const failedEvents = (events: AiRunEvent[]): FailedEvent[] =>
  events.filter((e) => e.type === "run.tool.failed") as FailedEvent[];

/** The structured half of the error envelope a failed call feeds the model. */
function envelopeData(message: AiChatMessage): Record<string, unknown> {
  return (
    JSON.parse(messageContentToText(message.content)) as {
      data: Record<string, unknown>;
    }
  ).data;
}

function toolCallStep(
  toolCallId: string,
  name: string,
  argumentsJson: string,
): MockScriptStep {
  return { kind: "tool_call", toolCallId, name, argumentsJson };
}

function makeEchoTool(name = "echo"): AiTool<{ text: string }, unknown> {
  return {
    definition: {
      name,
      version: "1",
      effect: "read",
      capability: "test",
      description: "echo",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
        required: ["text"],
      },
    },
    async execute(ctx, input) {
      return {
        ok: true,
        data: { echoed: input.text },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

describe("runChat — cancellation", () => {
  it("cancels before touching the provider when the signal is already aborted", async () => {
    const client = new MockProviderClient();
    client.setScript([{ steps: [{ kind: "text", content: "never sent" }] }]);
    const controller = new AbortController();
    controller.abort();

    const { events, result } = await collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
    );

    // A run cancelled before it starts must not spend a request.
    expect(client.callCount).toBe(0);
    expect(events.length).toBe(1);
    expect(events[0]!.type).toBe("run.cancelled");
    expect(result.terminal).toBe("cancelled");
    expect(result.appendedMessages).toEqual([]);
  });

  it("keeps the result of a tool that finished, and balances the rest", async () => {
    const controller = new AbortController();
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          toolCallStep("c1", "aborts", "{}"),
          toolCallStep("c2", "echo", '{"text":"b"}'),
          toolCallStep("c3", "echo", '{"text":"c"}'),
        ],
      },
      { steps: [{ kind: "text", content: "never reached" }] },
    ]);

    // Tool #1 cancels the run WHILE running, then still succeeds. Its side
    // effects are real by then, so its result must survive the cancellation.
    const abortingTool: AiTool<Record<string, never>, unknown> = {
      definition: {
        name: "aborts",
        version: "1",
        effect: "write",
        capability: "test",
        description: "cancels the run mid-execution",
        inputSchema: { type: "object", properties: {} },
      },
      async execute(ctx) {
        controller.abort();
        return {
          ok: true,
          data: { applied: true },
          sources: [],
          warnings: [],
          truncated: false,
          limits: ctx.limits,
        };
      },
    };
    const registry = new AiToolRegistry();
    registry.register(abortingTool as unknown as AiTool);
    registry.register(makeEchoTool() as unknown as AiTool);

    const messages: AiChatMessage[] = [{ role: "user", content: "go" }];
    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages,
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
    );

    // The tool that ran reports its real outcome — NOT "cancelled". Discarding
    // it would tell a replay the write never happened and invite a duplicate.
    const succeeded = events.filter(
      (e) => e.type === "run.tool.succeeded",
    ) as Array<AiRunEvent & { data: { toolCallId: string } }>;
    expect(succeeded.map((e) => e.data.toolCallId)).toEqual(["c1"]);

    // The calls that never got to run are failed as cancelled, not left hanging.
    const cancelled = failedEvents(events).filter(
      (e) => e.data.errorCode === "cancelled",
    );
    expect(cancelled.map((e) => e.data.toolCallId)).toEqual(["c2", "c3"]);

    expect(result.terminal).toBe("cancelled");
    expect(events.at(-1)!.type).toBe("run.cancelled");
    // Only one provider round-trip: the run stopped before asking again.
    expect(client.callCount).toBe(1);

    // Every tool_call_id the assistant message announced gets exactly one tool
    // response. An orphan tool_call_id in replayed history is rejected outright
    // by Chat Completions on the next turn.
    const assistant = result.appendedMessages.find(
      (m) => m.role === "assistant" && m.toolCalls && m.toolCalls.length > 0,
    );
    expect(assistant?.toolCalls?.map((tc) => tc.id)).toEqual([
      "c1",
      "c2",
      "c3",
    ]);
    const responded = result.appendedMessages
      .filter((m) => m.role === "tool")
      .map((m) => m.toolCallId);
    expect(responded).toEqual(["c1", "c2", "c3"]);
    expect(new Set(responded).size).toBe(3);
    expect(messages.length).toBe(1);
  });
});

describe("runChat — tool execution failures", () => {
  it("reports a throwing tool as exec_failed and finishes the run", async () => {
    const client = new MockProviderClient();
    client.setScript([
      { steps: [toolCallStep("c1", "explodes", "{}")] },
      { steps: [{ kind: "text", content: "recovered" }] },
    ]);
    const explodingTool: AiTool<Record<string, never>, unknown> = {
      definition: {
        name: "explodes",
        version: "1",
        effect: "read",
        capability: "test",
        description: "throws",
        inputSchema: { type: "object", properties: {} },
      },
      // Throws synchronously, before ever returning a promise.
      execute(): Promise<never> {
        throw new Error("kaboom");
      },
    };
    const registry = new AiToolRegistry();
    registry.register(explodingTool as unknown as AiTool);

    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    const failed = failedEvents(events);
    expect(failed.length).toBe(1);
    expect(failed[0]!.data.errorCode).toBe("exec_failed");
    expect(
      (failed[0] as { data: { errorMessage: string } }).data.errorMessage,
    ).toContain("kaboom");
    // A thrown tool is a fact to report to the model, not a reason to abandon
    // the run.
    expect(events.filter((e) => TERMINAL.has(e.type)).length).toBe(1);
    expect(result.terminal).toBe("completed");
    expect(client.callCount).toBe(2);
    const toolMsg = result.appendedMessages.find((m) => m.role === "tool");
    expect(toolMsg?.toolCallId).toBe("c1");
    // A plain throw carries no retry promise, so the envelope must not invent
    // one — `retryable: false` unless the error itself says otherwise.
    expect(envelopeData(toolMsg!)).toMatchObject({
      errorCode: "exec_failed",
      phase: "execution",
      retryable: false,
    });
  });

  it("carries a thrown error's own retryable flag into the envelope", async () => {
    const client = new MockProviderClient();
    client.setScript([
      { steps: [toolCallStep("c1", "flaky", "{}")] },
      { steps: [{ kind: "text", content: "will try later" }] },
    ]);
    const flaky: AiTool<Record<string, never>, unknown> = {
      definition: {
        name: "flaky",
        version: "1",
        effect: "read",
        capability: "test",
        description: "throws something transient",
        inputSchema: { type: "object", properties: {} },
      },
      execute(): Promise<never> {
        throw Object.assign(new Error("upstream 503"), { retryable: true });
      },
    };
    const registry = new AiToolRegistry();
    registry.register(flaky as unknown as AiTool);

    const { result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    const toolMsg = result.appendedMessages.find((m) => m.role === "tool");
    expect(envelopeData(toolMsg!)).toMatchObject({
      errorCode: "exec_failed",
      phase: "execution",
      retryable: true,
    });
  });

  it("reports unparseable arguments as bad_args and feeds the error back", async () => {
    const client = new MockProviderClient();
    client.setScript([
      { steps: [toolCallStep("c1", "echo", "not json{{{")] },
      { steps: [{ kind: "text", content: "sorry, retrying" }] },
    ]);
    const registry = new AiToolRegistry();
    registry.register(makeEchoTool() as unknown as AiTool);

    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    const failed = failedEvents(events);
    expect(failed.length).toBe(1);
    expect(failed[0]!.data.errorCode).toBe("bad_args");
    expect(events.some((e) => e.type === "run.tool.succeeded")).toBe(false);

    // The model gets a balanced error envelope back so it can correct itself.
    const toolMsg = result.appendedMessages.find((m) => m.role === "tool");
    expect(toolMsg?.toolCallId).toBe("c1");
    const envelope = JSON.parse(messageContentToText(toolMsg!.content)) as {
      ok: boolean;
      status: string;
      data: { errorCode: string; errorMessage: string };
    };
    expect(envelope.ok).toBe(false);
    expect(envelope.status).toBe("error");
    expect(envelope.data.errorCode).toBe("bad_args");
    // ...and the run carries on to the next iteration.
    expect(result.terminal).toBe("completed");
    expect(result.iterations).toBe(2);
    expect(events.filter((e) => TERMINAL.has(e.type)).length).toBe(1);
  });
});
