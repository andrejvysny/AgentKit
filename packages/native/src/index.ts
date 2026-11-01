/**
 * Thin TypeScript wrapper over the native AgentKit binding.
 *
 * Everything runtime-related — sessions, storage, governance, durable
 * execution, recovery — lives in Rust. This file only moves JSON across the
 * boundary, routes provider/tool callbacks to the functions registered per
 * handle, and maps the stable error contract. It deliberately contains no
 * session, queue, approval, or event logic: if code here could fail a
 * runtime invariant, that logic belongs in Rust instead.
 */

import {
  withNativeTransaction,
  type NativeStorePort,
} from "./store-transaction.js";
export type { NativeStorePort } from "./store-transaction.js";
import type {
  NativeStore as RawNativeStore,
  SessionService as RawSessionService,
  SubscriptionHandle as RawSubscriptionHandle,
} from "../generated/index.js";
import { loadBinding } from "./loader.js";
import { CallbackRouter } from "./callback-router.js";
import type {
  ProviderFunction,
  ToolDefinitionJson,
  ToolResultJson,
  ToolRunFunction,
} from "./callback-types.js";
export type * from "./callback-types.js";
import type {
  NativeMcpClientConfig,
  NativeMcpConnection,
  NativeMcpServerInfo,
  NativeMcpServerOptions,
  NativeMcpServerStatus,
} from "./mcp.js";
export * from "./mcp.js";
import {
  NativeSessionManagement,
  waitFor,
  type NativeCallOptions,
} from "./management.js";
export {
  NativeRunHandle,
  pushRunProviderEvent,
  endRunProvider,
  isRunCallbackActive,
  reserveRunProviderRequest,
  resolveCall,
} from "./loader.js";
export type { RawRunHandle } from "./loader.js";
export type {
  NativeCallOptions,
  NativeModelInput,
  NativeContextOptions,
  NativeConfiguration,
  StandingApprovalGrant,
} from "./management.js";

export const bindingId: string = "agentkit-native";

/** A stable native failure. `code` is one of the codes in
 * `rust/crates/napi/error-codes.json` — the single shared contract. */
export class NativeAgentError extends Error {
  readonly code: string;
  readonly details?: unknown;

  constructor(code: string, message: string, details?: unknown) {
    super(message);
    this.name = "NativeAgentError";
    this.code = code;
    if (details !== undefined) this.details = details;
  }
}

/** Rust throws `Error`s whose message is the JSON payload
 * `{code, message, details?}`. Decode it; anything unparseable is honestly
 * reported as `internal_error` with the raw text, never as a guess. */
function mapError(error: unknown): NativeAgentError {
  const raw = error instanceof Error ? error.message : String(error);
  try {
    const parsed = JSON.parse(raw) as {
      code?: unknown;
      message?: unknown;
      details?: unknown;
    };
    if (typeof parsed.code === "string" && typeof parsed.message === "string") {
      return new NativeAgentError(parsed.code, parsed.message, parsed.details);
    }
  } catch {
    // Fall through to the honest default below.
  }
  return new NativeAgentError("internal_error", raw);
}

async function lift<T>(run: () => Promise<T>): Promise<T> {
  try {
    return await run();
  } catch (error) {
    throw mapError(error);
  }
}

// ---------------------------------------------------------------------------
// Callback routing
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

/** Construction-time service configuration. Every field selects NATIVE
 * behavior — plan-mode blocking, per-call approval, and the plan-approval
 * escalation all run on the Rust coding-pack authority, never in JS. */
export interface NativeServiceOptions {
  /** How long `close()` waits for owned execution to drain before it
   * completes and reports `closeUncertain`. Default 5000. */
  closeTimeoutMs?: number;
  jsToolAuthority?: {
    /** Mode applied when a session has no durable execution state yet.
     * Default `ask` (every JS write call needs one human approval). */
    configuredMode?: "read_only" | "ask" | "auto_edit" | "full_auto" | "plan";
    /** Tools whose approvable refusal is a PLAN approval. Default
     * `["submit_plan"]`. */
    planToolNames?: string[];
    /** The mode an approved plan durably grants. Never `plan`. Default
     * `ask`. */
    planGrantMode?: "read_only" | "ask" | "auto_edit" | "full_auto";
  };
  /** Selects the Rust-native provider lane: provider HTTP/SSE executes in
   * Rust over the connection registered by `registerProviderConnection`.
   * The API key is NOT part of this object — only the connection id is, so
   * the options may be logged or persisted freely. When absent, the JS
   * `setProvider` callback slot serves turns exactly as before. */
  providerConfig?: NativeProviderConfig;
}

