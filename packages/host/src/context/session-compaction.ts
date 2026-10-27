/**
 * Applying and creating context summaries for one agent-session pass
 * (ADR 0017 part 2). Called by `TurnRunner.assembleSessionHistory` between
 * transcript assembly and M3a trimming.
 *
 * The flow: pick the newest summary that still matches the assembled path and
 * substitute it for the covered prefix; when the history is over budget (or a
 * compaction was requested) summarize once more and save the result. Every
 * failure degrades to the caller's trimming and is reported as a
 * `compaction_failed` warning; nothing here can fail the turn except an abort.
 */
import type { AiChatMessage, AiToolLimits } from "@agentkit/contracts";
import type {
  AiProviderClient,
  AiRunEventDraft,
  TokenEstimator,
} from "@agentkit/core";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { ContextSummaryRecord } from "../ports/context-summary-store.js";
import type { Clock } from "../ports/system.js";
import {
  COMPACT_REQUESTED_METADATA_KEY,
  type AgentSessionDescriptor,
} from "../sessions/descriptor.js";
import {
  DEFAULT_KEEP_RECENT_TOKENS,
  applyValidSummary,
  compactSessionContext,
  type AppliedSummary,
} from "./compactor.js";

export interface SessionCompactionInput {
  store: AssistantStore;
  clock: Clock;
  estimator: TokenEstimator;
  session: AgentSessionDescriptor;
  chatId: string;
  taskId: string;
  client: AiProviderClient;
  model: string;
  providerId: string;
  limits: AiToolLimits;
  signal: AbortSignal;
  /** The assembled transcript, system message first when there is one. */
  messages: AiChatMessage[];
  /** Record id per non-system message. */
  recordIds: string[];
  windowTokens: number;
  reserveTokens: number;
  /** Token budget for the history (window - reserve - tools - run content). */
  budgetTokens: number;
  emitWarning(code: string, message: string): Promise<void>;
  appendEvent(draft: AiRunEventDraft): Promise<void>;
}

function totalTokens(
  messages: readonly AiChatMessage[],
  estimator: TokenEstimator,
): number {
  return messages.reduce(
    (sum, message) => sum + estimator.estimateMessage(message),
    0,
  );
}

async function clearCompactRequest(
  store: AssistantStore,
  chatId: string,
): Promise<void> {
  const chat = await store.conversations.getChat(chatId);
  if (chat === null || !(COMPACT_REQUESTED_METADATA_KEY in chat.metadata))
    return;
  await store.conversations.updateChat(chatId, {
    metadata: Object.fromEntries(
      Object.entries(chat.metadata).filter(
        ([key]) => key !== COMPACT_REQUESTED_METADATA_KEY,
      ),
    ),
  });
}

/**
 * The messages a session pass should be trimmed from: the assembled history
 * with the newest valid summary substituted for its covered prefix, compacted
 * first when over budget.
 */
export async function applySessionSummaries(
  input: SessionCompactionInput,
): Promise<AiChatMessage[]> {
  const { store, estimator } = input;
  const summaries = store.summaries;
  if (summaries === undefined) return input.messages;

  const stored = await summaries.listSummaries(input.chatId);
  let previous: ContextSummaryRecord | undefined;
  let working: AppliedSummary = {
    messages: input.messages,
    recordIds: input.recordIds,
  };
  for (const candidate of stored) {
    const applied = await applyValidSummary(
      input.messages,
      input.recordIds,
      candidate,
    );
    if (applied === undefined) continue;
    previous = candidate;
    working = applied;
    break;
  }

  const chat = await store.conversations.getChat(input.chatId);
  const manual =
    typeof chat?.metadata[COMPACT_REQUESTED_METADATA_KEY] === "string";
  if (!manual && totalTokens(working.messages, estimator) <= input.budgetTokens)
    return working.messages;

  const compactionNumber = stored.length + 1;
  const outcome = await compactSessionContext({
    sessionId: input.chatId,
    messages: input.messages,
    recordIds: input.recordIds,
    client: input.client,
    model: input.model,
    providerId: input.providerId,
    estimator,
    windowTokens: input.windowTokens,
    reserveTokens: input.reserveTokens,
    keepRecentTokens:
      input.session.context?.keepRecentTokens ?? DEFAULT_KEEP_RECENT_TOKENS,
    limits: input.limits,
    ...(previous === undefined ? {} : { previous }),
    signal: input.signal,
    taskId: input.taskId,
    compactionNumber,
    summaryId: `${input.taskId}:compaction:${compactionNumber}`,
    nowIso: input.clock.nowIso(),
  });

  if (outcome === undefined) {
    // Nothing older than the kept tail: a manual request is satisfied, not retried forever.
    if (manual) await clearCompactRequest(store, input.chatId);
    return working.messages;
  }
  if (!outcome.ok) {
    await input.emitWarning(
      "compaction_failed",
      `Context compaction failed (${outcome.reason}); the history was trimmed instead.`,
    );
    return working.messages;
  }

  const record = outcome.record;
  await store.transaction(async (tx) => {
    if (tx.summaries !== undefined) await tx.summaries.saveSummary(record);
    if (manual) await clearCompactRequest(tx, input.chatId);
  });
  await input.appendEvent({
    type: "run.context.compacted",
    runId: input.taskId,
    timestamp: input.clock.nowIso(),
    data: {
      summaryId: record.summaryId,
      tokensBefore: record.tokensBefore,
      tokensAfter: record.tokensAfter,
      coveredMessages: outcome.coveredMessages,
      reason: manual ? "manual" : "threshold",
    },
  });
  for (const usage of outcome.usage) {
    await input.appendEvent({
      type: "run.usage",
      runId: input.taskId,
      timestamp: usage.timestamp,
      data: { ...usage.data, step: 0 },
    });
  }
  const applied = await applyValidSummary(
    input.messages,
    input.recordIds,
    record,
  );
  return applied === undefined ? working.messages : applied.messages;
}
