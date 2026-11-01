/**
 * H3 — cancellation arriving during the FINAL synthesized warning must
 * settle the run `cancelled` with NO assistant partial committed.
 *
 * Reviewer probe: aborting while the final `tool_call_unparseable` warning
 * was being synthesized still produced `run.completed` and appended the
 * partial assistant message. The native event pump is lockstep: every
 * emitted event parks the Rust producer at the event gate until the JS
 * consumer asks for the next one. So when this test RECEIVES the warning
 * event, the producer is parked exactly between the warning emit and the
 * pre-commit cancellation recheck — the frozen window the probe hit.
 * Aborting there (and letting the native cancel land, real-thread race →
 * real-timer settle) makes the recheck deterministic without a sleep guess:
 * the producer cannot pass the checkpoint before the consumer resumes it.
 */
import { expect, test } from "bun:test";
import {
  AiToolRegistry,
  resolveToolLimits,
  runChat,
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

// Real-timer settle for the napi cancel call to land on the Rust side
// before the lockstep consumer releases the parked warning emit. The
// correctness check itself is not a sleep race: the producer cannot pass
// the recheck until THIS loop resumes it, so the only thing the delay
// orders is cancel-vs-resume dispatch, both already dispatched here.
const settle = (ms: number): Promise<void> => {
  const { promise, resolve } = Promise.withResolvers<void>();
  setTimeout(resolve, ms);
  return promise;
};

suite(
  "abort during the final warning settles cancelled without an assistant partial",
  async () => {
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [{ kind: "text", content: "partial" }],
        finishReason: "tool_calls",
      },
    ]);
    const controller = new AbortController();
    const iterator = runChat({
      client,
      registry: new AiToolRegistry(),
      model: "m",
      messages: [],
      limits: resolveToolLimits({ preference: "small" }),
      signal: controller.signal,
    });
    const events: AiRunEvent[] = [];
    let sawFinalWarning = false;
    let next = await iterator.next();
    while (!next.done) {
      const event = next.value;
      events.push(event);
      const data =
        "data" in event ? (event.data as Record<string, unknown>) : {};
      if (
        event.type === "run.warning" &&
        data.code === "tool_call_unparseable" &&
        !sawFinalWarning
      ) {
        // The Rust producer is parked at THIS event's gate: the abort lands
        // exactly inside the final-warning synthesis window.
        sawFinalWarning = true;
        controller.abort();
        await settle(25);
      }
      next = await iterator.next();
    }
    const result = next.value;
    expect(sawFinalWarning).toBe(true);
    expect(result.terminal).toBe("cancelled");
    const kinds = events.map((event) => event.type);
    expect(kinds).toContain("run.cancelled");
    expect(kinds).not.toContain("run.completed");
    // Store-level witness: nothing was appended to commit — no assistant
    // partial, and no tool/other carry-over either.
    expect(
      result.appendedMessages.filter((message) => message.role === "assistant"),
    ).toEqual([]);
    expect(JSON.stringify(result.appendedMessages).includes("partial")).toBe(
      false,
    );
  },
);
