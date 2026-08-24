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
import type { AiToolDefinition } from "@agentkit/contracts";
import type {
  ApplyOutcome,
  ApplyProposalRequest,
  ApproveProposalInput,
  AssistantStore,
  Logger,
  ProposalRecord,
  RejectProposalInput,
  SubmitMessageInput,
  SubmitMessageResult,
} from "@agentkit/host";

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
 * SSE tuning. All three have defaults that suit a UI on a local network; a
 * deployment behind a proxy that kills idle connections lowers
 * `heartbeatIntervalMs`, and one with a slow store raises `pollIntervalMs`.
 */
export interface RestStreamOptions {
  /**
   * How long the stream waits before asking the event log again, once it has
   * caught up. The floor on end-to-end latency for a live token.
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
}

export const DEFAULT_STREAM_OPTIONS: Required<RestStreamOptions> =
  Object.freeze({
    pollIntervalMs: 150,
    heartbeatIntervalMs: 15_000,
    retryHintMs: 2_000,
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
   * What `listTools` advertises.
   *
   * Optional because `ToolSetContributor.contribute` is a per-RUN call — it
   * takes bindings, limits and a scope, all of which belong to a conversation
   * — and `GET /v1/tools` names no conversation. A host that can enumerate a
   * static catalogue passes one here; one that cannot leaves it out and the
   * route answers 501 rather than inventing a fake run context and advertising
   * tools no turn would actually get.
   */
  toolCatalog?(): Promise<AiToolDefinition[]>;
  /** Reported by `getVersion` as `packages`, for an identifiable build. */
  packages?: Record<string, string>;
  /**
   * Called before any route runs.
   *
   * Returning a `Response` short-circuits the request with it (401, a redirect,
   * whatever the host's scheme needs). Any other value is an opaque principal:
   * this adapter does not read it and threads it nowhere, because the contract
   * has no per-principal scoping yet and pretending otherwise would bake a
   * half-authorization into a published surface. A host needing that today
   * filters in front of the handler.
   */
  authenticate?(req: Request): Promise<unknown | Response>;
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
      options?.heartbeatIntervalMs ?? DEFAULT_STREAM_OPTIONS.heartbeatIntervalMs,
    retryHintMs: options?.retryHintMs ?? DEFAULT_STREAM_OPTIONS.retryHintMs,
  };
}