/** Construction-time selection of the Rust provider lane (mirrors
 * `providerConfig` in `rust/crates/napi/src/service.rs`). */
export interface NativeProviderConfig {
  protocol: "chat-completions" | "responses";
  providerId: string;
  connectionId: string;
  model: string;
  temperature?: number;
  maxTokens?: number;
}

/** Options for `NativeSessionService.startHttpServer` (mirrors
 * `startHttpServer` in `rust/crates/napi/src/http_server.rs`). The server
 * is the Rust REST v1 + SSE engine serving this service's own host and
 * store. Only `bearerToken` is required; the backend itself refuses tokens
 * shorter than 32 bytes, and the binding rejects them up front. */
export interface NativeHttpServerOptions {
  /** Interface to bind. Default `127.0.0.1`. */
  host?: string;
  /** Port to bind; `0` (the default) asks the OS for an ephemeral port. */
  port?: number;
  /** Optional URL prefix; routes then live under `{basePath}/v1/...`. */
  basePath?: string;
  /** Request body cap in bytes. Default 1048576 (1 MiB). */
  maxBodyBytes?: number;
  /** Query-string cap in bytes. Default 16384. */
  maxQueryBytes?: number;
  /** Durable desktop actor identity that owns the store. Default
   * `desktop-user`; the first start binds it to the store permanently. */
  actorId?: string;
  /** Model label recorded on REST-submitted turns when neither the
   * request nor the stored settings name one. Default `model`. */
  defaultModel?: string;
  /** REQUIRED shared secret; clients must present it as
   * `Authorization: Bearer <token>`. At least 32 bytes. */
  bearerToken: string;
}

/** Where a started desktop server actually listens (an ephemeral port is
 * resolved by the OS; `baseUrl` already includes the normalized base
 * path). */
export interface NativeHttpServerInfo {
  port: number;
  baseUrl: string;
}

export class NativeStore {
  private readonly raw: RawNativeStore;
  private closed = false;

  private constructor(raw: RawNativeStore) {
    this.raw = raw;
  }

  static async open(path: string): Promise<NativeStore> {
    const { NativeStore: RawNativeStore } = await loadBinding();
    return new NativeStore(
      await lift(() => Promise.resolve(RawNativeStore.open(path))),
    );
  }

  static async inMemory(): Promise<NativeStore> {
    const { NativeStore: RawNativeStore } = await loadBinding();
    return new NativeStore(
      await lift(() => Promise.resolve(RawNativeStore.memory())),
    );
  }

  /** Internal compatibility port. Unknown operations fail before effects. */
  async callStore<T>(operation: string, input: unknown = {}): Promise<T> {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    return JSON.parse(
      await lift(() => this.raw.callStore(operation, JSON.stringify(input))),
    ) as T;
  }

  async transaction<T>(fn: (view: NativeStorePort) => Promise<T>): Promise<T> {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    const transaction = await lift(() => this.raw.beginTransaction());
    return withNativeTransaction(transaction, fn, lift);
  }

  setTransactionGateTimeout(milliseconds: number): void {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    try {
      this.raw.setTransactionGateTimeout(milliseconds);
    } catch (error) {
      throw mapError(error);
    }
  }

  /** Create a session service over this store with one JS provider slot
   * and the JS tool registry. Options select Rust-side behavior (shutdown
   * deadline, JS-tool authority); unsupported values fail with
   * `invalid_input` — the binding never advertises a policy it cannot
   * enforce. */
  async createService(
    options?: NativeServiceOptions,
  ): Promise<NativeSessionService> {
    if (this.closed) {
      throw new NativeAgentError("closed", "the native store is closed");
    }
    const router = new CallbackRouter();
    const raw = await lift(() =>
      this.raw.createService(
        (err: Error | null, payload: string) => {
          // Callee-handled threadsafe function: `err` carries a delivery
          // failure; the pending call still gets reaped by Rust shutdown.
          if (err !== null) return;
          void router.handle(payload);
        },
        options === undefined ? null : JSON.stringify(options),
      ),
    );
    return new NativeSessionService(raw, router, this, options?.providerConfig);
  }

