#!/usr/bin/env bun
/**
 * Benchmarks the `runChat` agent loop itself — request assembly, event
 * stamping, tool dispatch and envelope capping — against a scripted
 * `MockProviderClient`, so provider and network time are zero and what is
 * left is the loop's own overhead. NOT run in CI (no dev-hardware baseline
 * to compare a CI runner against) — run manually:
 *
 *   bun scripts/bench-run-loop.ts
 *
 * PURPOSE: the M0 reference for the unified-runtime refactor (ADR 0015). The
 * M1 split of `runChat` into model/tool steps must land within noise of the
 * numbers this prints on the same machine; record both runs in
 * `docs/baseline/`. There is no absolute budget — only "no regression".
 *
 * Shapes:
 *   (a) text-only answer over a 1,000-message history (request assembly
 *       cost scales with history length: `expandToolResultContent` and the
 *       request copy run once per provider call);
 *   (b) 10 tool calls: 10 iterations of one echo call each, then an answer;
 *   (c) 100 tool calls: 10 iterations of 10 echo calls each, then an answer
 *       (`maxToolCallsPerIteration` raised to 10 so nothing is capped).
 *
 * Each shape: WARMUP untimed runs, then RUNS timed runs (`performance.now()`
 * around a full drain of the generator), p50/p95 over those samples, plus the
 * RSS delta across the timed runs (a leak shows up as a delta that grows with
 * RUNS rather than settling).
 */
import {
  AiToolRegistry,
  resolveToolLimits,
  runChat,
} from "../packages/core/src/index.js";
import type { AiTool } from "../packages/core/src/index.js";
import type { AiChatMessage } from "../packages/contracts/src/index.js";
import { MockProviderClient } from "../packages/testing/src/mock-provider.js";
import type { MockTurn } from "../packages/testing/src/mock-provider.js";

const WARMUP = 20;
const RUNS = 200;
const LIMITS = resolveToolLimits({ preference: "small" });

function echoTool(): AiTool {
  const tool: AiTool<{ text: string }, { echoed: string }> = {
    definition: {
      name: "echo",
      version: "1",
      effect: "read",
      capability: "bench",
      description: "Echoes its input back.",
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
  return tool as unknown as AiTool;
}

function longHistory(length: number): AiChatMessage[] {
  const messages: AiChatMessage[] = [];
  for (let i = 0; i < length; i++) {
    messages.push({
      role: i % 2 === 0 ? "user" : "assistant",
      content: `message ${i}: ${"lorem ipsum ".repeat(20)}`,
    });
  }
  return messages;
}

function toolScript(iterations: number, callsPerIteration: number): MockTurn[] {
  const turns: MockTurn[] = [];
  for (let i = 0; i < iterations; i++) {
    const steps: MockTurn["steps"] = [];
    for (let c = 0; c < callsPerIteration; c++) {
      steps.push({
        kind: "tool_call",
        toolCallId: `call_${i}_${c}`,
        name: "echo",
        argumentsJson: JSON.stringify({ text: `iteration ${i} call ${c}` }),
      });
    }
    turns.push({ steps, finishReason: "tool_calls" });
  }
  turns.push({ steps: [{ kind: "text", content: "Done." }] });
  return turns;
}

interface Shape {
  name: string;
  run(): Promise<number>;
}

async function drain(
  client: MockProviderClient,
  messages: readonly AiChatMessage[],
  extra: { maxToolIterations?: number; maxToolCallsPerIteration?: number },
): Promise<number> {
  const registry = new AiToolRegistry();
  registry.register(echoTool());
  let events = 0;
  const generator = runChat({
    client,
    registry,
    model: "bench",
    messages,
    limits: LIMITS,
    ...extra,
  });
  for (;;) {
    const next = await generator.next();
    if (next.done) {
      if (next.value.terminal !== "completed")
        throw new Error(`bench run ended ${next.value.terminal}`);
      return events;
    }
    events++;
  }
}

const history = longHistory(1_000);
const shapes: Shape[] = [
  {
    name: "(a) text-only, 1k-message history",
    run: () => {
      const client = new MockProviderClient();
      client.setScript([{ steps: [{ kind: "text", content: "Answer." }] }]);
      return drain(client, history, {});
    },
  },
  {
    name: "(b) 10 tool calls (10 iterations x 1)",
    run: () => {
      const client = new MockProviderClient();
      client.setScript(toolScript(10, 1));
      return drain(client, [{ role: "user", content: "go" }], {
        maxToolIterations: 11,
      });
    },
  },
  {
    name: "(c) 100 tool calls (10 iterations x 10)",
    run: () => {
      const client = new MockProviderClient();
      client.setScript(toolScript(10, 10));
      return drain(client, [{ role: "user", content: "go" }], {
        maxToolIterations: 11,
        maxToolCallsPerIteration: 10,
      });
    },
  },
];

function percentile(sortedAsc: number[], p: number): number {
  const index = Math.min(
    sortedAsc.length - 1,
    Math.max(0, Math.ceil((p / 100) * sortedAsc.length) - 1),
  );
  return sortedAsc[index] ?? Number.NaN;
}

function fmt(ms: number): string {
  return `${ms.toFixed(3)}ms`;
}

console.log(
  `bench-run-loop: bun ${Bun.version}, ${WARMUP} warmup + ${RUNS} timed runs per shape`,
);
for (const shape of shapes) {
  let events = 0;
  for (let i = 0; i < WARMUP; i++) events = await shape.run();
  Bun.gc(true);
  const rssBefore = process.memoryUsage().rss;
  const samples: number[] = [];
  for (let i = 0; i < RUNS; i++) {
    const start = performance.now();
    await shape.run();
    samples.push(performance.now() - start);
  }
  Bun.gc(true);
  const rssDeltaMb = (process.memoryUsage().rss - rssBefore) / (1024 * 1024);
  samples.sort((a, b) => a - b);
  console.log(
    `${shape.name}: events=${events} p50=${fmt(percentile(samples, 50))} p95=${fmt(percentile(samples, 95))} rssDelta=${rssDeltaMb.toFixed(1)}MB`,
  );
}
