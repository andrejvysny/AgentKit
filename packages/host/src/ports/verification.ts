/**
 * Post-run "did the work actually land?" check.
 *
 * A run that called tools and narrated success can still have achieved nothing —
 * the most confusing failure mode there is, because every visible signal says it
 * worked. A host that can inspect its own domain plugs the check in here; the
 * checks themselves are the host's business and no part of this framework.
 */
export interface VerificationCheck {
  id: string;
  ok: boolean;
  message?: string;
}

/**
 * `status` is deliberately two-valued. A verifier reports either "everything I
 * checked passed" or "some of it did not"; anything finer (how bad, what to do)
 * belongs in `deficiencies`, which is what a human and a model both read.
 */
export interface DeficiencyReport {
  status: "pass" | "partial";
  checks: VerificationCheck[];
  /** Human/model-facing lines describing what is still wrong. */
  deficiencies: string[];
}

export interface VerificationInput {
  runId: string;
  chatId: string;
  scopeId?: string;
  attemptId?: string;
  /** Tool calls the run actually made; zero means there is nothing to verify. */
  toolCallCount: number;
  /** The answer as it will be persisted. */
  finalContent: string;
  signal?: AbortSignal;
}

export interface VerificationHook {
  /** Null when the hook has nothing to say about this run. */
  verify(input: VerificationInput): Promise<DeficiencyReport | null>;
}
