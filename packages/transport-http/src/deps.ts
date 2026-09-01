/**
 * What the adapter needs from a host, and nothing more.
 *
 * The services are taken as STRUCTURAL interfaces rather than as
 * `@agentkit/host`'s concrete classes. `TurnRunner`, `TaskService` and
 * `ProposalService` all hold private fields, and TypeScript's structural
 * compatibility rules make a private member nominal: typing a dependency as the
 * class would force a host to hand over an instance of that exact class and
 * make a hand-rolled equivalent — or a test double — impossible to pass. Naming
 * the three methods this adapter actually calls costs a dozen lines and keeps
 * the seam a seam.
 */
import type { ModelDto } from "@agentkit/contracts";
import type {
  ApplyOutcome,
  ApplyProposalRequest,
  ApproveProposalInput,
  AssistantStore,
  AuthorizationPort,
  Clock,
  Logger,
  ProposalRecord,
  RegenerateMessageInput,
  RejectProposalInput,
  SecretStore,
  SubmitMessageInput,
  SubmitMessageResult,
  ToolCatalog,
  WriteAllowance,
  WriteAllowanceInput,
} from "@agentkit/host";
import type { RestCorsOptions } from "./cors.js";

/** `TurnRunner`, narrowed to the two calls that start a run. */
export interface TurnSubmitter {
  submitMessage(input: SubmitMessageInput): Promise<SubmitMessageResult>;
  /**
   * Required, not optional, unlike the service deps below: `regenerateMessage`
   * is a route in the contract, and a `TurnRunner` has always had somewhere to
   * put this. A host that hands over a hand-rolled submitter is hand-rolling a
   * turn runner, and "re-answer the same question" is not a capability it can
   * meaningfully lack while claiming to run turns.
   */
  regenerate(input: RegenerateMessageInput): Promise<SubmitMessageResult>;
}

/** `TaskService`, narrowed to the one call `cancelRun` makes. */
export interface RunCanceller {
  cancelTask(taskId: string): Promise<void>;
}

/** `ProposalService`, narrowed to the three decision routes. */
export interface ProposalOperations {
  approve(input: ApproveProposalInput): Promise<ProposalRecord>;
  reject(input: RejectProposalInput): Promise<ProposalRecord>;
  apply(input: ApplyProposalRequest): Promise<ApplyOutcome>;
}

/** `ConversationService`, narrowed to the one call `deleteChat` makes. */
export interface ConversationOperations {
  deleteChat(chatId: string): Promise<void>;
}

/**
 * The two provider operations this adapter CANNOT perform itself.
 *
 * Both of them talk to somebody else's server: refreshing a catalogue is a
 * `GET /models` against the provider, and testing a connection is a probe with
 * the provider's credential injected. This package has no provider client, no
 * `SecretStore` resolution path and no opinion about what "reachable" means for
 * an endpoint it has never heard of — a host has all three, in the same
 * `providerFactory` its `TurnRunner` already uses.
 *
 * Absent, both routes answer 501: the routes exist in the contract and another
 * deployment of the same version serves them.
 */
export interface ProviderOperations {
  /** Probe the catalogue and REPLACE the stored one; answers with what it wrote. */
  refreshModels(providerId: string): Promise<ModelDto[]>;
  /**
   * Reachability + credentials. A failure is a RESULT, not a throw: `ok: false`
   * with a sayable `error` is what a settings pane renders, and an exception
   * would turn "this endpoint is down" into a 500.
   */
  testConnection(providerId: string): Promise<{ ok: boolean; error?: string }>;
}

/**
 * `WritePolicy`, narrowed to the three allowance routes.
 *
 * Every method is CHAT-SCOPED, because the port is: `SessionWritePolicy` holds
 * grants per `(chat, tool, kind)` and offers no unscoped listing. That is why
 * all three routes are nested under `/v1/chats/:chatId` rather than standing
 * alone — widening the port to serve a prettier URL would let one chat's UI
 * enumerate (and revoke) another's consent, and a chat that is not in the path
 * is a chat an `AuthorizationPort` cannot gate on.
 */
export interface WritePolicyOperations {
  list(chatId: string): WriteAllowance[];
  allow(input: WriteAllowanceInput): WriteAllowance;
  revoke(chatId: string, key: string): void;
}

/**
 * One stored MCP server config, as this adapter sees it.
 *
 * A STRUCTURAL restatement of `McpServerConfigRecord`
 * (`@agentkit/mcp-client`), not an import of it: taking the dependency would
 * make every host that wires this transport install the MCP bridge — and its
 * SDK — to serve four CRUD routes it may not use. `transport` and `resilience`
 * are `unknown` here because this adapter never interprets either; the request
 * validator checks the transport union's shape at the boundary, and the store
 * on the other side has the real types.
 */
export interface McpServerConfigLike {
  id: string;
  alias: string;
  transport: unknown;
  secretRefs?: Record<string, string>;
  enabled?: boolean;
  toolAliases?: Record<string, string>;
  resilience?: unknown;
  createdAt: string;
  updatedAt: string;
}

