import { describe, expect, it } from "bun:test";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";
import { MockProviderClient } from "@agentkit/testing";
import type { AiRunEvent } from "@agentkit/contracts";
import type { RunChatResult } from "../src/runs/run-loop.js";

const enc = new TextEncoder();

/**
 * A provider that streams one content delta and then STOPS producing bytes
 * without ever closing — the stalled-proxy shape. Nothing but the abort can end
 * a run against it, which is exactly what makes it the honest cancellation
 * double: the existing `HangingProviderClient` throws an `AbortError` of its
 * own and so never exercises the real parser.
 */
function stallingFetch(): typeof fetch {
  return (async () =>
    new Response(
      new ReadableStream<Uint8Array>({
        start(c) {
          c.enqueue(
            enc.encode(
              `data: ${JSON.stringify({
                choices: [{ delta: { content: "half an ans" } }],
              })}\n\n`,
            ),
          );
        },
      }),
      { status: 200, headers: { "content-type": "text/event-stream" } },
    )) as unknown as typeof fetch;
}

/** Drain a run, aborting the first time an event of `abortOn` is yielded. */
async function runAndAbortOn(
  gen: AsyncGenerator<AiRunEvent, RunChatResult, unknown>,
  abortOn: string,
  controller: AbortController,
): Promise<{ events: AiRunEvent[]; result: RunChatResult }> {
  const events: AiRunEvent[] = [];
  for (;;) {
    const next = await gen.next();
    if (next.done) return { events, result: next.value };
    events.push(next.value);
    if (next.value.type === abortOn) controller.abort();
  }
}

describe("runChat — cancellation through the real provider client", () => {
  it("ends a stalled stream as cancelled, with no assistant message", async () => {
    const controller = new AbortController();
    const client = new OpenAiCompatibleClient({
      id: "stalling",
      kind: "openai-compatible",
      baseUrl: "http://localhost:9/v1",
      fetchImpl: stallingFetch(),
    });

    const { events, result } = await runAndAbortOn(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
      "run.message.delta",
      controller,
    );

    expect(result.terminal).toBe("cancelled");
    expect(events.at(-1)!.type).toBe("run.cancelled");
    // The half sentence must NOT be committed: the parser used to return
    // cleanly on abort, which made the client report a finished message and the
    // loop complete the run.
    expect(events.some((e) => e.type === "run.message.completed")).toBe(false);
    expect(events.some((e) => e.type === "run.completed")).toBe(false);
    expect(result.appendedMessages).toEqual([]);
  });

  it("does not commit the final assistant message when cancelled after the last chunk", async () => {
    // The provider finished cleanly; the user cancelled while the loop was
    // between the completed event and its commit.
    const controller = new AbortController();
    const client = new MockProviderClient();
    client.setScript([{ steps: [{ kind: "text", content: "done answer" }] }]);

    const { events, result } = await runAndAbortOn(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content: "go" }],
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
      "run.message.completed",
      controller,
    );

    expect(result.terminal).toBe("cancelled");
    expect(events.at(-1)!.type).toBe("run.cancelled");
    expect(events.some((e) => e.type === "run.completed")).toBe(false);
    expect(result.appendedMessages).toEqual([]);
  });
});