  /**
   * Connect one MCP server (Rust `agentkit-mcp` client manager hosted by
   * this store handle). `config` is the crate's `ServerConfig` shape —
   * stdio `{kind:"stdio", command, args?, env?}` or
   * `{kind:"http", url, headers?}`; see `NativeMcpClientConfig` in
   * `mcp.ts` for every field. Effects are HOST-controlled: every returned
   * descriptor is `effect:"write"` unless the host config sets
   * `readOnly:true`. Register the descriptors through `registerTool` (or
   * `registerMcpTools`) so the authority chain keeps judging them;
   * `mcpCallTool(alias, descriptor.remoteName, args)` is the host call for
   * the tool lane. Failures carry the `mcp_*` codes from the error
   * contract; `secretRefs` entries fail with `mcp_secret_missing` (no
   * resolver in this build). Configs are in-memory only.
   */
  async mcpConnect(
    config: NativeMcpClientConfig,
  ): Promise<NativeMcpConnection> {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    const text = await lift(() =>
      this.raw.mcpConnect(JSON.stringify(config)),
    );
    return JSON.parse(text) as NativeMcpConnection;
  }

  /**
   * Invoke one tool on a connected MCP server. `toolName` accepts the
   * remote's original name (`descriptor.remoteName`), a configured
   * `toolAliases` target, or the full descriptor name. Arguments are
   * validated LOCALLY against the descriptor's recorded input schema
   * before dispatch — invalid arguments never reach the server
   * (`mcp_invalid_arguments`). Returns the host tool-result contract.
   * Call it from the `registerTool` run function (see
   * `registerMcpTools`) so the durable guard and authority layers still
   * judge the call; direct use from host code is a host decision, not a
   * bypass of the engine lane.
   *
   * `handle` is the descriptor's registration handle
   * (`descriptor.handle`, e.g. `"fix#2"`). Pass it: once an alias has been
   * re-registered, a call whose handle is missing or comes from a
   * superseded registration is refused with `mcp_authority_revoked`
   * before the transport is touched, so a stale descriptor can never
   * execute the replacement server's callback.
   */
  async mcpCallTool(
    alias: string,
    toolName: string,
    args: unknown,
    handle?: string,
  ): Promise<ToolResultJson> {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    const text = await lift(() =>
      this.raw.mcpCallTool(alias, toolName, JSON.stringify(args ?? {}), handle),
    );
    return JSON.parse(text) as ToolResultJson;
  }

  /** Disconnect one MCP server: handles revoked immediately, transport
   * closed (stdio children killed), declaration dropped. Idempotent. */
  async mcpDisconnect(alias: string): Promise<void> {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    await lift(() => this.raw.mcpDisconnect(alias));
  }

  /** Every MCP server declared on this store, with the crate manager's
   * live connected flag. */
  async mcpListServers(): Promise<NativeMcpServerStatus[]> {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    const text = await lift(() => this.raw.mcpListServers());
    const parsed = JSON.parse(text) as { servers: NativeMcpServerStatus[] };
    return parsed.servers;
  }

  /** Flush and release file ownership. Idempotent. Connected MCP
   * servers are torn down (handles revoked, transports closed) before
   * the store is released. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await lift(() => this.raw.close());
  }
}

/**
 * The same compatibility port `NativeStore` exposes, wrapped directly around
 * a raw binding handle (same JSON in/out and stable-error contract, no
 * closed-flag bookkeeping). `NativeAssistantStore` uses it for `{ raw }`
 * construction and for nothing else.
 */
export function createNativeStorePort(raw: RawNativeStore): NativeStorePort {
  return {
    async callStore<T>(operation: string, input: unknown = {}): Promise<T> {
      return JSON.parse(
        await lift(() => raw.callStore(operation, JSON.stringify(input))),
      ) as T;
    },
    async transaction<T>(
      fn: (view: NativeStorePort) => Promise<T>,
    ): Promise<T> {
      const transaction = await lift(() => raw.beginTransaction());
      return withNativeTransaction(transaction, fn, lift);
    },
  };
}

export { NativeAssistantStore } from "./assistant-store.js";
export type {
  NativeAssistantStoreOptions,
  NativeAssistantStoreClock,
} from "./assistant-store.js";

// ---------------------------------------------------------------------------
// Session service
// ---------------------------------------------------------------------------

export interface SendInputJson {
  content: unknown;
  inputId?: string;
  whenBusy?: "reject" | "queue" | "steer";
  turnId?: string;
  onSettled?: "followUp" | "cancel";
}

export interface CreateSessionInputJson {
  sessionId?: string;
  title?: string;
  profileId: string;
  profileDigest?: string;
  providerId?: string;
  model?: string;
  workspace?: unknown;
  context?: unknown;
  skills?: unknown[];
  turnKind?: string;
  executionMode?: string;
  metadata?: Record<string, unknown>;
}

export interface AnswerInputJson {
  requestId: string;
  answerId: string;
  response: unknown;
}

export type SendResultJson =
  | {
      kind: "turn";
      turnId: string;
      userMessageId: string;
      assistantMessageId: string;
    }
  | { kind: "queued"; input: Record<string, unknown> };

/** One turn subscription: pull committed events until `done`. */
export class TurnSubscription {
  private readonly raw: RawSubscriptionHandle;
  constructor(raw: RawSubscriptionHandle) {
    this.raw = raw;
  }

