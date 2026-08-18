import { InvalidProposalTransitionError } from "../errors.js";
import type { ProposalStatus } from "../ports/proposal-store.js";

/**
 * The ONLY legal proposal status changes.
 *
 * Read it as the safety argument it is:
 * - `pending` is the only state a decision can be made in — approving something
 *   already applied, or rejecting something mid-apply, would be a decision about
 *   a world that no longer exists.
 * - `approved → applying` is the claim. It is a separate state from `approved`
 *   so exactly one worker can take the work.
 * - `applying → applied | failed` is the only exit, and both require an outcome
 *   to have been recorded first.
 * - Terminals are terminal. Re-running a proposal means staging a new one.
 *
 * Same-state transitions are ILLEGAL here, unlike in a task queue where a
 * re-delivered "start" is harmless: `applying → applying` would be a second
 * apply of side effects that already happened, which is precisely the bug the
 * operation-id machinery exists to prevent.
 */
export const PROPOSAL_TRANSITIONS: Readonly<
  Record<ProposalStatus, readonly ProposalStatus[]>
> = Object.freeze({
  pending: Object.freeze(["approved", "rejected", "invalidated"] as const),
  approved: Object.freeze(["applying"] as const),
  applying: Object.freeze(["applied", "failed"] as const),
  applied: Object.freeze([] as const),
  failed: Object.freeze([] as const),
  rejected: Object.freeze([] as const),
  invalidated: Object.freeze([] as const),
});

/** Every status, for exhaustive iteration (tests, UI filters). */
export const PROPOSAL_STATUSES: readonly ProposalStatus[] = Object.freeze([
  "pending",
  "approved",
  "applying",
  "applied",
  "failed",
  "rejected",
  "invalidated",
] as const);

/** True when `from → to` is in {@link PROPOSAL_TRANSITIONS}. */
export function isProposalTransitionAllowed(
  from: ProposalStatus,
  to: ProposalStatus,
): boolean {
  return PROPOSAL_TRANSITIONS[from].includes(to);
}

/**
 * Throw unless `from → to` is legal. Every store adapter and the
 * {@link ProposalService} call this, so the rules cannot drift between them.
 */
export function assertProposalTransition(
  from: ProposalStatus,
  to: ProposalStatus,
): void {
  if (!isProposalTransitionAllowed(from, to)) {
    throw new InvalidProposalTransitionError(
      `Illegal proposal transition ${from} → ${to}.`,
      { from, to, allowed: PROPOSAL_TRANSITIONS[from] },
    );
  }
}
