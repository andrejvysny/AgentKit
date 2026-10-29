declare type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };
/**
 * Which binding artifact the generated loader actually loaded: `'native'` for
 * a native addon, otherwise the `platformArchABI` of the WASI flavor. Every
 * flavor napi-rs can build is listed, because `NAPI_RS_NATIVE_LIBRARY_PATH`
 * can point the loader at a WASI artifact this package does not build itself.
 */
export declare const __napiBindingTarget: 'native' | 'wasm32-wasi' | 'wasm32-wasip1'

export declare class NativeStore {
  /**
   * Open (creating if necessary) a durable store file. A second open of
   * the same path while it is owned is rejected with `already_owned`.
   */
  static open(path: string): NativeStore
  /** Open an in-memory store (tests, throwaway sessions). */
  static memory(): NativeStore
  /** Flush all pending writes durably and release ownership. Idempotent. */
  close(): Promise<void>
  /**
   * Create the session service over this store: host + chat executor +
   * durable session service + background worker, with crash recovery run
   * before the worker claims any work. `onEvent` receives every
   * provider/tool callback as a JSON envelope. `optionsJson` configures
   * the shutdown deadline and the JS-tool Rust authority (see
   * `SessionService::build`); an unsupported value fails the call.
   */
  createService(onEvent: ((err: Error | null, arg: string) => any), optionsJson?: string | undefined | null): Promise<SessionService>
}

export declare class SessionService {
  /**
   * Register a JS-executed tool. `definitionJson` is the portable tool
   * definition: `{name, version?, effect?, capability?, description?,
   * inputSchema, outputSchema?, timeoutMs?}`.
   */
  registerTool(definitionJson: string): Promise<void>
  createSession(inputJson: string): Promise<string>
  send(sessionId: string, inputJson: string): Promise<string>
  followUp(sessionId: string, inputJson: string): Promise<string>
  steer(sessionId: string, inputJson: string): Promise<string>
  getState(sessionId: string): Promise<string>
  getExecution(sessionId: string): Promise<string | null>
  listTurns(sessionId: string): Promise<string>
  /**
   * The durable projection of one session (turns, messages, inputs,
   * descriptor and execution metadata) — the reopened-history view.
   * Private tool execution receipt rows are not included.
   */
  exportSession(sessionId: string): Promise<string>
  getUsage(sessionId: string): Promise<string>
  /**
   * Answer a pending human request. `inputJson` is
   * `{requestId, answerId, response}`.
   */
  answer(inputJson: string): Promise<string>
  approve(requestId: string, answerId: string, note?: string | undefined | null, remember?: boolean | undefined | null): Promise<string>
  deny(requestId: string, answerId: string, note?: string | undefined | null): Promise<string>
  cancel(sessionId: string, turnId: string): Promise<void>
  interrupt(sessionId: string, turnId: string): Promise<void>
  resume(sessionId: string, turnId: string): Promise<void>
  flushQueue(sessionId: string): Promise<string | null>
  /**
   * Subscribe to the durable committed event stream of one turn task.
   * Events are handed out in `seq` order; a gap is impossible by
   * construction because every poll continues from the last delivered
   * cursor and each event's `seq` is returned to the caller.
   */
  subscribeTurn(taskId: string, afterSeq?: number | undefined | null): Promise<SubscriptionHandle>
  /**
   * Release the event-loop reference held by the callback delivery
   * function. The JS wrapper calls this first in `close()`; after it, no
   * new callback can wake the loop, so a process whose script has ended
   * can actually exit even while the class object is still referenced.
   */
  detach(): void
  /**
   * Shutdown: `Open → Closing → Closed` in one atomic sequence.
   * Entering Closing rejects outstanding callbacks AND new JS dispatches
   * under the same lock (no insert-after-drain window), then the service
   * stops taking operations, cancels subscriptions, and awaits the
   * worker. Active runs observe the closing bridge, fail their next
   * callback honestly, and settle durably — close never fakes success,
   * and rejecting a JS promise never claims to stop external effects the
   * callback already started. If the worker does not finish draining
   * within `closeTimeoutMs`, close still completes and `closeUncertain`
   * reports that cleanup could not be established. Idempotent;
   * concurrent callers await the same completion.
   *
   * SHUTDOWN SCOPE — what `close()` does and does not promise:
   * * It stops the runtime: no new dispatch, no new runs, worker drained
   *   (or `closeUncertain` records the unproven drain).
   * * It does NOT quiesce external effects a dispatched JS callback
   *   already started. A callback handed to JS before Closing may
   *   continue acting after `close()` resolves; the runtime only
   *   guarantees it never fabricates that effect's outcome — the durable
   *   receipt stays admitted-without-result and recovery treats it as
   *   uncertain. Use the JS wrapper's `pendingCallbacks` to observe
   *   dispatched, unsettled callbacks before relying on any stronger
   *   shutdown claim.
   */
  close(): Promise<void>
  /**
   * True when a `close()` completed without establishing that owned
   * execution had drained (the worker deadline expired). Durable state
   * stays authoritative; this flag records what close could not prove.
   * It describes WORKER DRAIN only — it never claims external JS effects
   * stopped; the JS wrapper's `pendingCallbacks` is the observability
   * for dispatched, unsettled callbacks.
   */
  get closeUncertain(): boolean
}

/**
 * One turn subscription. `poll` drains the next committed events; the
 * stream is pull-based, so a slow consumer applies backpressure to the
 * follower instead of overflowing a queue.
 */
export declare class SubscriptionHandle {
  /**
   * Drain up to `limit` (max 64) committed events, waiting up to
   * `waitMs` for at least one when idle. `done` means the turn settled
   * (or failed/closed) and no further events will arrive.
   */
  poll(limit?: number | undefined | null, waitMs?: number | undefined | null): Promise<string>
  /**
   * Stop the subscription. Idempotent; polling afterwards reports
   * `done` with no events.
   */
  cancel(): void
  get done(): boolean
}

/**
 * Load-time identity probe: proves the addon, its runtime, and the error
 * contract are all present before any session call is attempted.
 */
export declare function bindingSmoke(): string

/**
 * The JS wrapper settles exactly one in-flight provider/tool callback with
 * either `{ payload }` (the JSON result) or `{ error }` (a rejection
 * message). Returns false for unknown or already-settled ids: a stale
 * resolve is ignored, never an exception, and never a double-settle.
 */
export declare function resolveCall(callId: string, payload?: string | undefined | null, error?: string | undefined | null): boolean