  async poll(
    limit = 64,
    waitMs = 25,
  ): Promise<{ events: unknown[]; done: boolean }> {
    const text = await lift(() => this.raw.poll(limit, waitMs));
    return JSON.parse(text) as { events: unknown[]; done: boolean };
  }

  cancel(): void {
    this.raw.cancel();
  }

  get done(): boolean {
    return this.raw.done;
  }

  /** Iterate committed events. Stopping the iterator (break/return) only
   * stops delivery; the durable turn keeps running (GC never cancels work
   * — same rule as the Rust executor ownership). */
  async *[Symbol.asyncIterator](): AsyncIterator<unknown> {
    while (true) {
      const batch = await this.poll(64, 250);
      for (const event of batch.events) yield event;
      if (batch.done) return;
    }
  }
}

export class NativeSessionService extends NativeSessionManagement {
  private readonly router: CallbackRouter;
  /** Kept alive while the service exists: the store clone the worker and
   * session service run on must not be collected under the service. */
  readonly store: NativeStore;
  /** The Rust provider lane this service was built with, if any. Kept only
   * so `createSession` can stamp the lane's providerId/model — the Rust
   * executor enforces task↔provider binding, so a session on the Rust lane
   * must carry the configured provider id. */
  private readonly providerConfig?: NativeProviderConfig;
  private closing: Promise<void> | null = null;

  constructor(
    raw: RawSessionService,
    router: CallbackRouter,
    store: NativeStore,
    providerConfig?: NativeProviderConfig,
  ) {
    super(raw, lift);
    this.router = router;
    this.store = store;
    this.providerConfig = providerConfig;
  }

  /** Register a provider connection for the Rust provider lane: the base
   * URL of an OpenAI-compatible server plus its API key (presented as
   * `Authorization: Bearer`). The key crosses into Rust here and never
   * again — `createService` receives only the returned connection id. */
  static async registerProviderConnection(
    baseUrl: string,
    apiKey?: string | null,
  ): Promise<string> {
    return lift(async () =>
      (await loadBinding()).registerProviderConnection(baseUrl, apiKey ?? null),
    );
  }

  /** Revoke a registered connection. Returns false for an unknown or
   * already-revoked id; in-flight runs on that lane fail their next
   * provider request instead of firing with a retired key. */
  static async revokeProviderConnection(
    connectionId: string,
  ): Promise<boolean> {
    return lift(async () =>
      (await loadBinding()).revokeProviderConnection(connectionId),
    );
  }

  async createSession(
    input: CreateSessionInputJson,
    options?: NativeCallOptions,
  ): Promise<Record<string, unknown>> {
    // Marshaling only: on the Rust provider lane the executor refuses tasks
    // whose providerId differs from the installed provider, so a session
    // created without one inherits the lane's provider and model. An
    // explicit caller value is never overridden.
    const marshaled = this.providerConfig
      ? {
          ...input,
          providerId: input.providerId ?? this.providerConfig.providerId,
          model: input.model ?? this.providerConfig.model,
        }
      : input;
    const text = await waitFor(
      () => lift(() => this.raw.createSession(JSON.stringify(marshaled))),
      options,
    );
    return JSON.parse(text) as Record<string, unknown>;
  }

  setProvider(provider: ProviderFunction | null): void {
    if (this.closing !== null)
      throw new NativeAgentError("closed", "the session service is closed");
    this.router.setProvider(provider);
  }

  async registerTool(
    definition: ToolDefinitionJson,
    run: ToolRunFunction,
  ): Promise<void> {
    await lift(() => this.raw.registerTool(JSON.stringify(definition)));
    this.router.registerTool(definition, run);
  }

  async send(
    sessionId: string,
    input: SendInputJson,
    options?: NativeCallOptions,
  ): Promise<SendResultJson> {
    const text = await waitFor(
      () => lift(() => this.raw.send(sessionId, JSON.stringify(input))),
      options,
    );
    return JSON.parse(text) as SendResultJson;
  }

