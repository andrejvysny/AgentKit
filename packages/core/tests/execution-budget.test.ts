import { describe, expect, it } from "bun:test";
import {
  ExecutionBudget,
  ExecutionBudgetError,
} from "../src/runs/execution-budget.js";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { MockProviderClient } from "@agentkit/testing";
import type { AiProviderClient } from "../src/providers/client.js";
import type { AiRunEvent } from "@agentkit/contracts";

const limits = resolveToolLimits({ preference: "small" });
const never = (): Promise<never> => new Promise(() => {});
function hanging(): AiProviderClient {
  return {
    id: "hung",
    kind: "openai-compatible",
    capabilities: async () => ({
      toolCalling: true,
      streaming: true,
      modelList: true,
    }),
    listModels: async () => [],
    // biome-ignore lint/correctness/useYield: deliberately models a provider that never produces a frame.
    async *streamChat() {
      await never();
    },
  };
}
async function collect(
  client: AiProviderClient,
  budgets: ConstructorParameters<typeof ExecutionBudget>[0],
  signal?: AbortSignal,
  registry = new AiToolRegistry(),
) {
  const events: AiRunEvent[] = [];
  for await (const event of runChat({
    client,
    model: "m",
    registry,
    messages: [],
    limits,
    budgets,
    signal,
  }))
    events.push(event);
  return events;
}

describe("execution budgets", () => {
  it("ends an uncooperative provider on first-byte deadline", async () => {
    const events = await collect(hanging(), { firstByteMs: 5 });
    expect(events.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "execution_timeout" },
    });
  });
  it("cancels an uncooperative provider without waiting for return", async () => {
    const controller = new AbortController();
    const work = collect(hanging(), { overallMs: 1000 }, controller.signal);
    controller.abort();
    expect((await work).at(-1)?.type).toBe("run.cancelled");
  });
  it("counts provider calls across passes and restored snapshots", async () => {
    const budget = new ExecutionBudget({ providerRequests: 1 });
    await budget.consume("providerRequests");
    const restored = new ExecutionBudget(
      { providerRequests: 1 },
      { state: budget.snapshot() },
    );
    await expect(restored.consume("providerRequests")).rejects.toMatchObject({
      code: "execution_budget_exhausted",
    });
    budget.dispose();
    restored.dispose();
  });
  it("persists a reservation before dispatch; failed persistence starts nothing", async () => {
    let started = false;
    const budget = new ExecutionBudget(
      {},
      {
        checkpoint: async () => {
          throw new Error("disk full");
        },
      },
    );
    await expect(
      (async () => {
        await budget.consume("toolCalls");
        started = true;
      })(),
    ).rejects.toThrow("disk full");
    expect(started).toBe(false);
    budget.dispose();
  });
  it("retains original overall deadline after restart", () => {
    const first = new ExecutionBudget({}, { now: () => 100 });
    const resumed = new ExecutionBudget(
      { overallMs: 10 },
      { state: first.snapshot(), now: () => 111 },
    );
    expect(() => resumed.check()).toThrow(ExecutionBudgetError);
    first.dispose();
    resumed.dispose();
  });
  it("stops hung tools and never starts another provider call", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "call",
            name: "hang",
            argumentsJson: "{}",
          },
        ],
      },
    ]);
    const registry = new AiToolRegistry();
    registry.register({
      definition: {
        name: "hang",
        version: "1",
        description: "hang",
        effect: "read",
        capability: "test",
        inputSchema: { type: "object" },
      },
      execute: never,
    });
    const events = await collect(
      client,
      { toolMs: 5, overallMs: 500 },
      undefined,
      registry,
    );
    expect(events.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "execution_timeout" },
    });
    expect(client.callCount).toBe(1);
  });
});
