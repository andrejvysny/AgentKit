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
  /** Start-time subagent `maxConcurrency` (1..=64, default 8). It sizes
   * the worker pool, so `configureSubagents` can lower the limit but any
   * larger value is clamped to this one. */
  subagentMaxConcurrency?: number;
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
  /** Further Rust provider lanes (at most 16; ids unique and different from
   * the primary's) that durable turns may route to by `providerId`, e.g. a
   * subagent role or the advisor pinned to another provider. Same shape and
   * validation as `providerConfig`; fixed for the service lifetime. */
  additionalProviders?: ReadonlyArray<NativeProviderConfig>;
  /** Declares the JS callback lane's parameter capabilities (mirrors
   * `providerAuthKind` in `rust/crates/napi/src/service.rs`).
   * `"chatgpt-account"` means the callback wraps an account-backed client
   * that rejects sampling/output parameters: context admission then never
   * injects an implicit output cap, and the bridge payload omits options
   * the adapter does not declare — exactly what the TypeScript lane does
   * for a `chatgpt-account` client. Default `"api-key"` (unchanged
   * behavior). Valid only without `providerConfig`, whose Rust lane is
   * always API-key shaped. */
  providerAuthKind?: "api-key" | "chatgpt-account";
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

/** Registration options for the NATIVE coding toolpack (mirrors
 * `optionsJson` on `registerCodingTools` in
 * `rust/crates/napi/src/coding_lane.rs`). An unsupported value fails
 * registration with `invalid_input`, never a silently ignored policy. */
export interface NativeCodingToolsOptions {
  /** The one root every native tool is confined to. REQUIRED, absolute. */
  workspaceRoot: string;
  /** Default false. Sessions whose descriptor workspace differs from the
   * registered root never get these tools (nor, for coding, standing
   * approvals), including their subagent children. Sessions with NO
   * descriptor workspace keep the service-wide behavior unless this is
   * true, in which case they get none either. */
  requireSessionWorkspace?: boolean;
  /** Execution backend for shell/test commands. Default `"required"`:
   * commands run only inside the platform sandbox, and hosts without one
   * refuse (`shell_isolation_unavailable`); `"off"` runs unsandboxed — an
   * explicit opt-in for trusted hosts and CI, never the default. */
  sandbox?: "required" | "off";
  /** Default false: the sandbox denies network. */
  network?: boolean;
  /** Exact command strings `run_tests` may run, e.g. `["bun test"]`. */
  testCommands?: readonly string[];
  /** Default false: the sandbox denies writes under `<root>/.git`. */
  allowGitWrites?: boolean;
  /** Default false: credential-like paths are refused for read and write. */
  allowSensitive?: boolean;
  /** Mode applied until the SESSION's durable execution state says
   * otherwise (the mode is a fact in the database, never binding memory).
   * Default `"ask"` — fail closed, like the JS authority's default. */
  configuredMode?: "read_only" | "ask" | "auto_edit" | "full_auto" | "plan";
  /** Role ceiling; a ceiling NO mode lifts. Default `"implementer"`. */
  role?: "implementer" | "reviewer" | "supervisor";
  /** Tool names whose approvable refusal is a PLAN approval. Default
   * `["submit_plan"]`. */
  planToolNames?: readonly string[];
  /** The workspace identity inside the standing-approval key envelope:
   * two different checkouts mounted at the same path keep independent
   * approvals. Default: `workspaceRoot`. */
  identity?: string;
  /** Lock-dir coordination directory. Default the per-uid shared
   * `/tmp/agentkit-workspace-locks-<uid>` (TS parity: durable shell
   * ownership and cross-process crash recovery are on disk). */
  coordinationDir?: string;
  /** Longest a mutation/command waits for a foreign lease before
   * `workspace_busy`. Default `min(20000, floor(toolTimeoutMs * 0.66))`. */
  coordinationWaitMs?: number;
  /** Base environment for spawned shell/test commands. REPLACES the host
   * process environment (it is not overlaid), and is the same map the
   * standing-approval key fingerprints. Default: the host process
   * environment. Names are non-empty without `=` or NUL, values are
   * strings without NUL, at most 1024 entries; otherwise `invalid_input`. */
  environment?: Readonly<Record<string, string>>;
}