  async followUp(
    sessionId: string,
    input: SendInputJson,
    options?: NativeCallOptions,
  ): Promise<SendResultJson> {
    const text = await waitFor(
      () => lift(() => this.raw.followUp(sessionId, JSON.stringify(input))),
      options,
    );
    return JSON.parse(text) as SendResultJson;
  }

  async steer(
    sessionId: string,
    input: SendInputJson,
    options?: NativeCallOptions,
  ): Promise<SendResultJson> {
    const text = await waitFor(
      () => lift(() => this.raw.steer(sessionId, JSON.stringify(input))),
      options,
    );
    return JSON.parse(text) as SendResultJson;
  }

  async getState(sessionId: string): Promise<Record<string, unknown>> {
    const text = await lift(() => this.raw.getState(sessionId));
    return JSON.parse(text) as Record<string, unknown>;
  }

  async getExecution(
    sessionId: string,
  ): Promise<Record<string, unknown> | null> {
    const text = await lift(() => this.raw.getExecution(sessionId));
    return text === null || text === undefined
      ? null
      : (JSON.parse(text) as Record<string, unknown>);
  }

  async listTurns(sessionId: string): Promise<unknown[]> {
    const text = await lift(() => this.raw.listTurns(sessionId));
    return JSON.parse(text) as unknown[];
  }

  /** The full durable projection of one session (turns, messages, inputs). */
  async exportSession(sessionId: string): Promise<Record<string, unknown>> {
    const text = await lift(() => this.raw.exportSession(sessionId));
    return JSON.parse(text) as Record<string, unknown>;
  }

  async getUsage(sessionId: string): Promise<Record<string, unknown>> {
    const text = await lift(() => this.raw.getUsage(sessionId));
    return JSON.parse(text) as Record<string, unknown>;
  }

  async answer(input: AnswerInputJson): Promise<Record<string, unknown>> {
    const text = await lift(() => this.raw.answer(JSON.stringify(input)));
    return JSON.parse(text) as Record<string, unknown>;
  }

  async approve(
    requestId: string,
    answerId: string,
    note?: string,
    remember?: boolean,
  ): Promise<Record<string, unknown>> {
    const text = await lift(() =>
      this.raw.approve(requestId, answerId, note ?? null, remember ?? null),
    );
    return JSON.parse(text) as Record<string, unknown>;
  }

  async deny(
    requestId: string,
    answerId: string,
    note?: string,
  ): Promise<Record<string, unknown>> {
    const text = await lift(() =>
      this.raw.deny(requestId, answerId, note ?? null),
    );
    return JSON.parse(text) as Record<string, unknown>;
  }

  async cancel(sessionId: string, turnId: string): Promise<void> {
    await lift(() => this.raw.cancel(sessionId, turnId));
  }

  async interrupt(sessionId: string, turnId: string): Promise<void> {
    await lift(() => this.raw.interrupt(sessionId, turnId));
  }

  async resume(sessionId: string, turnId: string): Promise<void> {
    await lift(() => this.raw.resume(sessionId, turnId));
  }

  async flushQueue(sessionId: string): Promise<SendResultJson | null> {
    const text = await lift(() => this.raw.flushQueue(sessionId));
    return text === null || text === undefined
      ? null
      : (JSON.parse(text) as SendResultJson);
  }

  /**
   * Replay-and-live pump over the turn's durable event log.
   *
   * `afterSeq` is an exclusive cursor ("deliver events AFTER this sequence"),
   * and sequences start at 0 — pass `undefined` (or omit) to receive every
   * event from the first. Passing `0` skips the seq-0 event, which is
   * whatever the executor emitted first (for a session turn, a leading
   * `run.warning`).
   */
  async subscribeTurn(
    taskId: string,
    afterSeq?: number,
  ): Promise<TurnSubscription> {
    const raw = await lift(() =>
      this.raw.subscribeTurn(taskId, afterSeq ?? null),
    );
    return new TurnSubscription(raw);
  }

