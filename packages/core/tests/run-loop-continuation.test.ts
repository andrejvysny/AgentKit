import { describe, expect, it } from "bun:test";
import {
  AiToolRegistry,
  ResponsesClient,
  runChat,
  resolveToolLimits,
  type AiTool,
  type AiProviderContinuation,
} from "../src/index.js";
import {
  call,
  encode,
  harness,
  message,
  request,
  stream,
  terminal,
  tool,
} from "./responses-helpers.js";

async function roundtrip(
  persist: (state: AiProviderContinuation) => Promise<void> = async () => {},
) {
  const saved: AiProviderContinuation[] = [];
  let executed = 0;
  const registry = new AiToolRegistry();
  const registered: AiTool = {
    definition: tool,
    async execute(context) {
      executed++;
      expect(saved).toHaveLength(1);
      return {
        ok: true,
        data: { value: "found" },
        sources: [],
        warnings: [],
        truncated: false,
        limits: context.limits,
      };
    },
  };
  registry.register(registered);
  const { transport } = harness();
  const bodies: Readonly<Record<string, unknown>>[] = [];
  transport.request = async (input) => {
    bodies.push(input.body ?? {});
    return stream(
      encode([
        bodies.length === 1
          ? terminal([
              {
                type: "reasoning",
                id: "reasoning",
                encrypted_content: "opaque",
                summary: [],
              },
              call("exact"),
            ])
          : terminal([message("Done")]),
      ]),
    );
  };
  const generator = runChat({
    client: new ResponsesClient({ id: "provider", transport }),
    registry,
    model: request.model,
    messages: request.messages,
    limits: resolveToolLimits({ preference: "medium" }),
    budgets: { providerRequests: 2 },
    continuationScope: { chatId: "chat", branchId: "branch" },
    async onContinuation(state) {
      await persist(state);
      saved.push(state);
    },
  });
  const events = [];
  for (;;) {
    const next = await generator.next();
    if (next.done)
      return { result: next.value, events, saved, bodies, executed };
    events.push(next.value);
  }
}

describe("runChat Responses continuation forwarding", () => {
  it("one existing loop replays exact state/IDs/tool results across tool iterations", async () => {
    const { result, saved, bodies, executed, events } = await roundtrip();
    expect(result.terminal).toBe("completed");
    expect(result.iterations).toBe(2);
    expect(executed).toBe(1);
    expect(saved).toHaveLength(2);
    const input = bodies[1]?.input as Record<string, unknown>[];
    expect(input[1]).toMatchObject({ encrypted_content: "opaque" });
    expect(input[2]).toMatchObject({ type: "function_call", call_id: "exact" });
    expect(input[3]).toMatchObject({
      type: "function_call_output",
      call_id: "exact",
    });
    expect(result.appendedMessages.map((value) => value.role)).toEqual([
      "assistant",
      "tool",
      "assistant",
    ]);
    expect(JSON.stringify(events)).not.toContain("opaque");
    expect(events.filter((event) => event.type === "run.usage")).toHaveLength(
      2,
    );
  });

  it("failed persistence never reaches tool executor or another provider pass", async () => {
    const { result, executed, bodies, saved } = await roundtrip(async () => {
      throw new Error("storage failed");
    });
    expect(result.terminal).toBe("failed");
    expect(executed).toBe(0);
    expect(bodies).toHaveLength(1);
    expect(saved).toHaveLength(0);
  });
  it("zero request budget prevents Responses dispatch", async () => {
    const { transport, sent } = harness();
    const generator = runChat({
      client: new ResponsesClient({ id: "provider", transport }),
      registry: new AiToolRegistry(),
      model: request.model,
      messages: request.messages,
      limits: resolveToolLimits({ preference: "medium" }),
      budgets: { providerRequests: 0 },
    });
    const events = [];
    for await (const event of generator) events.push(event);
    expect(sent).toHaveLength(0);
    expect(
      events.find((event) => event.type === "run.failed")?.data,
    ).toMatchObject({ errorCode: "execution_budget_exhausted" });
  });
});