/** Global fan-out bounds for delegation, enforced from durable rows at
 * spawn time (mirrors `SubagentRoleLimits` in
 * `rust/crates/host/src/subagents/config.rs`). */
export interface NativeSubagentLimits {
  /** Child depth below the main agent. Default 2; 0 disables nesting. */
  maxDepth?: number;
  /** Non-terminal children across the whole service. Default 8. */
  maxConcurrency?: number;
  /** Non-terminal children per parent chat. Default 4. */
  maxPerParent?: number;
  /** Poll cadence inside `subagent_await`, ms. Floor 10. */
  awaitPollMs?: number;
  /** Hard wall on one `subagent_await` call, ms. */
  maxAwaitMs?: number;
  /** Exclusive workspace-lease TTL, ms; refreshed while the child runs. */
  leaseTtlMs?: number;
  /** Cancel still-live children when their parent turn settles
   * (startup sweep + settle observers). Default true. */
  cancelOrphans?: boolean;
  /** Cap on provider requests reserved by all subagents below one root
   * session; unset means unlimited. Provider requests only; token and
   * monetary budgets are not enforced. Integer 1..=1000000. */
  maxTreeProviderRequests?: number;
}

/** One delegation role: a ceiling, never a grant (mirrors `RoleSpec`).
 * Unknown role ids in `roles` are custom roles; naming a built-in
 * (general, researcher, explorer, implementer, reviewer) partially
 * overrides its defaults. */
export interface NativeSubagentRole {
  description?: string;
  /** Prepended to the spawn prompt as the child's role briefing. */
  prompt?: string;
  /** Tool allowlist (`"<ns>:<name>"` or bare name; `"*"` = everything
   * the host guard chain allows anyway). Empty array denies ALL tools. */
  allowedTools?: readonly string[];
  /** Read-only roles are pinned to `read_only` execution mode and may
   * not mutate the workspace, regardless of the parent's grants. */
  readOnly?: boolean;
  /** Whether this role may spawn its own children (subject to depth). */
  canSpawn?: boolean;
  /** Model override for this role; inherits the parent when omitted. */
  model?: string | null;
  /** Provider override; inherits the parent binding when omitted. */
  providerId?: string | null;
  /** Tool-loop bound for the role's turns. */
  maxIterations?: number | null;
  /** Free-form capability summary surfaced in listings. */
  capability?: string | null;
}

/** Subagent subsystem configuration (mirrors `SubagentConfig`).
 * An omitted `roles` table ships the five built-in roles. */
export interface NativeSubagentConfig {
  /** Master switch. While false the delegation tools do not exist. */
  enabled?: boolean;
  limits?: NativeSubagentLimits;
  roles?: Record<string, NativeSubagentRole>;
  /** Guidance text injected into session system prompts while enabled. */
  delegationGuidance?: string | null;
}

/** One child's registry row joined with its live task state (mirrors
 * `SubagentView`). `status`: queued | running | completed | failed |
 * cancelled. */
export interface NativeSubagentView {
  childChatId: string;
  parentChatId: string;
  parentTaskId: string | null;
  role: string;
  depth: number;
  prompt: string;
  model: string | null;
  status: string;
  result: string | null;
  error: string | null;
  createdAt: number;
  updatedAt: number;
  turnTaskId: string | null;
  taskStatus: string | null;
  waiting: boolean;
  queuedInputs: number;
}

/** Explicit language-server command (discovery is override → env → PATH
 * → workspace `node_modules/.bin`; nothing is ever installed). */
export interface NativeLspServerCommand {
  command: string;
  args?: readonly string[];
  env?: Record<string, string>;
}

