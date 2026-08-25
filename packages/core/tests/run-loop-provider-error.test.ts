import { describe, expect, it } from "bun:test";
import { runChat } from "../src/runs/run-loop.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { nowIso } from "../src/ids.js";
import { collectRun } from "./helpers.js";
import { createEventStamper } from "../src/events.js";
import type {
  AiChatRequest,
  AiProviderClient,
} from "../src/providers/client.js";
import type {
  AiProviderCapabilities,
  AiProviderModel,
  AiRunEvent,
} from "@agentkit/contracts";

/** Events only, for the cases that don't care about the return value. */
async function collect(
  input: Parameters<typeof runChat>[0],
): Promise<AiRunEvent[]> {
  return (await collectRun(runChat(input))).events;
}

/**
 * A client whose stream yields a couple of events and then throws — the shape of
 * a socket reset, a malformed body, or a bug in a third-party client. The
 * contract says a client SHOULD emit run.failed itself; this one doesn't.
 */
class ThrowingProviderClient implements AiProviderClient {
  readonly id = "throwing";
  readonly kind = "openai-compatible";
  constructor(private readonly error: unknown) {}

  async capabilities(): Promise<AiProviderCapabilities> {
    return { streaming: true, toolCalling: true, modelList: false };
  }
  async listModels(): Promise<AiProviderModel[]> {
    return [];
  }
  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const stamp = createEventStamper();
    yield stamp({
      type: "run.started",
      runId: input.runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: 0 },
    });
    yield stamp({
      type: "run.message.delta",
      runId: input.runId,
      timestamp: nowIso(),
      data: { delta: "half an ans" },
    });
    throw this.error;
  }
}

const TERMINAL = new Set(["run.completed", "run.failed", "run.cancelled"]);

describe("runChat — provider throws mid-stream", () => {
  it("normalizes the throw into exactly one terminal run.failed", async () => {
    const { events, result } = await collectRun(
      runChat({
        client: new ThrowingProviderClient(new Error("socket hang up")),
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content: "hi" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );

    expect(result.terminal).toBe("failed");
    const terminals = events.filter((e) => TERMINAL.has(e.type));
    expect(terminals.length).toBe(1);
    const failed = terminals[0] as AiRunEvent & {
      data: { errorMessage: string; errorCode?: string };
    };
    expect(failed.type).toBe("run.failed");
    expect(failed.data.errorCode).toBe("provider_error");
    expect(failed.data.errorMessage).toBe("socket hang up");
    // Events emitted before the throw are kept — a partial answer is still real.
    expect(events.some((e) => e.type === "run.message.delta")).toBe(true);
    expect(events.some((e) => e.type === "run.completed")).toBe(false);
  });

  it("describes a non-Error throw without exploding", async () => {
    const events = await collect({
      client: new ThrowingProviderClient("plain string rejection"),
      registry: new AiToolRegistry(),
      model: "m",
      messages: [{ role: "user", content: "hi" }],
      limits: resolveToolLimits({ preference: "small" }),
    });
    const failed = events.find((e) => e.type === "run.failed") as
      | (AiRunEvent & { data: { errorMessage: string; errorCode?: string } })
      | undefined;
    expect(failed?.data.errorCode).toBe("provider_error");
    expect(failed?.data.errorMessage).toBe("plain string rejection");
  });

  it("classifies an AbortError throw as cancellation, not failure", async () => {
    const abortError = new Error("The operation was aborted.");
    abortError.name = "AbortError";
    const { events, result } = await collectRun(
      runChat({
        client: new ThrowingProviderClient(abortError),
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content: "hi" }],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );
    expect(result.terminal).toBe("cancelled");
    const terminals = events.filter((e) => TERMINAL.has(e.type));
    expect(terminals.length).toBe(1);
    expect(terminals[0]!.type).toBe("run.cancelled");
    expect(events.some((e) => e.type === "run.failed")).toBe(false);
  });

  it("classifies a throw under an aborted signal as cancellation", async () => {
    const controller = new AbortController();
    controller.abort();
    // Signal aborted at the top of iteration 1 — the loop cancels before it ever
    // reaches the client, which is the same terminal from the caller's view.
    const { events, result } = await collectRun(
      runChat({
        client: new ThrowingProviderClient(new Error("aborted")),
        registry: new AiToolRegistry(),
        model: "m",
        messages: [{ role: "user", content: "hi" }],
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
    );
    expect(result.terminal).toBe("cancelled");
    const terminals = events.filter((e) => TERMINAL.has(e.type));
    expect(terminals.length).toBe(1);
    expect(terminals[0]!.type).toBe("run.cancelled");
  });
});
