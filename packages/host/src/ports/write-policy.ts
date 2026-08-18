import type { RiskLevel } from "./proposal-store.js";

/**
 * How much a chat may write without asking.
 *
 * - `auto_readonly_confirm_writes` — the default: reads run freely, writes stage
 *   a proposal and wait, unless the user granted a standing allowance for that
 *   tool + kind.
 * - `confirm_all_writes` — no allowance is ever honoured; every write waits.
 * - `auto_all` — everything auto-applies. For trusted, fully-undoable hosts.
 */
export type WritePolicyMode =
  | "auto_readonly_confirm_writes"
  | "confirm_all_writes"
  | "auto_all";

/**
 * A standing "yes" for one `(chat, tool, proposal kind)` combination, up to a
 * risk ceiling.
 *
 * The ceiling is what keeps the grant honest: a user who approved low-risk edits
 * for this tool has not thereby approved a destructive one, and a model cannot
 * escalate by re-labelling its own proposal.
 */
export interface WriteAllowance {
  key: string;
  chatId: string;
  toolName: string;
  proposalKind: string;
  /** Highest risk covered; rank N covers everything ≤ N. */
  maxRisk: RiskLevel;
  createdAt: string;
}

export interface WriteAllowanceInput {
  chatId: string;
  toolName: string;
  proposalKind: string;
  maxRisk: RiskLevel;
}

export interface AutoApplyQuery {
  chatId: string;
  toolName: string;
  proposalKind: string;
  risk: RiskLevel;
}

/**
 * Decides whether a staged write applies immediately or waits for a human.
 *
 * Synchronous by design: it is consulted inside a write tool's execution, on the
 * hot path of a model turn, and an answer that could block on IO would be an
 * answer that can time out — turning "needs confirmation" into "tool failed".
 */
export interface WritePolicy {
  mode(): WritePolicyMode;
  isAutoApplyAllowed(query: AutoApplyQuery): boolean;
  allow(input: WriteAllowanceInput): WriteAllowance;
  revoke(chatId: string, key: string): void;
  list(chatId: string): WriteAllowance[];
}
