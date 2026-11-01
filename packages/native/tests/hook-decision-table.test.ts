/**
 * H1 — the `beforeToolCall` decision table through the PUBLIC binding.
 *
 * The frozen contract (`runToolGate` in `packages/core/src/runs/loop-hooks.ts`,
 * mirrored test-for-test by `packages/core/tests/agent-loop-hooks.test.ts`
 * on the TypeScript lane) accepts exactly two verdict shapes: `undefined`
 * (no opinion) and `{ action: "allow" }`. EVERYTHING else — `null`, `NaN`,
 * booleans, numbers, strings, arrays, objects without a plain-string
 * `action`, `deny` without a string `reason`, a hung hook, a throwing hook
 * — denies with the fixed gate sentence, and a `deny` with a plain-string
 * `reason` denies with that reason. A JSON bridge that collapses
 * `undefined`/`null`/`NaN` into one wire value silently turns frozen denies
 * into allows; this suite pins the native lane to the frozen table using
 * tagged envelopes across the boundary.
 */
import { describe, expect, test } from "bun:test";
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
const GATE_MESSAGE = "A loop hook could not authorize this tool call.";

process.env.AGENTKIT_BACKEND = "native";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

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

async function gateRun(
  hooks: readonly AgentLoopHooks[],
  hookTimeoutMs?: number,
) {
  const runs = { n: 0 };
  const iterator = runAgentLoop({
    suspendable: false,
    client: scripted(),
    registry: registry(runs),
    model: "m",
    messages: [],
    limits: resolveToolLimits({ preference: "small" }),
    hooks,
    ...(hookTimeoutMs === undefined ? {} : { hookTimeoutMs }),
  });
  const events: AiRunEvent[] = [];
  let next = await iterator.next();
  while (!next.done) {
    events.push(next.value);
    next = await iterator.next();
  }
  return { events, result: next.value, toolRuns: runs.n };
}

function failedData(events: AiRunEvent[]): Record<string, unknown>[] {
  return events
    .filter((event) => event.type === "run.tool.failed")
    .map((event) => (event as { data?: Record<string, unknown> }).data ?? {});
}

const verdict = (value: unknown): AgentLoopHooks => ({
  beforeToolCall: (() => value) as AgentLoopHooks["beforeToolCall"],
});

describe("beforeToolCall allow table — tool executes, run completes", () => {
  for (const [label, hook] of [
    ["undefined (no opinion)", () => undefined],
    ["explicit allow", () => ({ action: "allow" as const })],
  ] as const) {
    suite(`allows ${label}`, async () => {
      const { events, result, toolRuns } = await gateRun([
        { beforeToolCall: hook as AgentLoopHooks["beforeToolCall"] },
      ]);
      expect(toolRuns).toBe(1);
      expect(failedData(events)).toHaveLength(0);
      expect(result.terminal).toBe("completed");
      const assistant = result.appendedMessages.filter(
        (message) => message.role === "assistant",
      );
      expect(JSON.stringify(assistant.at(-1))).toContain("final");
    });
  }
});

describe("beforeToolCall deny table — tool never runs, run goes on", () => {
  const rows: Array<[string, unknown, string]> = [
    ["deny with reason", { action: "deny", reason: "nope" }, "nope"],
    ["deny without reason", { action: "deny" }, GATE_MESSAGE],
    ["deny with numeric reason", { action: "deny", reason: 42 }, GATE_MESSAGE],
    ["null", null, GATE_MESSAGE],
    ["NaN", Number.NaN, GATE_MESSAGE],
    ["Infinity", Number.POSITIVE_INFINITY, GATE_MESSAGE],
    ["false", false, GATE_MESSAGE],
    ["true", true, GATE_MESSAGE],
    ["zero", 0, GATE_MESSAGE],
    ["number", 42, GATE_MESSAGE],
    ["empty string", "", GATE_MESSAGE],
    ["string allow", "allow", GATE_MESSAGE],
    ["array", [], GATE_MESSAGE],
    ["empty object", {}, GATE_MESSAGE],
    ["unrecognized action", { action: "maybe" }, GATE_MESSAGE],
    ["numeric action", { action: 42 }, GATE_MESSAGE],
    ["bigint", 10n, GATE_MESSAGE],
    [
      "circular object",
      Object.assign(
        {},
        {
          get self(): unknown {
            return this;
          },
        },
      ),
      GATE_MESSAGE,
    ],
  ];
  for (const [label, value, expected] of rows) {
    suite(`denies ${label}`, async () => {
      const { events, result, toolRuns } = await gateRun([verdict(value)]);
      expect(toolRuns).toBe(0);
      const failed = failedData(events);
      expect(failed).toHaveLength(1);
      expect(failed[0]?.errorCode).toBe("tool_hook_denied");
      expect(String(failed[0]?.errorMessage)).toContain(expected);
      expect(result.terminal).toBe("completed");
      // The run goes on to answer after the guarded refusal.
      const assistant = result.appendedMessages.filter(
        (message) => message.role === "assistant",
      );
      expect(JSON.stringify(assistant.at(-1))).toContain("final");
    });
  }
});

describe("beforeToolCall failure modes deny like garbage", () => {
  suite("a hung hook denies at the per-hook deadline", async () => {
    const { events, result, toolRuns } = await gateRun(
      [
        {
          beforeToolCall: (async () => {
            await sleep(1_000);
            return { action: "allow" };
          }) as AgentLoopHooks["beforeToolCall"],
        },
      ],
      20,
    );
    expect(toolRuns).toBe(0);
    const failed = failedData(events);
    expect(failed).toHaveLength(1);
    expect(failed[0]?.errorCode).toBe("tool_hook_denied");
    expect(String(failed[0]?.errorMessage)).toContain(GATE_MESSAGE);
    expect(result.terminal).toBe("completed");
  });

  suite("a throwing hook denies without leaking its message", async () => {
    const { events, toolRuns } = await gateRun([
      {
        beforeToolCall: (async () => {
          throw new Error("SECRET-STACK");
        }) as AgentLoopHooks["beforeToolCall"],
      },
    ]);
    expect(toolRuns).toBe(0);
    const failed = failedData(events);
    expect(failed).toHaveLength(1);
    expect(failed[0]?.errorCode).toBe("tool_hook_denied");
    expect(String(failed[0]?.errorMessage)).toContain(GATE_MESSAGE);
    expect(JSON.stringify(failed)).not.toContain("SECRET-STACK");
  });

  suite("the first deny short-circuits later hooks", async () => {
    const seen: string[] = [];
    const { toolRuns } = await gateRun([
      {
        beforeToolCall: (() => ({
          action: "deny",
          reason: "no",
        })) as AgentLoopHooks["beforeToolCall"],
      },
      {
        beforeToolCall: ((context: { toolCallId: string }) => {
          seen.push(context.toolCallId);
          return { action: "allow" };
        }) as AgentLoopHooks["beforeToolCall"],
      },
    ]);
    expect(toolRuns).toBe(0);
    expect(seen).toEqual([]);
  });

  suite("observer return values are inert", async () => {
    // Observers never authorize anything: returning `undefined` (and even
    // garbage) from `onModelResponse` must not change the run outcome.
    const { result, toolRuns } = await gateRun([
      {
        onModelResponse: (() => undefined) as AgentLoopHooks["onModelResponse"],
      },
      {
        onModelResponse: (() =>
          null) as unknown as AgentLoopHooks["onModelResponse"],
      },
    ]);
    expect(toolRuns).toBe(1);
    expect(result.terminal).toBe("completed");
  });
});