/** LSP client tuning (mirrors `LspConfig` in `rust/crates/lsp`). */
export interface NativeLspConfig {
  /** Per-language overrides, keyed `rust` | `typescript`. */
  servers?: Record<string, NativeLspServerCommand>;
  requestTimeoutMs?: number;
  initTimeoutMs?: number;
  idleTimeoutMs?: number;
  diagnosticsWaitMs?: number;
  maxRestarts?: number;
  restartWindowMs?: number;
}

/** Registration options for the native LSP namespace: the six `lsp_*`
 * read tools (`lsp_definition`, `lsp_references`, `lsp_hover`,
 * `lsp_symbols`, `lsp_implementation`, `lsp_diagnostics`), shared
 * workspace-scoped servers. */
export interface NativeLspToolsOptions {
  /** The one workspace root the servers serve. REQUIRED, absolute. */
  workspaceRoot: string;
  /** Default false. Sessions whose descriptor workspace differs from the
   * registered root never get these tools (nor, for coding, standing
   * approvals), including their subagent children. Sessions with NO
   * descriptor workspace keep the service-wide behavior unless this is
   * true, in which case they get none either. */
  requireSessionWorkspace?: boolean;
  config?: NativeLspConfig;
}

/** Advisor + watchdog configuration (mirrors `AdvisorConfig`). The
 * advisor's models are set independently of the main agent's model. */
export interface NativeAdvisorConfig {
  /** Master switch. While false there are no advisor tools, no hooks,
   * no settled observations. */
  enabled?: boolean;
  /** Model for explicit `advisor_review` calls. */
  model?: string | null;
  /** Provider for explicit reviews; absent shares the service's primary
   * provider. A different provider requires `model`. Must name the primary
   * or an `additionalProviders` entry (`invalid_input` otherwise). */
  providerId?: string | null;
  /** Provider for automatic watchdog reviews; same rules, needs
   * `watchdogModel`. */
  watchdogProviderId?: string | null;
  /** Model for automatic watchdog reviews. */
  watchdogModel?: string | null;
  maxIterations?: number;
  maxToolCallsPerIteration?: number;
  toolTimeoutMs?: number;
  providerRequests?: number | null;
  overallTimeoutMs?: number | null;
  /** Hard cap on findings kept from one report. */
  maxFindings?: number;
  /** Concurrent reviews process-wide. */
  maxReviewConcurrency?: number;
  /** Per-chat+trigger cooldown between automatic reviews, ms. */
  cooldownMs?: number;
  /** Rolling-window watchdog review budget. */
  reviewsPerWindow?: number;
  windowMs?: number;
  /** Consecutive failed tool results in one run that raise a review. */
  triggerConsecutiveToolFailures?: number;
  /** Repetitions of the SAME error code that raise a review. */
  triggerRepeatedErrorCode?: number;
  /** Distinct files one turn may touch before a scope-deviation review. */
  triggerEditCount?: number;
  /** Consecutive failed turns of a session that raise a review. */
  triggerTurnFailures?: number;
  /** Deliver critical findings to the reviewed session as a steer. */
  escalateCritical?: boolean;
}

/** One persisted advisor finding (`advisor_findings` rows, newest
 * first). `detail` carries the report summary, evidence, recommendation,
 * and category. */
export interface NativeAdvisorFinding {
  findingId: string;
  chatId: string;
  source: "explicit" | "watchdog";
  triggerKind: string;
  severity: "info" | "warning" | "critical";
  title: string;
  detail: Record<string, unknown>;
  model: string;
  createdAt: number;
  escalated: boolean;
}

/** The structured result of one advisor review. */
export interface NativeAdvisorReport {
  runId: string;
  summary: string;
  findings: Array<{
    severity: "info" | "warning" | "critical";
    category: string;
    title: string;
    evidence: Array<{ file?: string; line?: number; detail: string }>;
    recommendation?: string;
  }>;
  findingIds: string[];
}

