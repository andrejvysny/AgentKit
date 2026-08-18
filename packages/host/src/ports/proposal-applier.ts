import type { ApplyOutcome, ProposalRecord } from "./proposal-store.js";

export interface ApplyProposalInput {
  proposal: ProposalRecord;
  /** Idempotency key for THIS apply attempt. */
  operationId: string;
  signal?: AbortSignal;
}

/**
 * The host side of a write: the only component that actually changes the world.
 *
 * Everything else in the proposal pipeline is bookkeeping around this call, and
 * the bookkeeping is only sound if the applier keeps two promises:
 *
 * 1. `getOutcome(operationId)` answers for work that already happened, even
 *    across a process restart. Recovery cannot ask "did my write land?" any
 *    other way, and guessing means either a lost write or a duplicated one.
 * 2. An apply that partially lands reports `status: "partial"` with the failed
 *    operations, rather than throwing away the half that worked.
 */
export interface ProposalApplier {
  apply(input: ApplyProposalInput): Promise<ApplyOutcome>;
  /** The recorded outcome for an operation id, or null if it never ran. */
  getOutcome(operationId: string): Promise<ApplyOutcome | null>;
  /**
   * Current revision of a scope, when the host versions its state. Enables the
   * staleness check that invalidates proposals built against an older world.
   */
  currentRevision?(scopeKey: string): Promise<string | null>;
}
