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
import type {
  ApplyOutcome,
  ApplyProposalRequest,
  ApproveProposalInput,
  AssistantStore,
  AuthorizationPort,
  Logger,
  ProposalRecord,
  RejectProposalInput,
  SubmitMessageInput,
  SubmitMessageResult,
  ToolCatalog,
} from "@agentkit/host";
import type { RestCorsOptions } from "./cors.js";

/** `TurnRunner`, narrowed to the one call `submitMessage` makes. */
export interface TurnSubmitter {
  submitMessage(input: SubmitMessageInput): Promise<SubmitMessageResult>;
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
  logger?: Logger;
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
