declare type JsonValue = string | number | boolean | null | JsonValue[] | { [key: string]: JsonValue };
/**
 * Which binding artifact the generated loader actually loaded: `'native'` for
 * a native addon, otherwise the `platformArchABI` of the WASI flavor. Every
 * flavor napi-rs can build is listed, because `NAPI_RS_NATIVE_LIBRARY_PATH`
 * can point the loader at a WASI artifact this package does not build itself.
 */
export declare const __napiBindingTarget: 'native' | 'wasm32-wasi' | 'wasm32-wasip1'

export declare class NativeRunHandle {
  static start(optionsJson: string, callback: ((err: Error | null, arg: string) => any)): Promise<NativeRunHandle>
  next(): Promise<string>
  close(): void
  failBudget(code: string, message: string, providerRequests: number): void
  cancel(): void
}

export declare class NativeStore {
  /**
   * Connect one MCP server. `configJson` is the crate's `ServerConfig`
   * shape: `{alias, transport: {kind:"stdio", command, args?, env?} |
   * {kind:"http", url, headers?}, secretRefs?, enabled?, toolAliases?,
   * readOnly?, resilience?}`. Effects are HOST-controlled: every
   * returned descriptor carries `effect:"write"` UNLESS the host config
   * sets `readOnly:true` — remote annotations never raise or lower it.
   * Returns `{alias, tools}` where `tools` are descriptors ready for
   * `NativeSessionService.registerTool`; call `mcpCallTool(alias,
   * descriptor.remoteName, args)` from the tool lane to invoke. Fails
   * with `mcp_invalid_config` for a bad declaration or remote identity,
   * `mcp_connect_failed`/`mcp_*` for transport problems, and
   * `mcp_secret_missing` if the config carries `secretRefs` (the
   * binding has no secret resolver yet). Connecting an already-known
   * alias fails with `invalid_input`; disconnect it first.
   */
  mcpConnect(configJson: string): Promise<string>
  /**
   * Invoke one tool on a connected MCP server. `toolName` accepts the
   * remote's original name, the configured alias target, or the full
   * descriptor name. `handle` is the descriptor's `handle` field, i.e. the
   * registration that minted it: it is REQUIRED once an alias has been
   * re-registered (disconnect + reconnect under the same alias mints a new
   * registration), and a call whose handle names a superseded registration
   * is refused with `mcp_authority_revoked` — a descriptor from generation
   * N never authorizes generation N+1's callback. Dispatch is additionally
   * PINNED to the transport the descriptor was listed from (crate
   * `ToolBinding`): after an automatic reconnect the stale descriptor is
   * refused with `mcp_authority_revoked` instead of executing the new
   * generation's handler, and the pin is re-checked wire-last, after any
   * reconnect, secret resolution, or backoff, immediately before the
   * request is written. The arguments are
   * validated LOCALLY against the descriptor's recorded input schema before
   * dispatch (invalid arguments never reach the server,
   * `mcp_invalid_arguments`; a schema that could not be compiled fails
   * closed the same way).
   * `cancellationKey` is the abort handle the JS tool lane mints for one
   * originating invocation: while the key is registered, `mcpCancelCall`
   * tears the in-flight call down (pre-dispatch it is never written to the
   * wire — connect, reconnect, backoff and the wire-last guard all observe
   * the token — and mid-flight it aborts like any crate cancellation,
   * which keeps its ambiguous-delivery semantics).
   * Returns the host tool-result contract as JSON text (`{ok, data,
   * modelData, summary, status, warnings, truncated, limits}`). Call
   * it from the `registerTool` run function so the durable guard and
   * authority layers keep judging the call — this host method is for
   * the JS tool lane, not a bypass around those checks.
   */
  mcpCallTool(alias: string, toolName: string, argsJson: string, handle?: string | undefined | null, cancellationKey?: string | undefined | null): Promise<string>
  /**
   * Tear down one in-flight `mcpCallTool` by the cancellation key the JS
   * lane minted: this is the binding half of the originating invocation's
   * AbortSignal. Pre-dispatch the request is never written (connect,
   * reconnect, backoff and the wire-last guard all observe the token);
   * after the request left, the call aborts like any crate cancellation.
   * Cancelling an unknown or already-settled key is a no-op success.
   */
  mcpCancelCall(cancellationKey: string): Promise<void>
  /**
   * Disconnect one MCP server: its tool handles are revoked
   * immediately, its transport closed, and its declaration dropped.
   * Idempotent — disconnecting an unknown or already-disconnected
   * alias is a no-op success.
   */
  mcpDisconnect(alias: string): Promise<void>
  /**
   * Every MCP server declared on this store: `{alias, kind, enabled,
   * readOnly, connected, tools}`. `connected` is the crate manager's
   * live-session view (a connected server whose transport dropped
   * reports false and reconnects on the next host call).
   */
  mcpListServers(): Promise<string>
  /**
   * Internal JSON port used by the compatibility facade. Rust owns validation,
   * persistence, ordering, and atomicity; unknown operations never run effects.
   */
  callStore(operation: string, inputJson: string): Promise<string>
  beginTransaction(): Promise<StoreTransactionHandle>
  setTransactionGateTimeout(milliseconds: number): void
  /**
   * Opt into claim-time priority aging. Omitted fields take the engine
   * defaults (30s interval, bonus 0 = off, uncapped).
   */
  setTaskAging(intervalMs?: number | undefined | null, bonus?: number | undefined | null, maxBonus?: number | undefined | null): void
  /**
   * Open (creating if necessary) a durable store file. A second open of
   * the same path while it is owned is rejected with `already_owned`.
   */
  static open(path: string): NativeStore
  /** Open an in-memory store (tests, throwaway sessions). */
  static memory(): NativeStore
  /**
   * Flush all pending writes durably and release ownership. Idempotent.
   * Connected MCP servers are torn down first: their handles are revoked
   * and transports closed (stdio children killed) before the store is
   * released, so a closed store never leaves a live MCP connection.
   */
  close(): Promise<void>
  /**
   * Create the session service over this store: host + chat executor +
   * durable session service + background worker, with crash recovery run
   * before the worker claims any work. `onEvent` receives every
   * provider/tool callback as a JSON envelope. `optionsJson` configures
   * the shutdown deadline, the JS-tool Rust authority, and optionally
   * the Rust-native provider lane (`providerConfig`, whose connection
   * must already be registered via `registerProviderConnection` — API
   * keys never travel in `optionsJson`); an unsupported value fails the
   * call. See `SessionService::build`.
   */
  createService(onEvent: ((err: Error | null, arg: string) => any), optionsJson?: string | undefined | null): Promise<SessionService>
}