/** Registration options for a DURABLE provider connection (mirrors
 * `optionsJson` on `registerProviderConnection` in
 * `rust/crates/napi/src/provider_connection.rs`). Without these, the
 * connection id is a process-local random and a Responses continuation
 * bound to it dies with the process. With them, the id is the store's
 * logical connection identity: the same `connectionKey` + same
 * credentials resolve to the same id and generation in every process
 * (credential restoration), and a changed credential rotates the
 * generation under the same identity while the continuation continues.
 * `connectionKey` is non-secret caller material — NEVER pass the API key
 * or anything derived from it. */
export interface NativeProviderConnectionOptions {
  /** Stable caller-chosen name identifying THIS logical connection
   * (e.g. include the provider id). Non-empty, ≤ 512 characters. */
  connectionKey: string;
  /** Path of the file-backed store that persists the connection state —
   * the same file `NativeStore.open` owns. */
  storePath: string;
}

/** Options for `NativeSessionService.startHttpServer` (mirrors
 * `startHttpServer` in `rust/crates/napi/src/http_server.rs`). The server
 * is the Rust REST v1 + SSE engine serving this service's own host and
 * store. Only `bearerToken` is required; the backend itself refuses tokens
 * shorter than 32 bytes, and the binding rejects them up front. */
export interface NativeHttpServerOptions {
  /** Interface to bind. Default `127.0.0.1`. Only loopback hosts
   * (`127.0.0.0/8`, `::1`, `localhost`) are accepted unless
   * `allowNonLoopback` is set. */
  host?: string;
  /** Opt in to binding a non-loopback host. Without it such a host fails
   * with `non_loopback_bind_refused`. Default false. */
  allowNonLoopback?: boolean;
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
  /** Cross-origin policy for browser callers, mirroring the retired
   * TypeScript transport's `CorsConfig` exactly. OFF by default: with no
   * `cors` no response ever carries a CORS header and `OPTIONS` answers
   * 405. When set, `Vary: Origin` rides EVERY response (including
   * problems and SSE), a matched `Origin` also gets
   * `Access-Control-Allow-Origin` (the list echoes the origin; `"*"`
   * echoes a literal `*`), and a matched-origin `OPTIONS` preflight
   * answers 204 with that path's real method set. An unmatched origin
   * is served normally — the browser, not the server, does the
   * blocking. */
  cors?: NativeHttpCorsOptions;
  /** `"memory"` wires a VOLATILE in-process credential vault so the
   * provider-management routes' `apiKey`/`extraHeaders` writes really
   * store secrets (mirroring the desktop example's `MemorySecretStore`;
   * secrets die with the process — durable vaults are a host-side port
   * decision). Omitted, those writes answer 501 exactly like the
   * TypeScript transport running without a `SecretStore`. */
  providerSecrets?: "memory";
}

/** `cors` member of {@link NativeHttpServerOptions}. */
export interface NativeHttpCorsOptions {
  /** Origins allowed to READ responses: `"*"` for any, or an exact-match
   * list of origin strings (scheme + host + port, no path, no trailing
   * slash). Non-empty; omit `cors` entirely to stay closed. */
  origins: readonly string[] | "*";
  /** Preflight `Access-Control-Allow-Headers`. Default: `Content-Type`,
   * `Idempotency-Key`, `Last-Event-ID`, `Authorization`. */
  allowHeaders?: readonly string[];
  /** Response headers browsers may read. Omitted by default: the bodies
   * carry everything a client needs. */
  exposeHeaders?: readonly string[];
  /** Preflight cache lifetime; omitted sends no `Max-Age`. */
  maxAgeSeconds?: number;
}

/** Where a started desktop server actually listens (an ephemeral port is
 * resolved by the OS; `baseUrl` already includes the normalized base
 * path). */
export interface NativeHttpServerInfo {
  port: number;
  baseUrl: string;
}

