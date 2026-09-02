import { describe, expect, it } from "bun:test";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { messageContentToText } from "../src/messages/content.js";
import { MockProviderClient, type MockScriptStep } from "@agentkit/testing";
import { collectRun } from "./helpers.js";
import type { AiTool } from "../src/tools/tool.js";
import type { AiRunEvent, AiToolEnvelope } from "@agentkit/contracts";

const TERMINAL = new Set(["run.completed", "run.failed", "run.cancelled"]);

function toolCallStep(name: string): MockScriptStep {
  return { kind: "tool_call", toolCallId: "c1", name, argumentsJson: "{}" };
}

/** A tool with no arguments whose body is whatever the test needs. */
function makeTool(
  name: string,
  execute: AiTool<Record<string, never>, unknown>["execute"],
  timeoutMs?: number,
): AiTool {
  return {
    definition: {
      name,
      version: "1",
      effect: "read",
      capability: "test",
      description: name,
      inputSchema: { type: "object", properties: {} },
      ...(timeoutMs === undefined ? {} : { timeoutMs }),
    },
    execute,
  } as unknown as AiTool;
}

function scriptedRun(
  tool: AiTool,
  extra: { defaultToolTimeoutMs?: number } = {},
) {
  const client = new MockProviderClient();
  client.setScript([
    { steps: [toolCallStep(tool.definition.name)] },
    { steps: [{ kind: "text", content: "carried on" }] },
  ]);
  const registry = new AiToolRegistry();
  registry.register(tool);
  return collectRun(
    runChat({
      client,
      registry,
      model: "m",
      messages: [{ role: "user", content: "go" }],
      limits: resolveToolLimits({ preference: "small" }),
      ...extra,
    }),
  );
}

const failedEvents = (events: AiRunEvent[]) =>
  events.filter((e) => e.type === "run.tool.failed") as Array<
    AiRunEvent & { data: { errorCode?: string; errorMessage: string } }
  >;

describe("runChat — unserializable tool results", () => {
  it("reports a BigInt result as result_unserializable and still terminates", async () => {
    const { events, result } = await scriptedRun(
      makeTool("bigint_tool", async (ctx) => ({
        ok: true,
        // Real host data: a counter read straight off a driver.
        data: { count: BigInt(9007199254740993n) },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      })),
    );

    const failed = failedEvents(events);
    expect(failed.length).toBe(1);
    expect(failed[0]!.data.errorCode).toBe("result_unserializable");
    // The throw used to escape runChat entirely: no terminal event, orphan
    // tool_call_id, run left unfinished.
    expect(events.filter((e) => TERMINAL.has(e.type)).length).toBe(1);
    expect(result.terminal).toBe("completed");

    const toolMsg = result.appendedMessages.find((m) => m.role === "tool");
    const envelope = JSON.parse(
      messageContentToText(toolMsg!.content),
    ) as AiToolEnvelope & { data: { phase?: string; retryable?: boolean } };
    expect(envelope.ok).toBe(false);
    expect(envelope.data.phase).toBe("execution");
    expect(envelope.data.retryable).toBe(false);
  });
});

describe("runChat — tool deadlines", () => {
  it("gives up on a tool that ignores its abort signal", async () => {
    const started = Date.now();
    const { events, result } = await scriptedRun(
      makeTool(
        "sleepy",
        (ctx) =>
          new Promise((resolve) =>
            // Ignores ctx.signal entirely, the way a remote tool behind a
            // library that never wired cancellation does.
            setTimeout(
              () =>
                resolve({
                  ok: true,
                  data: { late: true },
                  sources: [],
                  warnings: [],
                  truncated: false,
                  limits: ctx.limits,
                }),
              800,
            ),
          ),
        50,
      ),
    );
    const elapsed = Date.now() - started;

    const failed = failedEvents(events);
    expect(failed.length).toBe(1);
    expect(failed[0]!.data.errorMessage).toContain("timed out after 50ms");
    // The deadline must END the wait, not merely signal it.
    expect(elapsed).toBeLessThan(600);
    expect(result.terminal).toBe("completed");
    // The late result is dropped, not appended after the failure.
    expect(events.some((e) => e.type === "run.tool.succeeded")).toBe(false);
  });

  it("applies defaultToolTimeoutMs to a tool that declares none", async () => {
    const { events } = await scriptedRun(
      makeTool(
        "unbounded",
        () => new Promise(() => {}) as Promise<never>,
        undefined,
      ),
      { defaultToolTimeoutMs: 50 },
    );
    expect(failedEvents(events)[0]!.data.errorMessage).toContain(
      "timed out after 50ms",
    );
  });
});

