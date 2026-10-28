import type {
  AiChatMessage,
  AiContextBinding,
  AiToolLimits,
} from "@agentkit/contracts";
import type {
  AiChatRequest,
  AiProviderClient,
  AiReasoningEffort,
} from "../providers/client.js";
import type { AiToolRegistry } from "../tools/registry.js";
import type { ExecutionBudget, ExecutionBudgets } from "./execution-budget.js";
import type { ProviderRetryPolicy } from "./provider-retry.js";

export interface RunChatInput
  extends Pick<
    AiChatRequest,
    | "continuation"
    | "continuationScope"
    | "continuationRequired"
    | "onContinuation"
    | "runContent"
  > {
  budgets?: ExecutionBudgets;
  executionBudget?: ExecutionBudget;
  client: AiProviderClient;
  registry: AiToolRegistry;
  model: string;
  /** Conversation so far. Treated as read-only: the loop works on a copy. */
  messages: readonly AiChatMessage[];
  bindings?: AiContextBinding[];
  limits: AiToolLimits;
  chatId?: string;
  /**
   * The serialization/idempotency scope this run writes to, passed through
   * verbatim to every tool's {@link AiToolExecutionContext}. The loop does not
   * interpret it: only the host knows whether a run is scoped on its chat or on
   * the document several chats share.
   */
  scopeId?: string;
  userId?: string;
  temperature?: number;
  maxOutputTokens?: number;
  reasoningEffort?: AiReasoningEffort;
  maxToolIterations?: number;
  maxToolCallsPerIteration?: number;
  /**
   * Deadline applied to any tool whose definition sets no `timeoutMs` of its
   * own. Undefined (the default) keeps the historical behaviour: such tools run
   * unbounded. A host that cannot afford a hung local tool to hold its lease
   * forever sets this once for the whole run.
   */
  defaultToolTimeoutMs?: number;
  signal?: AbortSignal;
  /**
   * Internal seam (set by `boundedRun`): wraps a provider client in the
   * budget instrumentation. The agent loop applies it to every client it
   * dispatches on — including clients resolved by model routing — so a routed
   * provider request consumes the SAME budget as the run's own. Never set by
   * consumers.
   */
  wrapClient?: (client: AiProviderClient) => AiProviderClient;
  /**
   * Opt-in bounded retry of a model request after a transient provider failure
   * that produced no visible output. Absent: behaviour and events unchanged.
   */
  providerRetry?: ProviderRetryPolicy;
  /** Override runId for deterministic tests. */
  runId?: string;
  /**
   * First `seq` stamped on this run's events. Resume a stream without breaking
   * the consumer's ordering key by passing the number after the last one already
   * delivered. Default 0.
   */
  firstSeq?: number;
  /** Stamped on every event; groups the events of one attempt of this run. */
  attemptId?: string;
}

/**
 * What the run produced, returned as the generator's return value (`for await`
 * discards it — see the `collectRun` pattern in the tests to capture it).
 *
 * This is an in-process API type, deliberately NOT part of `@agentkit/contracts`:
 * nothing here crosses the wire. The events are the wire format; this is the
 * handoff between `runChat` and whoever drove it.
 */
export interface RunChatResult {
  runId: string;
  /** Which terminal event ended the run. */
  terminal: "completed" | "failed" | "cancelled";
  /**
   * Assistant + tool messages the run appended, in order. Append these to your
   * own history to continue the conversation.
   */
  appendedMessages: readonly AiChatMessage[];
  /** Provider round-trips actually taken (1-based). */
  iterations: number;
}
