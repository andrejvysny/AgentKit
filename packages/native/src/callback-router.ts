import { resolveCall } from "./loader.js";
import type {
  ProviderFunction,
  ProviderRequest,
  ToolCallRequest,
  ToolDefinitionJson,
  ToolRunFunction,
} from "./callback-types.js";

interface RegisteredTool {
  definition: ToolDefinitionJson;
  run: ToolRunFunction;
}

interface CallbackEnvelope {
  kind?: unknown;
  callId: string;
  request?: unknown;
}

function parseEnvelope(raw: string): CallbackEnvelope | undefined {
  try {
    const value: unknown = JSON.parse(raw);
    if (
      typeof value === "object" &&
      value !== null &&
      "callId" in value &&
      typeof value.callId === "string"
    )
      return value as CallbackEnvelope;
  } catch {
    // Nothing addressable; Rust-side shutdown still reaps the pending call.
  }
  return undefined;
}

/**
 * The single JS entry point for every Rust-originated callback. One
 * callback = one in-flight call; the wrapper resolves it exactly once. A
 * user function that throws (or rejects) settles the call as a failure —
 * never as a success, never as a dangling entry.
 */
export class CallbackRouter {
  private provider: ProviderFunction | null = null;
  private tools = new Map<string, RegisteredTool>();
  private closed = false;
  /** Per-call abort controllers for dispatched provider/tool callbacks. */
  private controllers = new Map<string, AbortController>();
  /**
   * Dispatched callbacks whose user function has not settled yet. The
   * Rust bridge CANNOT carry this count: `Closing` drains its pending map
   * (rejecting the Rust-side waiter) while the user promise may still be
   * running outside the runtime. This counter spans exactly the window
   * the runtime cannot control — from entering the user function until
   * its promise settles — and survives `close()`, because that is when
   * the distinction matters.
   */
  private inFlight = 0;

  get pendingEffects(): number {
    return this.inFlight;
  }

  setProvider(provider: ProviderFunction | null): void {
    this.provider = provider;
  }

  registerTool(definition: ToolDefinitionJson, run: ToolRunFunction): void {
    this.tools.set(definition.name, { definition, run });
  }

  close(): void {
    this.closed = true;
    this.provider = null;
    for (const controller of this.controllers.values()) controller.abort();
    this.controllers.clear();
    this.tools.clear();
  }

  /** Invoked from Rust with one JSON envelope per callback. Must never
   * throw: an escaping exception would strand the Rust pending entry. */
  async handle(raw: string): Promise<void> {
    const envelope = parseEnvelope(raw);
    if (!envelope) return;
    const { callId } = envelope;
    if (this.closed) {
      resolveCall(callId, null, "the binding closed before the callback ran");
      return;
    }
    // Host-side give-up notifications (the bridge settled the pending call
    // without this callback's answer). They are NOT settles: the entry is
    // already gone on the Rust side. Their one job is to abort the user
    // function's signal so consumer-owned effects (spawned processes,
    // network) stop instead of running on unobserved.
    if (envelope.kind === "cancel" || envelope.kind === "discard") {
      const controller = this.controllers.get(callId);
      if (controller !== undefined) {
        this.controllers.delete(callId);
        controller.abort();
      }
      return;
    }
    try {
      if (envelope.kind === "provider") {
        const provider = this.provider;
        if (provider === null) {
          resolveCall(
            callId,
            null,
            "no provider is registered for this session service",
          );
          return;
        }
        const request = envelope.request as ProviderRequest;
        const outcome = await this.dispatchUser(async () => {
          const result = await this.withAbort(callId, (signal) =>
            provider({ ...request, signal }),
          );
          resolveCall(callId, JSON.stringify(result ?? {}), null);
        });
        return outcome;
      }
      if (envelope.kind === "tool") {
        const request = envelope.request as ToolCallRequest;
        const tool = this.tools.get(request?.name);
        if (tool === undefined) {
          resolveCall(callId, null, `no JS tool named ${request?.name}`);
          return;
        }
        await this.dispatchUser(async () => {
          const outcome = await this.withAbort(callId, (signal) =>
            tool.run(request.input, { ...request, signal }),
          );
          resolveCall(callId, JSON.stringify(outcome ?? {}), null);
        });
        return;
      }
      resolveCall(callId, null, `unknown callback kind`);
    } catch (error) {
      resolveCall(
        callId,
        null,
        error instanceof Error ? error.message : String(error),
      );
    }
  }

  /** Registers the per-call abort controller before the user function is
   * entered, so a cancel/discard notification racing dispatch still finds
   * (and removes) the entry. */
  private async withAbort<T>(
    callId: string,
    run: (signal: AbortSignal) => T | Promise<T>,
  ): Promise<T> {
    const controller = new AbortController();
    this.controllers.set(callId, controller);
    try {
      return await run(controller.signal);
    } finally {
      this.controllers.delete(callId);
    }
  }

  /** One entry into USER code and its settlement — the only window the
   * runtime cannot control, counted for `pendingEffects`. Errors pass
   * through to the caller's rejection handling; the counter always
   * settles. */
  private async dispatchUser(run: () => Promise<void>): Promise<void> {
    this.inFlight += 1;
    try {
      await run();
    } finally {
      this.inFlight -= 1;
    }
  }
}
