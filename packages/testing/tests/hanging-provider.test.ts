import { describe, expect, it } from "bun:test";
import { runChat, AiToolRegistry, resolveToolLimits } from "@agentkit/core";
import { CONTRACT_VERSION, type AiRunEvent } from "@agentkit/contracts";
import {
  HangingProviderClient,
  createAbortError,
} from "../src/hanging-provider.js";
import { makeUserMessage } from "../src/fixtures.js";

/** Drain a `runChat` generator, keeping only the events. */
async function collect(
  gen: AsyncGenerator<AiRunEvent, unknown, unknown>,
): Promise<AiRunEvent[]> {
  const events: AiRunEvent[] = [];
  for (;;) {
    const next = await gen.next();
    if (next.done) return events;
    events.push(next.value);
  }
}

describe("HangingProviderClient", () => {
  it("streams its deltas, parks, and throws an AbortError on cancellation", async () => {
    const provider = new HangingProviderClient({
      deltas: ["Think", "ing"],
    });
    const controller = new AbortController();

    const run = collect(
      runChat({
        client: provider,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [makeUserMessage("go")],
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
    );

    // Deterministic: the cancel lands only once the stream is demonstrably
    // parked, with both deltas already emitted.
    await provider.whenBlocking();
    expect(provider.blocking).toBe(true);
    expect(provider.callCount).toBe(1);
    controller.abort();

    const events = await run;
    // The abort surfaced as a throw the loop recognized as cancellation, not
    // as a provider fault.
    expect(events.map((event) => event.type)).toEqual([
      "run.started",
      "run.message.delta",
      "run.message.delta",
      "run.cancelled",
    ]);
    // A stand-in provider still owes contract-valid events.
    for (const event of events) {
      expect(event.contractVersion).toBe(CONTRACT_VERSION);
      expect(typeof event.eventId).toBe("string");
    }
    expect(events.map((event) => event.seq)).toEqual([0, 1, 2, 3]);
  });

  it("honors emitStarted:false and an empty delta list", async () => {
    const provider = new HangingProviderClient({
      emitStarted: false,
      deltas: [],
      id: "parked",
    });
    expect(provider.id).toBe("parked");
    const controller = new AbortController();
    const run = collect(
      runChat({
        client: provider,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [makeUserMessage("go")],
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
    );
    await provider.whenBlocking();
    controller.abort();
    expect((await run).map((event) => event.type)).toEqual(["run.cancelled"]);
  });

  it("reports the capabilities and models it was configured with", async () => {
    const provider = new HangingProviderClient({
      capabilities: { streaming: false, toolCalling: false, modelList: false },
      models: [
        {
          providerId: "hanging",
          modelId: "m1",
          displayName: "M1",
          fetchedAt: "2026-01-01T00:00:00.000Z",
        },
      ],
    });
    expect(await provider.capabilities()).toEqual({
      streaming: false,
      toolCalling: false,
      modelList: false,
    });
    expect(await provider.listModels()).toEqual([
      {
        providerId: "hanging",
        modelId: "m1",
        displayName: "M1",
        fetchedAt: "2026-01-01T00:00:00.000Z",
      },
    ]);
    // Defaults, for a double nobody configured.
    expect(await new HangingProviderClient().capabilities()).toEqual({
      streaming: true,
      toolCalling: true,
      modelList: true,
    });
    expect(await new HangingProviderClient().listModels()).toEqual([]);
  });

  it("refuses to park when the caller handed it no signal", async () => {
    const provider = new HangingProviderClient();
    const iterate = async (): Promise<void> => {
      for await (const _event of provider.streamChat({
        runId: "run-1",
        model: "m",
        messages: [makeUserMessage("go")],
      })) {
        // drain
      }
    };
    // Hanging forever on an un-cancellable stream would present as a timeout
    // with no diagnosis; the double names the wiring bug instead.
    await expect(iterate()).rejects.toThrow("must hand the provider a signal");
  });

  it("mints the AbortError shape a cancelled fetch reports", () => {
    const err = createAbortError();
    expect(err.name).toBe("AbortError");
    expect(err.message).toBe("The operation was aborted.");
    expect(createAbortError("stopped").message).toBe("stopped");
  });
});
