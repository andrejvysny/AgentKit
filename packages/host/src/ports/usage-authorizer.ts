/**
 * Spend control around provider calls: ask before, report after.
 *
 * Two methods rather than one because the interesting failure is between them —
 * a run that was authorized and then burned ten times its estimate must still be
 * recorded, so the next authorization can say no.
 */
export interface UsageAuthorizationRequest {
  runId: string;
  chatId?: string;
  userId?: string;
  providerId: string;
  model: string;
  /** Best-effort estimate; a host may authorize on it or ignore it. */
  estimatedPromptTokens?: number;
}

export interface UsageAuthorizationDecision {
  allowed: boolean;
  reason?: string;
  remainingTokens?: number;
  /** When the caller may retry, for a quota that refills. */
  retryAfterMs?: number;
}

/**
 * One provider call's token accounting, mirroring the `run.usage` event that
 * produced it.
 *
 * EVERY usage event is reported, not only the settled ones: a streaming
 * provider emits interim numbers mid-call, and a recorder that never saw them
 * would lose the accounting for a call that died before it settled — exactly
 * the call a budget most needs to know about. {@link finalForCall} is what says
 * which is which, so a recorder can sum the settled ones and treat the rest as
 * a running estimate instead of double-counting them.
 *
 * `callId` + `attempt` identify the provider call the numbers belong to; the
 * interim reports and the final one for a call all share them.
 */
export interface UsageRecord {
  runId: string;
  callId: string;
  attempt: number;
  /**
   * The provider that produced the numbers: the id the emitting client put on
   * the event, falling back to the id the projector was told to bill against.
   * A routed run bills a switch to the provider it switched TO, not the one the
   * turn started on — the empty string means nobody named one, and the record
   * is kept anyway (see the note above).
   */
  providerId: string;
  model: string;
  promptTokens?: number;
  completionTokens?: number;
  totalTokens?: number;
  /**
   * Prompt tokens served from the provider's prompt cache, only when the
   * provider reported them — a recorder prices those below the uncached ones,
   * and inventing a zero would overbill a call the provider discounted.
   */
  cachedPromptTokens?: number;
  /**
   * Completion tokens spent on hidden reasoning, only when the provider
   * reported them. Some providers bill them at the output rate, so a recorder
   * that never sees them underbills every reasoning model it prices.
   */
  reasoningTokens?: number;
  /**
   * True when these are the call's settled numbers — the last word for this
   * `callId`. False for an interim report from a stream still in flight, which
   * a later record for the same `callId` supersedes rather than adds to.
   */
  finalForCall: boolean;
  /** Where the numbers came from: a stream chunk, or the completed response. */
  source?: "stream" | "response";
  /** The tool-loop step within the run, when the emitter tracks one. */
  step?: number;
  at: string;
}

export interface UsageAuthorizer {
  authorize(
    request: UsageAuthorizationRequest,
  ): Promise<UsageAuthorizationDecision>;
  record(usage: UsageRecord): Promise<void>;
}