/** `McpServerConfigStore` (`@agentkit/mcp-client`), structurally. */
export interface McpServerConfigOperations {
  list(): Promise<McpServerConfigLike[]>;
  get(id: string): Promise<McpServerConfigLike | null>;
  create(record: McpServerConfigLike): Promise<McpServerConfigLike>;
  update(
    id: string,
    patch: Partial<McpServerConfigLike>,
  ): Promise<McpServerConfigLike>;
  delete(id: string): Promise<void>;
}

/**
 * SSE tuning. All four have defaults that suit a UI on a local network; a
 * deployment behind a proxy that kills idle connections lowers
 * `heartbeatIntervalMs`, and one with a slow store raises `pollIntervalMs`.
 */
export interface RestStreamOptions {
  /**
   * How long the stream waits before asking the event log again, once it has
   * caught up. The floor on end-to-end latency for a live token.
   *
   * It does NOT govern a stream that is behind rather than idle: a pump parked
   * on a full queue resumes on the consumer's next read, not on a timer, so a
   * generous poll interval never becomes a throttle on a backlog.
   */
  pollIntervalMs?: number;
  /**
   * Idle time before a `: hb` comment is written. Its job is to keep proxies
   * and load balancers from reaping a connection that is legitimately quiet
   * (a long tool call), and to surface a dead peer to the writer.
   */
  heartbeatIntervalMs?: number;
  /** The `retry:` hint sent first — how long a client waits before reconnecting. */
  retryHintMs?: number;
  /**
   * How many events one read of the log may return, and — because they are the
   * same bound seen from the two ends of the pipe — how many frames may sit
   * queued for a consumer that has stopped reading.
   *
   * It is what keeps a stream's memory a function of this number instead of a
   * function of the run: replaying a hundred-thousand-event log used to mean
   * materialising a hundred thousand envelopes, and at cursor 0 doing it again
   * on every poll. The cost is one extra store round trip per full batch, paid
   * only while there is a backlog to walk.
   */
  readBatchSize?: number;
}

export const DEFAULT_STREAM_OPTIONS: Required<RestStreamOptions> =
  Object.freeze({
    pollIntervalMs: 150,
    heartbeatIntervalMs: 15_000,
    retryHintMs: 2_000,
    readBatchSize: 256,
  });

