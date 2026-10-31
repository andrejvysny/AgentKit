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
  ToolRunFunction,
} from "./callback-types.js";
export type * from "./callback-types.js";
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

export class NativeSessionService extends NativeSessionManagement {
  private readonly router: CallbackRouter;
  /** Kept alive while the service exists: the store clone the worker and
   * session service run on must not be collected under the service. */
  readonly store: NativeStore;
  private closing: Promise<void> | null = null;

  constructor(
    raw: RawSessionService,
    router: CallbackRouter,
    store: NativeStore,
  ) {
    super(raw, lift);
    this.router = router;
    this.store = store;
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

  async createSession(
    input: CreateSessionInputJson,
    options?: NativeCallOptions,
  ): Promise<Record<string, unknown>> {
    const text = await waitFor(
      () => lift(() => this.raw.createSession(JSON.stringify(input))),
      options,
    );
    return JSON.parse(text) as Record<string, unknown>;
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

  async subscribeTurn(
    taskId: string,
    afterSeq?: number,
  ): Promise<TurnSubscription> {
    const raw = await lift(() =>
      this.raw.subscribeTurn(taskId, afterSeq ?? null),
    );
    return new TurnSubscription(raw);
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