describe("runChat — output budget", () => {
  it("caps an over-budget result in the envelope fed to the model", async () => {
    const client = new MockProviderClient();
    client.setScript([
      { steps: [toolCallStep("chatty")] },
      { steps: [{ kind: "text", content: "ok" }] },
    ]);
    const registry = new AiToolRegistry();
    registry.register(
      makeTool("chatty", async (ctx) => ({
        ok: true,
        data: "x".repeat(50_000),
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      })),
    );
    const limits = resolveToolLimits({
      preference: "small",
      requestedMaxBytes: 2048,
    });

    const { events, result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits,
      }),
    );

    const toolMsg = result.appendedMessages.find((m) => m.role === "tool");
    const content = messageContentToText(toolMsg!.content);
    // Replayed into every later request of the run, so the cap is what keeps a
    // 50 KB result from being paid for again and again.
    expect(new TextEncoder().encode(content).length).toBeLessThanOrEqual(
      limits.maxBytes,
    );
    const envelope = JSON.parse(content) as AiToolEnvelope;
    expect(envelope.truncated).toBe(true);
    const succeeded = events.find((e) => e.type === "run.tool.succeeded") as
      | (AiRunEvent & { data: { truncated: boolean; resultJson: string } })
      | undefined;
    expect(succeeded!.data.truncated).toBe(true);
    // The UI's copy keeps the whole thing.
    expect(succeeded!.data.resultJson.length).toBeGreaterThan(50_000);
  });

  it("previews a non-string payload rather than emitting invalid JSON", async () => {
    const client = new MockProviderClient();
    client.setScript([
      { steps: [toolCallStep("rows")] },
      { steps: [{ kind: "text", content: "ok" }] },
    ]);
    const registry = new AiToolRegistry();
    registry.register(
      makeTool("rows", async (ctx) => ({
        ok: true,
        data: { rows: Array.from({ length: 2000 }, (_, i) => ({ i })) },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      })),
    );
    const limits = resolveToolLimits({
      preference: "small",
      requestedMaxBytes: 2048,
    });

    const { result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits,
      }),
    );

    const content = messageContentToText(
      result.appendedMessages.find((m) => m.role === "tool")!.content,
    );
    const envelope = JSON.parse(content) as AiToolEnvelope & {
      data: { truncated?: boolean; preview?: string };
    };
    expect(envelope.truncated).toBe(true);
    expect(envelope.data.truncated).toBe(true);
    expect(typeof envelope.data.preview).toBe("string");
    expect(new TextEncoder().encode(content).length).toBeLessThanOrEqual(
      limits.maxBytes,
    );
  });
});

describe("runChat — iteration budget", () => {
  it("clamps maxToolIterations to at least one provider round-trip", async () => {
    const client = new MockProviderClient();
    client.setScript([{ steps: [{ kind: "text", content: "an answer" }] }]);

    const { result } = await collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
        maxToolIterations: 0,
      }),
    );

    // 0 used to mean "succeed having asked nobody anything".
    expect(client.callCount).toBe(1);
    expect(result.terminal).toBe("completed");
    expect(result.appendedMessages.length).toBe(1);
  });
});

describe("runChat — finish reasons", () => {
  it("keeps an incomplete turn incomplete instead of calling it stop", async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [{ kind: "text", content: "half" }],
        finishReason: "incomplete",
      },
    ]);

    const { events } = await collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    const completed = events.find((e) => e.type === "run.completed") as
      | (AiRunEvent & { data: { finishReason?: string } })
      | undefined;
    expect(completed!.data.finishReason).toBe("incomplete");
  });

  it("warns about a length-truncated turn even when it carried tool calls", async () => {
    const client = new MockProviderClient();
    client.setScript([
      { steps: [toolCallStep("noop")], finishReason: "length" },
      { steps: [{ kind: "text", content: "ok" }] },
    ]);
    const registry = new AiToolRegistry();
    registry.register(
      makeTool("noop", async (ctx) => ({
        ok: true,
        data: null,
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      })),
    );

    const { events } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    // Cut-off tool arguments fail as bad_args with no hint why; the warning is
    // the hint, and it used to be emitted only on turns with no tool calls.
    expect(
      events.some(
        (e) =>
          e.type === "run.warning" &&
          (e as { data: { code: string } }).data.code === "truncated",
      ),
    ).toBe(true);
  });
});