export interface RestHandlerDeps {
  /** Reads for every route; the writes go through the services below. */
  store: AssistantStore;
  turns: TurnSubmitter;
  tasks: RunCanceller;
  /**
   * Absent on a host with no staged-write pipeline. `listProposals` still works
   * (it reads the store); the three decision routes answer 501, because the
   * route exists in the contract and this deployment simply cannot serve it.
   */
  proposals?: ProposalOperations;
  /**
   * Absent on a host that does not let a client delete conversations.
   * `deleteChat` answers 501 without it, and `updateChat` still works — a
   * rename goes straight to `ConversationStore`, while a delete spans three
   * stores and is a policy decision (`chat_busy`) only the service makes.
   */
  conversations?: ConversationOperations;
  /**
   * Probing a provider: `refreshProviderModels` and `testProvider`. Absent,
   * both answer 501 — this package cannot talk to a provider, and inventing a
   * client here would be a second place that decides what a request to someone
   * else's endpoint looks like. See {@link ProviderOperations}.
   */
  providerOps?: ProviderOperations;
  /**
   * Where a write-only `apiKey` on `createProvider`/`updateProvider` is put.
   *
   * Absent, a request CARRYING a key answers 501 rather than storing it: the
   * only other options are to write the credential into the provider config
   * (where `listProviders` and every log line would find it) or to accept the
   * request and silently drop the key, and a provider that quietly has no
   * credential fails later, somewhere else. A request with no `apiKey` needs no
   * secret store and works exactly as it would with one.
   */
  secrets?: SecretStore;
  /**
   * The standing-write-grant routes, over the host's own `WritePolicy` — e.g.
   * `SessionWritePolicy`. Absent, all three answer 501.
   *
   * Wiring it exposes consent to an HTTP client, which is a decision worth
   * making deliberately: `SessionWritePolicy` deliberately keeps grants in
   * memory for the life of the process, and a route that grants one is a route
   * that can be called by anything `authenticate` lets through.
   */
  writePolicy?: WritePolicyOperations;
  /**
   * The MCP server-config CRUD routes, over an `McpServerConfigStore`
   * (`@agentkit/mcp-client`). Absent, all four answer 501 — a host that
   * declares its MCP servers in a config file has nothing for them to write to.
   */
  mcpConfigs?: McpServerConfigOperations;
  /**
   * What `listTools` advertises — the host's {@link ToolCatalog} port.
   *
   * Still optional: `ToolSetContributor.contribute` is a per-RUN call (bindings,
   * limits, scope — all of which belong to a conversation) and `GET /v1/tools`
   * names no conversation, so a host that cannot enumerate without one leaves
   * this out and the route answers 501 rather than inventing a run context. A
   * host that can wires `createContributorToolCatalog` from `@agentkit/host`,
   * which answers by staging the real contributors through the real staging
   * path (namespaces, guards and unbound pruning included).
   */
  toolCatalog?: ToolCatalog;
  /** Reported by `getVersion` as `packages`, for an identifiable build. */
  packages?: Record<string, string>;
  /**
   * Called before any route runs. Answers "who is calling?".
   *
   * Returning a `Response` short-circuits the request with it (401, a redirect,
   * whatever the host's scheme needs). Any other value is the PRINCIPAL: it is
   * passed to {@link RestHandlerDeps.authorize} as the `subject` (an object
   * verbatim, anything else under `metadata.principal`) and threaded to every
   * route handler as `RouteContext.principal`. This adapter still never
   * interprets it — only the host's own `AuthorizationPort` does.
   */
  authenticate?(req: Request): Promise<unknown | Response>;
  /**
   * Consulted per route, after {@link RestHandlerDeps.authenticate} and before
   * the route runs, with the authenticated principal as the subject, the
   * resource the route's path names (see `authorize.ts`'s table) and an action
   * of `read` for GET or `write` for anything else. A refusal is a 403
   * `forbidden` problem carrying the decision's `reason`.
   *
   * ABSENT — the default — nothing is checked. That is not an oversight to work
   * around: a host that does not wire this port gets no authorization, and one
   * that needs it must supply it (or filter in front of the handler). The one
   * route never submitted to the port is `GET /v1/version`, which reads two
   * constants and is how a client discovers whether it can speak to this server
   * at all.
   */
  authorize?: AuthorizationPort;
  /**
   * Mount prefix, e.g. `"/api/agentkit"`, stripped before routing so
   * `REST_ROUTES`' `/v1/...` paths match underneath it. Normalized (a leading
   * slash added, trailing ones removed); `""` and `"/"` mean no prefix. A
   * request outside the prefix gets the same 404 problem any unrouted path
   * does.
   */
  basePath?: string;
  /**
   * Cross-origin access. Absent — the default — no response carries a CORS
   * header and `OPTIONS` answers 405 as it always did.
   */
  cors?: RestCorsOptions;
  /**
   * Largest request body this handler will accept, in bytes. **Absent — the
   * default — there is no cap at all.**
   *
   * Off by default because a limit that is right for one deployment is wrong
   * for the next: a submit carrying an inline image is legitimately megabytes,
   * and a handler that guessed a ceiling would reject real turns on a host that
   * never asked it to. A served deployment should set one here or, better,
   * enforce it in the proxy in front of the handler, which can refuse the
   * upload before the bytes are on this process's heap.
   *
   * Enforced from `Content-Length` when the request declares one — the refusal
   * then costs nothing — and by measuring the body when it does not. Over the
   * limit is a **413** `body_too_large` problem, in the same shape as every
   * other error on this surface.
   */
  maxBodyBytes?: number;
  logger?: Logger;
  /**
   * Where the two timestamps `createMcpServer` mints come from. Defaults to
   * `defaultClock`.
   *
   * A port rather than a `new Date()` for the same reason the host layer has
   * one: a test that cannot pin "now" cannot assert what a record was stamped
   * with, and a deployment whose records must agree with the host's own clock
   * has nowhere to say so otherwise. Every other timestamp on this surface is
   * the host's already — this is the one the adapter writes itself.
   */
  clock?: Clock;
  streaming?: RestStreamOptions;
}

export function resolveStreamOptions(
  options?: RestStreamOptions,
): Required<RestStreamOptions> {
  return {
    pollIntervalMs:
      options?.pollIntervalMs ?? DEFAULT_STREAM_OPTIONS.pollIntervalMs,
    heartbeatIntervalMs:
      options?.heartbeatIntervalMs ??
      DEFAULT_STREAM_OPTIONS.heartbeatIntervalMs,
    retryHintMs: options?.retryHintMs ?? DEFAULT_STREAM_OPTIONS.retryHintMs,
    // Clamped, not trusted: a batch size of 0 asks the store for nothing on
    // every read, and a stream that silently delivers no events is a worse
    // failure than one that ignores a nonsensical setting. `NaN` and the
    // infinities are checked BEFORE the clamp rather than caught by it —
    // `Math.max(1, NaN)` is `NaN`, so a value that arrived from a parsed
    // config string would otherwise walk straight through the guard and turn
    // every read into a `LIMIT NaN`.
    readBatchSize: clampBatchSize(options?.readBatchSize),
  };
}

/**
 * A usable read batch size: the default for anything that is not a finite
 * number, and at least one for everything else.
 *
 * `Infinity` falls back rather than clamping up, for the same reason `NaN`
 * does — it is a setting nobody meant, and "read the entire log in one call"
 * is the failure the batch size exists to prevent.
 */
function clampBatchSize(value: number | undefined): number {
  if (value === undefined || !Number.isFinite(value)) {
    return DEFAULT_STREAM_OPTIONS.readBatchSize;
  }
  return Math.max(1, Math.floor(value));
}