  /**
   * Start the Rust REST v1 + SSE desktop server over THIS service's host
   * and store: a message submitted via `POST /v1/chats/:chatId/messages`
   * is durable work executed by this service's own worker, exactly like a
   * `send()` from JS. The bearer token is checked by the Rust backend
   * (constant-time); every non-public route refuses a missing or wrong
   * token with a `problem+json` 401. Invalid options fail with
   * `invalid_input`; an address that cannot be bound fails with
   * `service_unavailable`. Fails if a server already runs on this service
   * (call `stopHttpServer()` first) or after `close()`.
   */
  async startHttpServer(
    options: NativeHttpServerOptions,
  ): Promise<NativeHttpServerInfo> {
    const text = await lift(() =>
      this.raw.startHttpServer(JSON.stringify(options)),
    );
    return JSON.parse(text) as NativeHttpServerInfo;
  }

  /** Stop the desktop server started by `startHttpServer`: the listener
   * closes immediately (follow-up requests are refused) and in-flight
   * requests drain up to the service's close deadline. Idempotent —
   * stopping a never-started, already-stopped, or closed service is a
   * no-op. `close()` stops a live server itself, before the runtime
   * begins Closing. */
  async stopHttpServer(): Promise<void> {
    await lift(() => this.raw.stopHttpServer());
  }

  /**
   * Start the Rust MCP server (streamable HTTP, official `rmcp` SDK) on
   * THIS service's JS tool lane. The catalogue a remote client sees is
   * exactly this service's registered JS tools: `tools/list` and
   * `tools/call` hide write-effect tools while `writesEnabled` is false
   * (the default), and a call executes the same `JsTool::execute` bridge
   * request a local tool call takes. Every request must present
   * `Authorization: Bearer <token>` for one of the REQUIRED
   * `bearerTokens` (≥ 32 bytes each); invalid options fail with
   * `invalid_input`, an unusable address with `service_unavailable`.
   * Remote calls run WITHOUT the run-scoped plan-mode guard and durable
   * execution receipts (see the module docs in
   * `rust/crates/napi/src/mcp_server.rs` for the exact scope), so treat
   * `writesEnabled: true` as granting real write authority. Fails if an
   * MCP server already runs on this service (call `stopMcpServer()`
   * first) or after `close()`.
   */
  async startMcpServer(
    options: NativeMcpServerOptions,
  ): Promise<NativeMcpServerInfo> {
    const text = await lift(() =>
      this.raw.startMcpServer(JSON.stringify(options)),
    );
    return JSON.parse(text) as NativeMcpServerInfo;
  }

  /** Stop the MCP server: session leases are revoked first (in-flight JS
   * tool callbacks are cancelled), the listener closes immediately, and
   * the accept loop drains up to the service's close deadline.
   * Idempotent. `close()` stops a live server itself, before the runtime
   * begins Closing. */
  async stopMcpServer(): Promise<void> {
    await lift(() => this.raw.stopMcpServer());
  }

  /** `Open → Closing → Closed` on the Rust side: no new callbacks are
   * dispatched, outstanding ones settle honestly, and the worker drain is
   * awaited up to `closeTimeoutMs`. Idempotent, and concurrent callers all
   * await the same completion. Closing releases the store clone so the
   * store can then be closed.
   *
   * SCOPE: `close()` stops the RUNTIME. It does not promise that external
   * effects started by an already-dispatched JS callback have stopped —
   * those continue outside the runtime, which only guarantees it never
   * fabricates their outcome (the durable receipt stays admitted-without-
   * result). `pendingCallbacks` is the observability for that gap; do not
   * treat `close()` alone as quiescing external effects. */
  async close(): Promise<void> {
    const started = this.closing;
    if (started) return started;
    const run = (async () => {
      try {
        // Release the event-loop reference first: a closed service must never
        // keep the process alive, even if the class object is still referenced.
        try {
          this.raw.detach();
        } catch {
          // Detach is best-effort; the async close below is authoritative.
        }
        await lift(() => this.raw.close());
      } finally {
        this.router.close();
      }
    })();
    this.closing = run;
    return run;
  }

  /** True when the last `close()` completed without establishing that
   * owned execution had drained. Durable state stays authoritative; this
   * only records what close could not prove. It describes WORKER DRAIN
   * only — never quiescence of external JS effects. */
  get closeUncertain(): boolean {
    return this.raw.closeUncertain === true;
  }

  /** Dispatched JS callbacks whose settlement has not arrived. After
   * `close()` this counts callbacks already handed to JS whose promises
   * are still unsettled — the observable count of external effects the
   * runtime cannot promise stopped. Zero means every dispatched call
   * settled (or was never dispatched). Counts USER-code windows: a call
   * still queued in the bridge but not yet entered is not an external
   * effect (Closing rejects it without running it). */
  get pendingCallbacks(): number {
    return this.router.pendingEffects;
  }
}