export declare class SessionService {
  /**
   * Start the Rust REST v1 + SSE server on this service's host and
   * store. `optionsJson`: `{host?, port?, basePath?, maxBodyBytes?,
   * maxQueryBytes?, actorId?, defaultModel?, bearerToken, cors?,
   * providerSecrets?}` —
   * `bearerToken` is REQUIRED and at least 32 bytes (the backend's own
   * floor); invalid values fail with `invalid_input`, an unusable
   * address with `service_unavailable`. Defaults: `127.0.0.1`, ephemeral
   * port, no base path, 1 MiB body / 16 KiB query caps.
   * `cors` (`{origins: "*"|[string], allowHeaders?, exposeHeaders?,
   * maxAgeSeconds?}`) pins the retired TypeScript transport's CORS
   * semantics: matched origins decorate every response (including
   * problems and SSE) and OPTIONS preflights answer 204; an unmatched
   * origin preflight gets the ordinary 405, an unmatched path 404.
   * Omitted, no response ever carries a CORS header.
   * `providerSecrets: "memory"` stores provider credentials in a
   * volatile vault so `createProvider`/`updateProvider` accept
   * `apiKey`/`extraHeaders`; omitted, those writes answer 501 like the
   * TypeScript transport without a `SecretStore` port. Returns
   * `{port, baseUrl}` as JSON text (the wrapper parses it). Fails with
   * `invalid_input` if a server already runs on this service, and
   * `closed` after `close()`. Every request is authenticated by the
   * backend's bearer check; a REST-submitted turn is executed by this
   * service's worker like any other durable task.
   */
  startHttpServer(optionsJson: string): Promise<string>
  /**
   * Stop the desktop server: the listener closes immediately (follow-up
   * connections are refused), in-flight requests drain up to the
   * service's close deadline, and the accept loop is force-stopped
   * after. Idempotent — stopping a service that never started, or was
   * already stopped or closed, is a no-op success.
   */
  stopHttpServer(): Promise<void>
  setExecutionMode(sessionId: string, mode: string): Promise<string>
  listApprovalGrants(sessionId: string): Promise<string>
  revokeApprovalGrant(sessionId: string, grantId: string): Promise<string>
  compact(sessionId: string): Promise<void>
  fork(sessionId: string, fromMessageId: string): Promise<string>
  setModel(sessionId: string, inputJson: string): Promise<string>
  cancelQueuedInput(sessionId: string, inputId: string): Promise<boolean>
  clearQueue(sessionId: string): Promise<number>
  configure(sessionId: string, inputJson: string): Promise<string>
  /**
   * Delete the chat scope behind a session, straight through the store
   * engine: an unknown chat fails `not_found`, and while any task in the
   * scope is non-terminal the engine refuses atomically with `chat_busy`
   * (never a delete that silently cancels live work). The binding adds no
   * policy of its own.
   */
  deleteChat(sessionId: string): Promise<void>
  /**
   * Hide the chat from the default listing; a named pass-through over the
   * store's `archived` field, answering with the updated record JSON.
   */
  archiveChat(sessionId: string): Promise<string>
  /**
   * The inverse of `archiveChat`: un-hide the chat, answering with the
   * updated record JSON. An unknown chat fails `not_found`.
   */
  unarchiveChat(sessionId: string): Promise<string>
  /**
   * Start the Rust MCP server (streamable HTTP, official-SDK-backed)
   * on this service's JS tool lane. `optionsJson`: `{host?, port?,
   * bearerTokens, allowedHosts?, allowedOrigins?, writesEnabled?,
   * maxConcurrentCalls?, maxBatchSize?, callDeadlineMs?, idleTtlMs?,
   * maxBodyBytes?, maxSessionsPerPrincipal?}` — `bearerTokens` is a
   * REQUIRED non-empty array, every entry at least 32 bytes; invalid
   * values fail with `invalid_input`, an unusable address with
   * `service_unavailable`. Defaults: `127.0.0.1`, ephemeral port,
   * `writesEnabled: false`, the crate's frozen limit defaults. Returns
   * `{port, url}` as JSON text. The catalogue a remote client sees is
   * exactly this service's registered JS tools (list AND call hide
   * write tools while `writesEnabled` is false); a `tools/call`
   * executes the same `JsTool::execute` bridge request a local tool
   * call takes. Fails with `invalid_input` if an MCP server already
   * runs on this service, and `closed` after `close()`.
   */
  startMcpServer(optionsJson: string): Promise<string>
  /**
   * Stop the MCP server: every session lease is revoked first (cancels
   * in-flight JS tool callbacks), the listener closes immediately
   * (follow-up connections are refused), and the accept loop is
   * force-stopped after the close deadline. Idempotent — stopping a
   * service that never started, or was already stopped or closed, is a
   * no-op success. `close()` stops a live server itself, before the
   * bridge begins Closing.
   */
  stopMcpServer(): Promise<void>
  /**
   * Register a JS-executed tool. `definitionJson` is the portable tool
   * definition: `{name, version?, effect?, capability?, description?,
   * inputSchema, outputSchema?, timeoutMs?}`.
   */
  registerTool(definitionJson: string): Promise<void>
  /**
   * Register the RUST coding toolpack as a first-class contributor on
   * this service's executor lane: `read_file`, `list_dir`, `find_files`,
   * `code_search`, `repo_map`, `git_inspect`, `edit_file`, `write_file`,
   * `run_command`, `run_tests`, and `submit_plan` — the crate's own
   * descriptors, executed by Rust, governed by the crate's own
   * permission + high-risk guards over the binding's durable authority,
   * with the crate's fingerprint-v4 standing approvals for the shell
   * tools. Nothing on this path crosses into JS: no callback, no TS
   * pack, no fallback.
   *
   * `optionsJson`: `{workspaceRoot (REQUIRED, absolute), sandbox?
   * ("required"|"off", default "required"), network?, testCommands?,
   * allowGitWrites?, allowSensitive?, configuredMode?
   * ("read_only"|"ask"|"auto_edit"|"full_auto"|"plan", default "ask" —
   * the mode used until the session's durable execution state says
   * otherwise), role? ("implementer"|"reviewer"|"supervisor"),
   * planToolNames? (default ["submit_plan"]), identity? (default the
   * root; part of the standing-approval key envelope), coordinationDir?
   * (default the per-uid shared lock dir), coordinationWaitMs?}`.
   * One-shot: a second registration is refused; the descriptor set of a
   * service is immutable once the worker can claim turns.
   */
  registerCodingTools(optionsJson: string): Promise<void>
  /**
   * Configure the subagent subsystem: `{enabled?, limits?:
   * {maxDepth?, maxConcurrency?, maxPerParent?, awaitPollMs?,
   * maxAwaitMs?, leaseTtlMs?, cancelOrphans?}, roles?:
   * {<id>: {description?, prompt?, allowedTools?, readOnly?,
   * canSpawn?, model?, providerId?, maxIterations?, capability?}},
   * delegationGuidance?}`. An omitted `roles` table ships the five
   * built-ins (general, researcher, explorer, implementer, reviewer);
   * partial tables keep the built-in defaults for any built-in role
   * they name. Hot: applies to the next spawn.
   * Disabled (the default) means no delegation tools exist at all.
   */
  configureSubagents(configJson: string): Promise<void>
  /** JSON view of the active subagent configuration. */
  subagentConfig(): Promise<string>
  /**
   * The children spawned by one parent chat: JSON array of the full
   * child views `{childChatId, parentChatId, parentTaskId, role, depth,
   * prompt, model, status, result, error, createdAt, updatedAt,
   * turnTaskId, taskStatus, waiting, queuedInputs}` — the registry row
   * joined with the live task state.
   */
  subagentsList(parentChatId: string): Promise<string>
  /** The state of one child session by id. */
  subagentStatus(childChatId: string): Promise<string>
  /**
   * Register the LSP namespace: `{workspaceRoot (REQUIRED, absolute),
   * config?: {servers?: {<language>: {command, args?, env?}},
   * requestTimeoutMs?, initTimeoutMs?, idleTimeoutMs?,
   * diagnosticsWaitMs?, maxRestarts?, restartWindowMs?,
   * allowWorkspaceBinaries?}}`. Language servers are DISCOVERED only
   * (explicit `servers` override → env → PATH; the workspace
   * `node_modules/.bin` walk requires `allowWorkspaceBinaries: true`
   * because running a binary that came with the repository is host
   * code execution) — a missing server fails the tool call honestly;
   * nothing is ever installed.
   */
  registerLspTools(optionsJson: string): Promise<void>
  /**
   * Configure advisor + watchdog: `{enabled?, model?, watchdogModel?,
   * maxIterations?, maxToolCallsPerIteration?, toolTimeoutMs?,
   * providerRequests?, overallTimeoutMs?, maxFindings?,
   * maxReviewConcurrency?, cooldownMs?, reviewsPerWindow?, windowMs?,
   * triggerConsecutiveToolFailures?, triggerRepeatedErrorCode?,
   * triggerEditCount?, triggerTurnFailures?, escalateCritical?}`.
   * `model`/`watchdogModel` set the explicit-review and automatic-review
   * models independently of the main agent's model. Hot: applies to the
   * next review.
   */
  configureAdvisor(configJson: string): Promise<void>
  /** JSON view of the active advisor configuration. */
  advisorConfig(): Promise<string>
  /**
   * Run one explicit advisory review of a session now (the `focus` is
   * the reviewer's task framing). Returns the persisted report JSON.
   */
  advisorReview(sessionId: string, focus: string): Promise<string>
  /** Persisted advisor findings for a session, newest first. */
  advisorFindings(sessionId: string, limit?: number | undefined | null): Promise<string>
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
   *   (or `closeUncertain` records the unproven drain). A running desktop
   *   HTTP server is stopped before Closing begins, so no REST/SSE
   *   request is admitted into the shutdown window.
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

export declare class StoreTransactionHandle {
  callStore(operation: string, inputJson: string): Promise<string>
  commit(): Promise<void>
  rollback(): Promise<void>
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
 * Synchronous liveness claim for one routed provider/tool callback: the
 * router asks this BEFORE entering user code. True exactly once, only
 * while Rust still considers the call live (not settled, rejected,
 * cancelled, abandoned by the executor, or drained by close); on false the
 * router must settle nothing and must never run the user function — the
 * host has already given up on this invocation.
 */
export declare function claimCall(callId: string): boolean

export declare function endRunProvider(callId: string): boolean

export declare function isRunCallbackActive(callId: string): boolean

export declare function pushRunProviderEvent(callId: string, eventJson: string): Promise<boolean>

/**
 * Register a provider connection: `baseUrl` addresses the OpenAI-compatible
 * server (the transport appends `/chat/completions`, `/responses`, or
 * `/models`), and `apiKey` — when present — is presented as
 * `Authorization: Bearer`. The key crosses the boundary exactly here and
 * never again: only the returned connection id may appear in options JSON.
 *
 * Without `optionsJson` the id is a process-local random: correct for a
 * scratch lane, but a Responses continuation bound to it dies with the
 * process. For a lane whose continuation must survive restarts, pass
 * `{connectionKey, storePath}` — the durable LOGICAL identity door
 * (P1-E): the id is derived from the key alone (never from the secret),
 * the store persists the credential fingerprint + generation, and the
 * credential-restoration rules hold: same key + same credential on every
 * restart → the same id AND generation (the continuation keeps working);
 * same key + a different credential → a rotation (new generation under
 * the same identity, stored scopes restamped, continuation continues);
 * a wrong credential is the provider's verdict — an auth error at
 * request time, never a silent identity change.
 */
export declare function registerProviderConnection(baseUrl: string, apiKey?: string | undefined | null, optionsJson?: string | undefined | null): Promise<string>

export declare function reserveRunProviderRequest(callId: string): Promise<void>

/**
 * The JS wrapper settles exactly one in-flight provider/tool callback with
 * either `{ payload }` (the JSON result) or `{ error }` (a rejection
 * message). Returns false for unknown or already-settled ids: a stale
 * resolve is ignored, never an exception, and never a double-settle.
 */
export declare function resolveCall(callId: string, payload?: string | undefined | null, error?: string | undefined | null): boolean

/**
 * Revoke a connection. Returns false for an unknown or already-revoked id.
 * A run in flight observes the revocation as `connection_changed` at its
 * next request; no further HTTP leaves for that connection afterwards.
 */
export declare function revokeProviderConnection(connectionId: string): Promise<boolean>
