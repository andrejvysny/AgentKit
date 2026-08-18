import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import type {
  AiProviderCapabilities,
  AiProviderKind,
  AiProviderModel,
  AiRunEvent,
} from "@agentkit/contracts";
import { createTestEventStamper, nowIso } from "./stamp.js";

export interface HangingProviderOptions {
  /** Reported as the client's `id`. Default `"hanging"`. */
  id?: string;
  /** Reported as the client's `kind`. Default `"openai-compatible"`. */
  kind?: AiProviderKind;
  /** Emit `run.started` before parking. Default true. */
  emitStarted?: boolean;
  /**
   * Content chunks yielded as `run.message.delta` before parking. Default one
   * chunk, `"Thinking"` — enough that a cancelled run has a partial answer to
   * assert about. Pass `[]` to park with nothing streamed.
   */
  deltas?: readonly string[];
  capabilities?: AiProviderCapabilities;
  models?: readonly AiProviderModel[];
}

/**
 * The `AbortError` a cancelled `fetch` reports, without depending on
 * `DOMException` — duck-typed on `name`, which is what
 * `@agentkit/core`'s run loop actually inspects.
 */
export function createAbortError(
  message = "The operation was aborted.",
): Error {
  const err = new Error(message);
  err.name = "AbortError";
  return err;
}

/** Resolves when `signal` aborts (immediately if it already has). */
function untilAborted(signal: AbortSignal | undefined): Promise<void> {
  if (!signal) {
    return Promise.reject(
      new Error("the run loop must hand the provider a signal"),
    );
  }
  if (signal.aborted) return Promise.resolve();
  return new Promise<void>((resolve) => {
    signal.addEventListener("abort", () => resolve(), { once: true });
  });
}

/**
 * A provider that streams a little and then hangs until the run is cancelled,
 * failing the way a real one does: the aborted `fetch` throws an `AbortError`.
 *
 * This is the double for every cancellation test that would otherwise be a race
 * against a sleep. {@link HangingProviderClient.blocking} (and
 * {@link HangingProviderClient.whenBlocking}) is the deterministic handle: it
 * says the stream is demonstrably parked mid-turn, with its deltas already
 * emitted and persisted, so a test can cancel at a known point instead of
 * guessing whether the stream got started.
 *
 * A missing `signal` is a hard failure rather than an infinite hang — handing a
 * provider no way to be cancelled is the wiring bug this double is best placed
 * to catch.
 *
 * Events are stamped through {@link createTestEventStamper}: a stand-in provider
 * still owes its consumer contract-valid events, base fields included.
 */
export class HangingProviderClient implements AiProviderClient {
  readonly id: string;
  readonly kind: AiProviderKind;
  /** True once the stream is parked, waiting to be aborted. */
  blocking = false;
  /** How many times `streamChat` has been invoked. */
  callCount = 0;

  private readonly options: HangingProviderOptions;
  private readonly parked: Promise<void>;
  private announceParked!: () => void;

  constructor(options: HangingProviderOptions = {}) {
    this.options = options;
    this.id = options.id ?? "hanging";
    this.kind = options.kind ?? "openai-compatible";
    this.parked = new Promise<void>((resolve) => {
      this.announceParked = resolve;
    });
  }

  /** Resolves the first time the stream parks. Await instead of polling. */
  whenBlocking(): Promise<void> {
    return this.parked;
  }

  async capabilities(): Promise<AiProviderCapabilities> {
    return (
      this.options.capabilities ?? {
        streaming: true,
        toolCalling: true,
        modelList: true,
      }
    );
  }

  async listModels(): Promise<AiProviderModel[]> {
    return [...(this.options.models ?? [])];
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.callCount++;
    const stamp = createTestEventStamper();
    if (this.options.emitStarted !== false) {
      yield stamp({
        type: "run.started",
        runId: input.runId,
        timestamp: nowIso(),
        data: { model: input.model, toolCount: input.tools?.length ?? 0 },
      });
    }
    for (const delta of this.options.deltas ?? ["Thinking"]) {
      yield stamp({
        type: "run.message.delta",
        runId: input.runId,
        timestamp: nowIso(),
        data: { delta },
      });
    }
    this.blocking = true;
    this.announceParked();
    await untilAborted(input.signal);
    throw createAbortError();
  }
}
