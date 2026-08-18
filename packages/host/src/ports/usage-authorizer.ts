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

/** One provider call's settled token accounting (mirrors `run.usage`). */
export interface UsageRecord {
  runId: string;
  callId: string;
  attempt: number;
  providerId: string;
  model: string;
  promptTokens?: number;
  completionTokens?: number;
  totalTokens?: number;
  at: string;
}

export interface UsageAuthorizer {
  authorize(
    request: UsageAuthorizationRequest,
  ): Promise<UsageAuthorizationDecision>;
  record(usage: UsageRecord): Promise<void>;
}
