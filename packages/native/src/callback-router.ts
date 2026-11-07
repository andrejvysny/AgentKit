import { claimCall, resolveCall, assertProviderAuthority } from "./loader.js";
import type {
  McpElicitationHandler,
  McpOAuthCallbacks,
  McpOAuthHttpRequest,
  ProviderSecretStore,
  ProviderConsentHandler,
  ProviderConsentRequest,
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
  private providerSecrets: ProviderSecretStore | null = null;
  private providerConsent: ProviderConsentHandler | null = null;
  private mcpOAuth: McpOAuthCallbacks | null = null;
  private mcpElicitation: McpElicitationHandler | null = null;
  private contentIdentity:
    | ((
        chatId: string,
        requiredConnectors: readonly string[],
        signal: AbortSignal,
      ) => unknown | Promise<unknown>)
    | null = null;
  private contentAuthority:
    | ((
        chatId: string,
        signal: AbortSignal,
      ) => readonly string[] | Promise<readonly string[]>)
    | null = null;
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

  setProviderSecretStore(store: ProviderSecretStore | null): void {
    this.providerSecrets = store;
  }
  setProviderConsent(handler: ProviderConsentHandler | null): void {
    this.providerConsent = handler;
  }

  setMcpOAuth(callbacks: McpOAuthCallbacks | null): void {
    this.mcpOAuth = callbacks;
  }

  setMcpElicitation(handler: McpElicitationHandler | null): void {
    this.mcpElicitation = handler;
  }

  setContentAuthority(
    authority:
      | ((
          chatId: string,
          signal: AbortSignal,
        ) => readonly string[] | Promise<readonly string[]>)
      | null,
    identity:
      | ((
          chatId: string,
          requiredConnectors: readonly string[],
          signal: AbortSignal,
        ) => unknown | Promise<unknown>)
      | null = null,
  ): void {
    this.contentAuthority = authority;
    this.contentIdentity = identity;
  }

  setProvider(provider: ProviderFunction | null): void {
    this.provider = provider;
  }

  /** Holds the callbacks Rust ACCEPTED for this service: `registerTool` on
   * the service calls the Rust contributor first and only reaches this
   * method when Rust accepted the definition, so a name is registered at
   * most once on both sides of the boundary. The dispatch key is the same
   * name Rust gates and advertises; an overwrite here would put a callback
   * behind the address that the descriptor/gate never authorized, so it is
   * refused outright. */
  registerTool(definition: ToolDefinitionJson, run: ToolRunFunction): void {
    if (this.tools.has(definition.name)) {
      throw new Error(
        `tool ${definition.name} is already registered in this session service`,
      );
    }
    this.tools.set(definition.name, { definition, run });
  }

  close(): void {
    this.closed = true;
    this.provider = null;
    this.providerSecrets = null;
    this.providerConsent = null;
    this.mcpElicitation = null;
    this.mcpOAuth = null;
    this.contentAuthority = null;
    this.contentIdentity = null;
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
    // Liveness gate, answered synchronously by the Rust bridge: every
    // give-up path (executor timeout abandonment, host cancellation, close
    // drain, an already-settled call) removes the pending entry on the Rust
    // side, so a failed claim means this envelope is STALE — the host
    // already settled the call. Settle nothing (Rust did) and, above all,
    // NEVER enter user code. An envelope queued behind a blocked JS thread
    // must not run a callback the runtime abandoned. The per-call
    // AbortController below remains a mid-execution stop signal only; it
    // is not the liveness gate.
    if (
      envelope.kind === "provider" ||
      envelope.kind === "tool" ||
      envelope.kind === "contentAuthority" ||
      envelope.kind === "mcp.elicit" ||
      envelope.kind === "mcp.oauth.secret" ||
      envelope.kind === "mcp.oauth.redirect" ||
      envelope.kind === "mcp.oauth.http" ||
      envelope.kind === "providerSecret" ||
      envelope.kind === "providerConsent"
    ) {
      if (!claimCall(callId)) return;
    }
    try {
      if (envelope.kind === "mcp.oauth.http") {
        const http = this.mcpOAuth?.http;
        if (http === undefined) {
          resolveCall(callId, null, "MCP OAuth HTTP callback unavailable");
          return;
        }
        const request = envelope.request as Omit<McpOAuthHttpRequest, "signal">;
        await this.dispatchUser(async () => {
          const response = await this.withAbort(callId, (signal) =>
            http({ ...request, signal }),
          );
          resolveCall(callId, JSON.stringify(response), null);
        });
        return;
      }
      if (
        envelope.kind === "mcp.oauth.secret" ||
        envelope.kind === "mcp.oauth.redirect"
      ) {
        const callbacks = this.mcpOAuth;
        if (callbacks === null) {
          resolveCall(callId, null, "MCP OAuth callbacks unavailable");
          return;
        }
        const request = envelope.request as {
          operation?: string;
          key?: string;
          value?: string;
          alias?: string;
          url?: string;
        };
        await this.dispatchUser(async () => {
          const result = await this.withAbort(callId, async (signal) => {
            if (
              envelope.kind === "mcp.oauth.redirect" &&
              typeof request.alias === "string" &&
              typeof request.url === "string"
            ) {
              await callbacks.redirect({
                alias: request.alias,
                url: request.url,
                signal,
              });
              return null;
            }
            if (typeof request.key !== "string")
              throw new Error("invalid OAuth secret request");
            if (request.operation === "get")
              return callbacks.secrets.get(request.key, signal);
            if (
              request.operation === "set" &&
              typeof request.value === "string"
            ) {
              await callbacks.secrets.set(request.key, request.value, signal);
              return null;
            }
            if (request.operation === "delete") {
              await callbacks.secrets.delete(request.key, signal);
              return null;
            }
            throw new Error("invalid OAuth callback operation");
          });
          resolveCall(callId, JSON.stringify(result), null);
        });
        return;
      }
      if (envelope.kind === "providerSecret") {
        const request = envelope.request as {
          operation: string;
          reference: string;
          value?: string;
        };
        const store = this.providerSecrets;
        if (store === null) {
          resolveCall(callId, null, "provider secret store unavailable");
          return;
        }
        await this.dispatchUser(async () => {
          const result = await this.withAbort(callId, async (signal) => {
            if (request.operation === "get")
              return { value: await store.get(request.reference, signal) };
            if (
              request.operation === "put" &&
              typeof request.value === "string"
            ) {
              await store.put(request.reference, request.value, signal);
              return { ok: true };
            }
            if (request.operation === "delete") {
              await store.delete(request.reference, signal);
              return { ok: true };
            }
            throw new Error("invalid secret store operation");
          });
          resolveCall(callId, JSON.stringify(result), null);
        });
        return;
      }
      if (envelope.kind === "providerConsent") {
        const request = envelope.request as Omit<
          ProviderConsentRequest,
          "signal"
        >;
        await this.dispatchUser(async () => {
          const result = await this.withAbort(
            callId,
            (signal) =>
              this.providerConsent?.({ ...request, signal }) ?? {
                approved: false,
              },
          );
          resolveCall(callId, JSON.stringify(result), null);
        });
        return;
      }
      if (envelope.kind === "mcp.elicit") {
        const request = envelope.request as { alias: string; params: unknown };
        await this.dispatchUser(async () => {
          const value = await this.withAbort(
            callId,
            (signal) =>
              this.mcpElicitation?.({ ...request, signal }) ?? {
                action: "decline",
              },
          );
          resolveCall(callId, JSON.stringify(value), null);
        });
        return;
      }
      if (envelope.kind === "contentAuthority") {
        const request = envelope.request as {
          chatId: string;
          operation?: string;
          requiredConnectors?: readonly string[];
        };
        await this.dispatchUser(async () => {
          const value = await this.withAbort(callId, (signal) =>
            request.operation === "identity"
              ? (this.contentIdentity?.(
                  request.chatId,
                  request.requiredConnectors ?? [],
                  signal,
                ) ??
                request.requiredConnectors ??
                [])
              : (this.contentAuthority?.(request.chatId, signal) ?? []),
          );
          resolveCall(callId, JSON.stringify(value), null);
        });
        return;
      }
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
            provider({
              ...request,
              signal,
              checkAuthority: () => assertProviderAuthority(callId),
            }),
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
