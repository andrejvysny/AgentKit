import type { AiProviderContinuation } from "@agentkit/contracts";
import type { RunWriteFence } from "./conversation-store.js";

export type ProviderContinuationScope = AiProviderContinuation["scope"];

/** Trusted provider state; never part of message metadata, events or REST DTOs. */
export interface ProviderContinuationRecord {
  runId: string;
  anchorMessageId: string;
  state: AiProviderContinuation;
}

/** Optional private persistence for lossless, branch-bound provider continuation. */
export interface ProviderContinuationStore {
  /** Insert once, or require an exact match with the original run identity. */
  bindRun(
    runId: string,
    scope: ProviderContinuationScope,
    fence: RunWriteFence,
  ): Promise<void>;
  getRunScope(runId: string): Promise<ProviderContinuationScope | null>;
  /** One latest state per run, persisted before its tool calls are dispatched. */
  put(
    input: ProviderContinuationRecord,
    fence: RunWriteFence,
  ): Promise<ProviderContinuationRecord>;
  /** Null before any successful save; missing previously saved state throws provider_continuation_state_missing. */
  getByRun(runId: string): Promise<ProviderContinuationRecord | null>;
  /** Input-anchor order, then run ID; at most 1024 anchor IDs. */
  findByAnchors(
    chatId: string,
    anchorMessageIds: readonly string[],
  ): Promise<ProviderContinuationRecord[]>;
}
