import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import { createTestEventStamper } from "@agentkit/testing";
import type {
  AiChatRequest,
  AiProviderClient,
} from "../src/providers/client.js";
import type { ProviderRetryPolicy } from "../src/runs/provider-retry.js";
import { runChat } from "../src/runs/run-loop.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { collectRun } from "./helpers.js";

const limits = resolveToolLimits({ preference: "small" });
const messages: AiChatMessage[] = [{ role: "user", content: "go" }];

type Attempt =
  | { kind: "fail"; code?: string; usage?: boolean; deltaFirst?: boolean }
  | { kind: "throw"; error: Error }
  | { kind: "ok" };

class ScriptedClient implements AiProviderClient {
  readonly id = "scripted";
  readonly kind = "openai-compatible" as const;
  calls = 0;
  constructor(private readonly attempts: Attempt[]) {}
  async capabilities() {
    return { streaming: true, toolCalling: true, modelList: false };
  }
  async listModels() {
    return [];
  }
  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const step = this.attempts[this.calls++] ?? { kind: "ok" };
    const stamp = createTestEventStamper();
    const runId = input.runId;
    const timestamp = new Date().toISOString();
    yield stamp({
      type: "run.started",
      runId,
      timestamp,
      data: { model: input.model, toolCount: 0 },
    });
    if (step.kind === "throw") throw step.error;
    if (step.kind === "fail") {
      if (step.deltaFirst) {
        yield stamp({
          type: "run.message.delta",
          runId,
          timestamp,
          data: { delta: "par" },
        });
      }
      if (step.usage) {
        yield stamp({
          type: "run.usage",
          runId,
          timestamp,
          data: {
            callId: `c${this.calls}`,
            attempt: 1,
            step: 0,
            model: input.model,
            promptTokens: 1,
            completionTokens: 0,
            totalTokens: 1,
            source: "stream",
            finalForCall: true,
          },
        });
      }
      yield stamp({
        type: "run.failed",
        runId,
        timestamp,
        data: {
          errorMessage: "boom",
          ...(step.code ? { errorCode: step.code } : {}),
        },
      });
      return;
    }
    yield stamp({
      type: "run.message.delta",
      runId,
      timestamp,
      data: { delta: "hi" },
    });
    yield stamp({
      type: "run.message.completed",
      runId,
      timestamp,
      data: { content: "hi", toolCallCount: 0 },
    });
    yield stamp({
      type: "run.usage",
      runId,
      timestamp,
      data: {
        callId: `c${this.calls}`,
        attempt: 1,
        step: 0,
        model: input.model,
        promptTokens: 2,
        completionTokens: 1,
        totalTokens: 3,
        source: "stream",
        finalForCall: true,
      },
    });
  }
}

function policy(over: Partial<ProviderRetryPolicy> = {}): ProviderRetryPolicy {
  return {
    maxAttempts: 3,
    baseDelayMs: 100,
    maxDelayMs: 1000,
    random: () => 0.5,
    sleep: async () => {},
    ...over,
  };
}

function run(
  client: ScriptedClient,
  providerRetry?: ProviderRetryPolicy,
  signal?: AbortSignal,
) {
  return collectRun(
    runChat({
      client,
      registry: new AiToolRegistry(),
      model: "m",
      messages,
      limits,
      ...(providerRetry ? { providerRetry } : {}),
      ...(signal ? { signal } : {}),
    }),
  );
}

const types = (events: AiRunEvent[]) => events.map((e) => e.type);

describe("providerRetry", () => {
  it("retries 503 once, warns, keeps both usage events", async () => {
    const client = new ScriptedClient([
      { kind: "fail", code: "503", usage: true },
      { kind: "ok" },
    ]);
    const { events } = await run(client, policy());
    expect(client.calls).toBe(2);
    const warnings = events.filter((e) => e.type === "run.warning");
    expect(warnings).toHaveLength(1);
    expect(warnings[0]?.data).toEqual({
      code: "provider_retry",
      message:
        "Provider request failed (503); retrying in 0.1s (attempt 2 of 3).",
    });
    expect(types(events).filter((t) => t === "run.started")).toHaveLength(1);
    expect(types(events).filter((t) => t === "run.usage")).toHaveLength(2);
    expect(types(events)).toContain("run.completed");
    expect(types(events)).not.toContain("run.failed");
  });

  it("does not retry 400", async () => {
    const client = new ScriptedClient([
      { kind: "fail", code: "400" },
      { kind: "ok" },
    ]);
    const { events } = await run(client, policy());
    expect(client.calls).toBe(1);
    expect(types(events)).toContain("run.failed");
  });

  it("does not retry after a delta", async () => {
    const client = new ScriptedClient([
      { kind: "fail", code: "503", deltaFirst: true },
      { kind: "ok" },
    ]);
    const { events } = await run(client, policy());
    expect(client.calls).toBe(1);
    expect(types(events)).toContain("run.failed");
    expect(types(events)).not.toContain("run.warning");
  });

  it("fails with the last code when attempts are exhausted", async () => {
    const client = new ScriptedClient([
      { kind: "fail", code: "503" },
      { kind: "fail", code: "429" },
    ]);
    const { events } = await run(client, policy({ maxAttempts: 2 }));
    expect(client.calls).toBe(2);
    expect(events.filter((e) => e.type === "run.warning")).toHaveLength(1);
    const failed = events.find((e) => e.type === "run.failed");
    expect(failed?.type === "run.failed" && failed.data.errorCode).toBe("429");
  });

  it("cancels when aborted during backoff, without another request", async () => {
    const controller = new AbortController();
    const client = new ScriptedClient([
      { kind: "fail", code: "503" },
      { kind: "ok" },
    ]);
    const { events } = await run(
      client,
      policy({
        sleep: async () => {
          controller.abort();
        },
      }),
      controller.signal,
    );
    expect(client.calls).toBe(1);
    const cancelled = events.find((e) => e.type === "run.cancelled");
    expect(cancelled?.type === "run.cancelled" && cancelled.data.reason).toBe(
      "aborted",
    );
    expect(types(events)).not.toContain("run.failed");
  });

  it("retries a thrown network error before output", async () => {
    const client = new ScriptedClient([
      { kind: "throw", error: new Error("socket reset") },
      { kind: "ok" },
    ]);
    const { events } = await run(client, policy());
    expect(client.calls).toBe(2);
    expect(types(events)).toContain("run.completed");
  });

  it("applies capped, jittered exponential backoff", async () => {
    const delays: number[] = [];
    const client = new ScriptedClient([
      { kind: "fail", code: "network_error" },
      { kind: "fail", code: "network_error" },
      { kind: "ok" },
    ]);
    await run(
      client,
      policy({
        baseDelayMs: 100,
        maxDelayMs: 150,
        random: () => 1,
        sleep: async (ms) => {
          delays.push(ms);
        },
      }),
    );
    expect(delays).toEqual([120, 180]);
  });

  it("invalid policy means no retry", async () => {
    const client = new ScriptedClient([
      { kind: "fail", code: "503" },
      { kind: "ok" },
    ]);
    const { events } = await run(client, policy({ maxAttempts: 99 }));
    expect(client.calls).toBe(1);
    expect(types(events)).toContain("run.failed");
  });

  it("absent option: failure sequence unchanged", async () => {
    const client = new ScriptedClient([
      { kind: "fail", code: "503", usage: true },
      { kind: "ok" },
    ]);
    const { events } = await run(client);
    expect(client.calls).toBe(1);
    expect(types(events)).toEqual(["run.started", "run.usage", "run.failed"]);
  });
});
