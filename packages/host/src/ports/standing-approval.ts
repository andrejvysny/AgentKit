import type { ToolGuardContext } from "./tool-guard.js";

/**
 * What a standing approval would cover, for ONE call.
 *
 * The digest is the whole identity: two calls with the same digest are the same
 * thing as far as a remembered approval is concerned, so a policy must fold in
 * everything that makes a call safe or unsafe to repeat (session, tool, working
 * directory, exact command, environment identity), not only the arguments.
 */
export interface StandingApprovalKey {
  /** Lowercase hex sha-256. */
  digest: string;
  /** Human summary shown where the human chooses to remember, at most 512 chars. */
  label: string;
}

/** One approvable refusal the call hit, as `canExecute` reported it. */
export interface StandingApprovalRefusal {
  reason: string;
}

/**
 * Decides whether a call may be covered by a durable, session-scoped standing
 * approval, and under which key.
 *
 * Standing approvals only ever REPLACE a fresh human approval of the exact same
 * call. They never override a final refusal (denial wins is evaluated first)
 * and they are looked up only after the one-use per-call approval has failed.
 */
export interface StandingApprovalPolicy {
  /**
   * `null` = this call can never be covered by a standing approval; the human
   * must approve it fresh. THROWING is also `null`: a policy that could not
   * answer has not said "remember this".
   */
  keyFor(
    ctx: ToolGuardContext,
    refusals: readonly StandingApprovalRefusal[],
  ): Promise<StandingApprovalKey | null>;
}

/** A remembered approval, stored on the session's chat metadata. */
export interface StandingApprovalGrant {
  /** `grant:<requestId>` — one grant per approval request, so a replay is idempotent. */
  grantId: string;
  digest: string;
  label: string;
  toolName: string;
  /** The human request whose approval created this grant. */
  sourceRequestId: string;
  /** The answer id that approved it. */
  answerId: string;
  grantedAt: string;
  /** Set once, when the grant is revoked. A revoked grant never matches. */
  revokedAt?: string;
}
