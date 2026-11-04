/**
 * H2 — the overall execution deadline must bound HOOK waits too.
 *
 * Reviewer probe: a 25 ms overall deadline waited ~153 ms for a slow hook
 * while the per-hook timeout was disabled (`hookTimeoutMs: 0`). The frozen
 * contract sells a wall-clock budget: a hook that ignores the abort signal
 * must never outlive the deadline the host bought. The effective native
 * wait is the SMALLER of the per-hook deadline and the budget's remaining
 * overall time; when the overall bound wins, the run settles exactly like
 * any other deadline breach (`run.failed` / `execution_timeout`).
 *
 * Real timers are deliberate here: the thing under test is the native
 * runtime's wall-clock bounding of a JS callback that parks on a real
 * timer; fake timers in this (JS) thread cannot drive the Rust scheduler.
 * Upper tolerances are generous (seconds) — only the LOSS of the bound
 * (the ~150 ms+ wait plus a late settle the probe measured, or a run that
 * never settles while the hook sleeps) flips any assertion.
 */
import { expect, test } from "bun:test";
import {
  AiToolRegistry,
  resolveToolLimits,
  runAgentLoop,
  type AgentLoopHooks,
  type AiRunEvent,
} from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available) {
  throw new Error(
    `AGENTKIT_REQUIRE_NATIVE=1 but the binding is not built (missing ${missing.join(", ")}); run \`bun run build:native\` in packages/native`,
  );
}
const suite = available ? test : test.skip;

process.env.AGENTKIT_BACKEND = "native";

const sleep = (ms: number): Promise<void> => {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
};

function scripted(): MockProviderClient {
  const client = new MockProviderClient();
  client.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "c1",
          name: "echo",
          argumentsJson: "{}",
        },
      ],
      finishReason: "tool_calls",
    },
    { steps: [{ kind: "text", content: "final" }] },
  ]);
  return client;
}

function registry(runs: { n: number }): AiToolRegistry {
  const registry = new AiToolRegistry();
  registry.register({
    definition: {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "echo",
      inputSchema: { type: "object" },
    },
    async execute(context) {
      runs.n += 1;
      return {
        ok: true,
        data: { echo: "x" },
        sources: [],
        warnings: [],
        truncated: false,
        limits: context.limits,
      };
    },
  });
  return registry;
}

async function drive(
  hooks: readonly AgentLoopHooks[],
  input: {
    hookTimeoutMs?: number;
    budgets?: Record<string, number>;
  },
) {
  const runs = { n: 0 };
  const started = Date.now();
  const iterator = runAgentLoop({
    suspendable: false,
    client: scripted(),
    registry: registry(runs),
    model: "m",
    messages: [],
    limits: resolveToolLimits({ preference: "small" }),
    hooks,
    ...input,
  });
  const events: AiRunEvent[] = [];
  let next = await iterator.next();
  while (!next.done) {
    events.push(next.value);
    next = await iterator.next();
  }
  return {
    events,
    result: next.value,
    toolRuns: runs.n,
    elapsedMs: Date.now() - started,
  };
}

const sleepingGate = (entered: { n: number }, ms: number): AgentLoopHooks => ({
  beforeToolCall: (async () => {
    entered.n += 1;
    await sleep(ms);
    return { action: "allow" };
  }) as AgentLoopHooks["beforeToolCall"],
});

suite(
  "the overall deadline bounds a hook whose own timeout is disabled",
  async () => {
    const entered = { n: 0 };
    const { events, result, toolRuns, elapsedMs } = await drive(
      [sleepingGate(entered, 3_000)],
      { hookTimeoutMs: 0, budgets: { overallMs: 25 } },
    );
    expect(entered.n).toBe(1);
    expect(toolRuns).toBe(0);
    // Generous upper bound (seconds): the pre-fix lane waited the hook's full
    // sleep and beyond; the bounded lane abandons at the deadline.
    expect(elapsedMs).toBeLessThan(2_500);
    expect(result.terminal).toBe("failed");
    const terminal = events.at(-1);
    expect(terminal?.type).toBe("run.failed");
    const data =
      terminal && "data" in terminal
        ? (terminal.data as Record<string, unknown>)
        : {};
    expect(data.errorCode).toBe("execution_timeout");
    expect(String(data.errorMessage)).toContain("overall");
  },
);

suite(
  "the smaller per-hook deadline wins over the overall budget",
  async () => {
    const entered = { n: 0 };
    const { events, result, toolRuns, elapsedMs } = await drive(
      [sleepingGate(entered, 3_000)],
      { hookTimeoutMs: 25, budgets: { overallMs: 20_000 } },
    );
    expect(entered.n).toBe(1);
    expect(toolRuns).toBe(0);
    expect(elapsedMs).toBeLessThan(2_500);
    const failed = events
      .filter((event) => event.type === "run.tool.failed")
      .map((event) => (event as { data?: Record<string, unknown> }).data ?? {});
    expect(failed).toHaveLength(1);
    expect(failed[0]?.errorCode).toBe("tool_hook_denied");
    expect(String(failed[0]?.errorMessage)).toContain(
      "A loop hook could not authorize this tool call.",
    );
    // The denial is ordinary: the run goes on to finish.
    expect(result.terminal).toBe("completed");
  },
);
