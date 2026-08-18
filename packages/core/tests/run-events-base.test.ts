import { describe, expect, it } from "bun:test";
import { CONTRACT_VERSION } from "@agentkit/contracts";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { MockProviderClient } from "./mocks/mock-provider.js";
import { collectRun } from "./helpers.js";
import type { AiTool } from "../src/tools/tool.js";
import type { AiRunEvent } from "@agentkit/contracts";

function makeEchoTool(): AiTool<{ text: string }, { echoed: string }> {
  return {
    definition: {
      name: "echo",
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

/** A two-turn run: one tool call, then a final answer. */
function toolThenAnswer(): {
  client: MockProviderClient;
  registry: AiToolRegistry;
} {
  const client = new MockProviderClient();
  client.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "c1",
          name: "echo",
          argumentsJson: '{"text":"a"}',
        },
      ],
    },
    { steps: [{ kind: "text", content: "done" }] },
  ]);
  const registry = new AiToolRegistry();
  registry.register(makeEchoTool() as unknown as AiTool);
  return { client, registry };
}

async function runEvents(
  overrides: Partial<Parameters<typeof runChat>[0]> = {},
): Promise<AiRunEvent[]> {
  const { client, registry } = toolThenAnswer();
  const { events } = await collectRun(
    runChat({
      client,
      registry,
      model: "m",
      messages: [{ role: "user", content: "go" }],
      limits: resolveToolLimits({ preference: "small" }),
      ...overrides,
    }),
  );
  return events;
}

describe("run event base fields", () => {
  it("stamps a strictly increasing seq starting at 0", async () => {
    const events = await runEvents();
    expect(events.length).toBeGreaterThan(4);
    expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => i));
  });

  it("continues from firstSeq when resuming a stream", async () => {
    const events = await runEvents({ firstSeq: 100 });
    // A resumed run must not restart the consumer's ordering key at 0 — that
    // would look like a replay of the events it already has.
    expect(events[0]!.seq).toBe(100);
    expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => 100 + i));
  });

  it("gives every event a unique eventId", async () => {
    const events = await runEvents();
    const ids = events.map((e) => e.eventId);
    expect(new Set(ids).size).toBe(ids.length);
    for (const id of ids) expect(id.length).toBeGreaterThan(0);
  });

  it("stamps CONTRACT_VERSION on every event", async () => {
    const events = await runEvents();
    for (const e of events) expect(e.contractVersion).toBe(CONTRACT_VERSION);
  });

  it("propagates attemptId to every event when given, and omits it otherwise", async () => {
    const withAttempt = await runEvents({ attemptId: "attempt_2" });
    for (const e of withAttempt) expect(e.attemptId).toBe("attempt_2");
    const without = await runEvents();
    for (const e of without) expect(e.attemptId).toBeUndefined();
  });

  it("re-stamps provider events so seq stays monotonic across provider calls", async () => {
    // Each provider call numbers its own events from 0. Passing those through
    // unchanged would produce a sequence that restarts mid-run, which reads as
    // a duplicate delivery to any consumer keyed on seq.
    const events = await runEvents();
    const provided = events.filter(
      (e) => e.type === "run.message.completed" || e.type === "run.message.delta",
    );
    expect(provided.length).toBeGreaterThan(1);
    const seqs = provided.map((e) => e.seq);
    expect([...seqs].sort((a, b) => a - b)).toEqual(seqs);
    expect(new Set(seqs).size).toBe(seqs.length);
  });

  it("has the provider client stamp valid base fields on its own events", async () => {
    // streamChat is also consumed directly (capability probes, hosts driving it
    // themselves), so its events must be valid without the loop's help.
    const { client } = toolThenAnswer();
    const direct: AiRunEvent[] = [];
    for await (const e of client.streamChat({
      runId: "r",
      model: "m",
      messages: [{ role: "user", content: "hi" }],
    }))
      direct.push(e);
    expect(direct.length).toBeGreaterThan(0);
    expect(direct.map((e) => e.seq)).toEqual(direct.map((_, i) => i));
    expect(new Set(direct.map((e) => e.eventId)).size).toBe(direct.length);
    for (const e of direct) expect(e.contractVersion).toBe(CONTRACT_VERSION);
  });
});