/** Services not yet closed; see `NativeStore.createService`. */
const OPEN_SERVICES = new Set<NativeSessionService>();

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

  /** Opt into claim-time priority aging (same formula as the TS stores).
   * Omitted fields take defaults; `agingBonus` 0 leaves aging off. Values
   * must be non-negative safe integers (interval >= 1). Call before
   * `createService`: a service built earlier keeps the setting it saw. */
  setTaskAging(options: {
    agingIntervalMs?: number;
    agingBonus?: number;
    agingMaxBonus?: number;
  }): void {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    try {
      this.raw.setTaskAging(
        options.agingIntervalMs,
        options.agingBonus,
        options.agingMaxBonus,
      );
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
    // P1-F: the JS wrapper is the earliest layer that sees
    // `providerConfig.maxTokens` (NaN would silently become `null` under
    // `JSON.stringify`), so it refuses anything the request budget cannot
    // represent, with the same stable message the Rust parser carries.
    // Service construction — not admission, not the wire — is where an
    // unusable cap dies; the admitted request is the transmitted request.
    const configuredTokens = options?.providerConfig?.maxTokens;
    if (
      configuredTokens !== undefined &&
      !(Number.isSafeInteger(configuredTokens) && configuredTokens > 0)
    ) {
      throw new NativeAgentError(
        "invalid_input",
        "providerConfig.maxTokens must be a positive safe integer",
      );
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
    // Tag the native service with this Node environment so the addon's
    // environment-cleanup hook only ever stops this env's services.
    raw.bindEnv();
    const service = new NativeSessionService(
      raw,
      router,
      this,
      options?.providerConfig,
    );
    // Strong reference until `close()`: a service kept only for its effects
    // (an HTTP/MCP listener, the worker) must not be reclaimed by GC when
    // the caller drops the handle. The native Drop is a last resort only.
    OPEN_SERVICES.add(service);
    return service;
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
    const text = await lift(() => this.raw.mcpConnect(JSON.stringify(config)));
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
   * `handle` is the descriptor's `handle` field
   * (`descriptor.handle`, e.g. `"fix#2"`). Pass it: once an alias has been
   * re-registered, a call whose handle is missing or comes from a
   * superseded registration is refused with `mcp_authority_revoked`
   * before the transport is touched, so a stale descriptor can never
   * execute the replacement server's callback. Dispatch is additionally
   * pinned to the transport generation the descriptor was listed from:
   * after an automatic reconnect a stale descriptor fails
   * `mcp_authority_revoked`/`mcp_not_connected` instead of executing the
   * new generation's handler — reconnect (`mcpDisconnect` + `mcpConnect`)
   * to publish a fresh descriptor set.
   *
   * `signal` is the originating invocation's abort handle (the tool ctx
   * `signal`): passing it lets the binding tear the in-flight call down —
   * a queued call is never written to the wire, and an already-aborted
   * signal never dispatches at all (`mcp_request_aborted`).
   */
  async mcpCallTool(
    alias: string,
    toolName: string,
    args: unknown,
    handle?: string,
    signal?: AbortSignal,
  ): Promise<ToolResultJson> {
    if (this.closed)
      throw new NativeAgentError("closed", "the native store is closed");
    if (signal?.aborted)
      throw new NativeAgentError(
        "mcp_request_aborted",
        `MCP tool ${alias}.${toolName} was cancelled before dispatch`,
      );
    const cancellationKey = signal ? crypto.randomUUID() : undefined;
    let onAbort: (() => void) | undefined;
    if (signal && cancellationKey) {
      onAbort = () => {
        void this.raw.mcpCancelCall(cancellationKey).catch(() => undefined);
      };
      signal.addEventListener("abort", onAbort, { once: true });
    }
    try {
      const text = await lift(() =>
        this.raw.mcpCallTool(
          alias,
          toolName,
          JSON.stringify(args ?? {}),
          handle,
          cancellationKey,
        ),
      );
      return JSON.parse(text) as ToolResultJson;
    } finally {
      if (signal && onAbort) signal.removeEventListener("abort", onAbort);
    }
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

export { NativeManagedContent } from "./managed-content.js";
export type {
  ManagedConnectionDraft,
  ManagedSkillEntry,
  NativeConnections,
  NativeManagedContentOptions,
  NativePromptTemplates,
  NativeRunContent,
  NativeSkills,
  RedactedConnectionRecord,
  RenderedPromptTemplate,
  RunContentBlock,
  RunContentResolution,
  RunContentResourceRecord,
  UnavailableRunSkill,
} from "./managed-content.js";

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

/** How an operator closes an uncertain (admitted, no result) receipt. */
export type ExecutionResolutionOutcome =
  | "completed"
  | "not_started"
  | "acknowledged";

export interface ExecutionResolutionJson {
  outcome: ExecutionResolutionOutcome;
  note?: string;
  resolvedAt: number;
  resolvedBy?: string;
}

/** A durable execution receipt as the host stores it. */
export interface ExecutionReceiptJson {
  executionId: string;
  runId: string;
  chatId: string;
  toolCallId: string;
  toolName: string;
  argsSha256: string;
  contextSha256: string;
  attemptId: string;
  approvalRequestId?: string;
  approvalGeneration?: string;
  status: "admitted" | "completed" | "resolved";
  result?: Record<string, unknown>;
  resolution?: ExecutionResolutionJson;
}

export interface ResolveUncertainExecutionInput {
  sessionId: string;
  executionId: string;
  outcome: ExecutionResolutionOutcome;
  /** Required for `completed` (string or JSON value); forbidden otherwise. */
  result?: unknown;
  note?: string;
  resolvedBy?: string;
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
   * again — `createService` receives only the returned connection id.
   * Pass `options` with a stable `connectionKey` and the store's
   * `storePath` to register a DURABLE logical connection whose Responses
   * continuation survives a real process restart (see
   * `NativeProviderConnectionOptions`); without them the id is a
   * process-local random. */
  static async registerProviderConnection(
    baseUrl: string,
    apiKey?: string | null,
    options?: NativeProviderConnectionOptions,
  ): Promise<string> {
    return lift(async () =>
      (await loadBinding()).registerProviderConnection(
        baseUrl,
        apiKey ?? null,
        options === undefined ? null : JSON.stringify(options),
      ),
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

  /** Register a JS-executed tool. A tool NAME is a unique registration
   * identity, enforced atomically across both registries: Rust accepts the
   * definition first, and the router receives the callback ONLY after that
   * acceptance. A second registration of a known name therefore REJECTS
   * (stable `invalid_input` error) and leaves the previously accepted
   * descriptor, gate, and callback untouched — name, advertised descriptor,
   * authority gate, and dispatched callback always denote one registration. */
  async registerTool(
    definition: ToolDefinitionJson,
    run: ToolRunFunction,
  ): Promise<void> {
    await lift(() => this.raw.registerTool(JSON.stringify(definition)));
    this.router.registerTool(definition, run);
  }

  /** Register the NATIVE coding toolpack: the Rust-owned `read_file`,
   * `list_dir`, `find_files`, `code_search`, `repo_map`, `git_inspect`,
   * `edit_file`, `write_file`, `run_command`, `run_tests`, and
   * `submit_plan` on the SAME executor lane as JS tools. Descriptors,
   * execution, permission (the shared decision matrix over the session's
   * durable mode), high-risk fresh-approval, and fingerprint-v4 standing
   * approvals are ALL Rust-side: no callback, no TS pack, no fallback
   * crosses into JS on this path. One-shot per service — the descriptor
   * set is immutable once the worker can claim turns; a second call
   * rejects with `invalid_input`. (mirrors `registerCodingTools` in
   * `rust/crates/napi/src/coding_lane.rs`) */
  async registerCodingTools(options: NativeCodingToolsOptions): Promise<void> {
    await lift(() => this.raw.registerCodingTools(JSON.stringify(options)));
  }

  /** Configure the subagent subsystem (hot: applies to the next spawn).
   * While disabled (the default) the seven `subagent_*` tools do not
   * exist, so sessions cannot delegate at all. (mirrors
   * `configure_subagents` in `rust/crates/napi/src/service.rs`) */
  async configureSubagents(config: NativeSubagentConfig): Promise<void> {
    await lift(() => this.raw.configureSubagents(JSON.stringify(config)));
  }

  /** JSON view of the active subagent configuration, or null before the
   * first `configureSubagents`. */
  async subagentConfig(): Promise<NativeSubagentConfig | null> {
    const text = await lift(() => this.raw.subagentConfig());
    return text === "null" ? null : (JSON.parse(text) as NativeSubagentConfig);
  }

  /** The children of one parent chat: the durable registry rows joined
   * with live task state. */
  async subagentsList(parentChatId: string): Promise<NativeSubagentView[]> {
    const text = await lift(() => this.raw.subagentsList(parentChatId));
    return JSON.parse(text) as NativeSubagentView[];
  }

  /** The state of one child session by id. */
  async subagentStatus(childChatId: string): Promise<NativeSubagentView> {
    const text = await lift(() => this.raw.subagentStatus(childChatId));
    return JSON.parse(text) as NativeSubagentView;
  }

  /** Register the native LSP namespace: the six `lsp_*` read tools over
   * workspace-scoped, shared, DISCOVERED-ONLY language servers. One-shot
   * per service; the root is immutable once the worker can claim turns.
   * (mirrors `register_lsp_tools` in `rust/crates/napi/src/service.rs`) */
  async registerLspTools(options: NativeLspToolsOptions): Promise<void> {
    await lift(() => this.raw.registerLspTools(JSON.stringify(options)));
  }

  /** Configure advisor + watchdog (hot: applies to the next review).
   * While disabled (the default) there are no advisor tools, no run
   * hooks, and no settled observations. */
  async configureAdvisor(config: NativeAdvisorConfig): Promise<void> {
    await lift(() => this.raw.configureAdvisor(JSON.stringify(config)));
  }

  /** JSON view of the active advisor configuration, or null before the
   * first `configureAdvisor`. */
  async advisorConfig(): Promise<NativeAdvisorConfig | null> {
    const text = await lift(() => this.raw.advisorConfig());
    return text === "null" ? null : (JSON.parse(text) as NativeAdvisorConfig);
  }

  /** Run one explicit advisory review of a session now. The advisor
   * investigates with its own read-only tools on its own model and
   * returns structured, evidence-backed findings — persisted durably,
   * announced on the host event stream, and (criticals) injected into
   * the reviewed session as a durable steer. */
  async advisorReview(
    sessionId: string,
    focus: string,
    options?: NativeCallOptions,
  ): Promise<NativeAdvisorReport> {
    const text = await waitFor(
      () => lift(() => this.raw.advisorReview(sessionId, focus)),
      options,
    );
    return JSON.parse(text) as NativeAdvisorReport;
  }

  /** Persisted advisor findings for a session, newest first. */
  async advisorFindings(
    sessionId: string,
    limit?: number,
  ): Promise<NativeAdvisorFinding[]> {
    const text = await lift(() => this.raw.advisorFindings(sessionId, limit));
    return JSON.parse(text) as NativeAdvisorFinding[];
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

  /** Steer a LIVE turn only; an idle session rejects with `chat_idle` rather
   * than starting a turn. */
  async steerLive(
    sessionId: string,
    input: SendInputJson,
    options?: NativeCallOptions,
  ): Promise<SendResultJson> {
    const text = await waitFor(
      () => lift(() => this.raw.steerLive(sessionId, JSON.stringify(input))),
      options,
    );
    return JSON.parse(text) as SendResultJson;
  }

  /** Admitted receipts without a result: effects whose outcome is unknown. */
  async listUncertainExecutions(
    sessionId: string,
  ): Promise<ExecutionReceiptJson[]> {
    const text = await lift(() => this.raw.listUncertainExecutions(sessionId));
    return JSON.parse(text) as ExecutionReceiptJson[];
  }

  /** Explicitly close one uncertain receipt; never happens implicitly. */
  async resolveUncertainExecution(
    input: ResolveUncertainExecutionInput,
  ): Promise<ExecutionReceiptJson> {
    const text = await lift(() =>
      this.raw.resolveUncertainExecution(JSON.stringify(input)),
    );
    return JSON.parse(text) as ExecutionReceiptJson;
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
        OPEN_SERVICES.delete(this);
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
