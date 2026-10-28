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
  NativeStore as RawNativeStore,
  bindingSmoke,
  resolveCall,
} from "../generated/index.js";
import type {
  SessionService as RawSessionService,
  SubscriptionHandle as RawSubscriptionHandle,
} from "../generated/index.js";

// The smoke probe validates the embedded error-code contract at load.
const SMOKE = (() => {
  try {
    return bindingSmoke();
  } catch (error) {
    throw new Error(
      `the native binding failed to load: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
})();

export const bindingId: string = SMOKE;

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

/** One model request: the JS provider returns the final assistant result. */
export interface ProviderRequest {
  runId: string;
  model: string;
  messages: unknown[];
  tools: unknown[];
  temperature?: number | null;
  maxOutputTokens?: number | null;
}

export interface ProviderToolCall {
  id?: string;
  name: string;
  arguments?: unknown;
}

export interface ProviderResult {
  content?: string;
  toolCalls?: ProviderToolCall[];
  finishReason?: string;
  usage?: Record<string, unknown> | null;
}

export type ProviderFunction = (
  request: ProviderRequest,
) => Promise<ProviderResult> | ProviderResult;

/** One tool execution: the JS tool returns the host tool-result shape. */
export interface ToolCallRequest {
  name: string;
  toolCallId: string | null;
  attemptId: string | null;
  runId: string;
  input: unknown;
}

export interface ToolResultJson {
  ok?: boolean;
  data?: unknown;
  sources?: unknown[];
  warnings?: string[];
  truncated?: boolean;
  limits?: { profile: string; maxBytes: number; maxItems?: number };
  modelData?: unknown;
  summary?: string;
  status?: string;
}

export interface ToolDefinitionJson {
  name: string;
  version?: string;
  effect?: "read" | "write";
  capability?: string;
  description?: string;
  inputSchema: Record<string, unknown>;
  outputSchema?: Record<string, unknown>;
  timeoutMs?: number;
}

export type ToolRunFunction = (
  input: unknown,
  request: ToolCallRequest,
) => Promise<ToolResultJson | unknown> | ToolResultJson | unknown;

interface RegisteredTool {
  definition: ToolDefinitionJson;
  run: ToolRunFunction;
}

/**
 * The single JS entry point for every Rust-originated callback. One
 * callback = one in-flight call; the wrapper resolves it exactly once. A
 * user function that throws (or rejects) settles the call as a failure —
 * never as a success, never as a dangling entry.
 */
class CallbackRouter {
  private provider: ProviderFunction | null = null;
  private tools = new Map<string, RegisteredTool>();
  private closed = false;

  setProvider(provider: ProviderFunction | null): void {
    this.provider = provider;
  }

  registerTool(definition: ToolDefinitionJson, run: ToolRunFunction): void {
    this.tools.set(definition.name, { definition, run });
  }

  close(): void {
    this.closed = true;
    this.provider = null;
    this.tools.clear();
  }

  /** Invoked from Rust with one JSON envelope per callback. Must never
   * throw: an escaping exception would strand the Rust pending entry. */
  async handle(raw: string): Promise<void> {
    let envelope: { kind?: unknown; callId?: unknown; request?: unknown };
    try {
      envelope = JSON.parse(raw);
    } catch {
      return; // Nothing addressable; Rust-side shutdown still reaps it.
    }
    const callId = typeof envelope.callId === "string" ? envelope.callId : null;
    if (callId === null) return;
    if (this.closed) {
      resolveCall(callId, null, "the binding closed before the callback ran");
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
        const result = await provider(envelope.request as ProviderRequest);
        resolveCall(callId, JSON.stringify(result ?? {}), null);
        return;
      }
      if (envelope.kind === "tool") {
        const request = envelope.request as ToolCallRequest;
        const tool = this.tools.get(request?.name);
        if (tool === undefined) {
          resolveCall(callId, null, `no JS tool named ${request?.name}`);
          return;
        }
        const outcome = await tool.run(request.input, request);
        resolveCall(callId, JSON.stringify(outcome ?? {}), null);
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
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

export class NativeStore {
  private readonly raw: RawNativeStore;
  private closed = false;

  private constructor(raw: RawNativeStore) {
    this.raw = raw;
  }

  static async open(path: string): Promise<NativeStore> {
    return new NativeStore(
      await lift(() => Promise.resolve(RawNativeStore.open(path))),
    );
  }

  static async inMemory(): Promise<NativeStore> {
    return new NativeStore(
      await lift(() => Promise.resolve(RawNativeStore.memory())),
    );
  }

  /** Create a session service over this store with one JS provider slot
   * and the JS tool registry. */
  async createService(): Promise<NativeSessionService> {
    if (this.closed) {
      throw new NativeAgentError("closed", "the native store is closed");
    }
    const router = new CallbackRouter();
    const raw = await lift(() =>
      this.raw.createService((err: Error | null, payload: string) => {
        // Callee-handled threadsafe function: `err` carries a delivery
        // failure; the pending call still gets reaped by Rust shutdown.
        if (err !== null) return;
        void router.handle(payload);
      }),
    );
    return new NativeSessionService(raw, router, this);
  }

  /** Flush and release file ownership. Idempotent. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
    await lift(() => this.raw.close());
  }
}

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

export class NativeSessionService {
  private readonly raw: RawSessionService;
  private readonly router: CallbackRouter;
  /** Kept alive while the service exists: the store clone the worker and
   * session service run on must not be collected under the service. */
  readonly store: NativeStore;
  private closed = false;

  constructor(
    raw: RawSessionService,
    router: CallbackRouter,
    store: NativeStore,
  ) {
    this.raw = raw;
    this.router = router;
    this.store = store;
  }

  setProvider(provider: ProviderFunction | null): void {
    this.router.setProvider(provider);
  }

  async registerTool(
    definition: ToolDefinitionJson,
    run: ToolRunFunction,
  ): Promise<void> {
    await lift(() => this.raw.registerTool(JSON.stringify(definition)));
    this.router.registerTool(definition, run);
  }

  async createSession(
    input: CreateSessionInputJson,
  ): Promise<Record<string, unknown>> {
    const text = await lift(() =>
      this.raw.createSession(JSON.stringify(input)),
    );
    return JSON.parse(text) as Record<string, unknown>;
  }

  async send(sessionId: string, input: SendInputJson): Promise<SendResultJson> {
    const text = await lift(() =>
      this.raw.send(sessionId, JSON.stringify(input)),
    );
    return JSON.parse(text) as SendResultJson;
  }

  async followUp(
    sessionId: string,
    input: SendInputJson,
  ): Promise<SendResultJson> {
    const text = await lift(() =>
      this.raw.followUp(sessionId, JSON.stringify(input)),
    );
    return JSON.parse(text) as SendResultJson;
  }

  async steer(
    sessionId: string,
    input: SendInputJson,
  ): Promise<SendResultJson> {
    const text = await lift(() =>
      this.raw.steer(sessionId, JSON.stringify(input)),
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

  async subscribeTurn(
    taskId: string,
    afterSeq?: number,
  ): Promise<TurnSubscription> {
    const raw = await lift(() =>
      this.raw.subscribeTurn(taskId, afterSeq ?? null),
    );
    return new TurnSubscription(raw);
  }

  /** Reject outstanding callbacks, stop the worker, drain store writes.
   * Idempotent. Closing the service releases the store clone so the store
   * can then be closed. */
  async close(): Promise<void> {
    if (this.closed) return;
    this.closed = true;
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
  }
}
