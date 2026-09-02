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
 * A standing "yes" for one `(chat, tool, proposal kind[, scope])` combination,
 * up to a risk ceiling.
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
  /**
   * What this grant is scoped to — the proposal's `scopeKey`, i.e. the thing
   * being written. See {@link AutoApplyQuery.scopeKey}.
   */
  scopeKey?: string;
  /** Highest risk covered; rank N covers everything ≤ N. */
  maxRisk: RiskLevel;
  createdAt: string;
}

export interface WriteAllowanceInput {
  chatId: string;
  toolName: string;
  proposalKind: string;
  /**
   * Confine the grant to ONE scope. Absent, it covers every scope the tool can
   * reach from this chat — which is what a grant meant before this field
   * existed, and remains the meaning of one recorded without it.
   */
  scopeKey?: string;
  maxRisk: RiskLevel;
}

export interface AutoApplyQuery {
  chatId: string;
  toolName: string;
  proposalKind: string;
  /**
   * The scope the staged proposal actually writes to.
   *
   * It matters because the scope comes from MODEL-SUPPLIED input
   * (`ProposalBuilderToolOptions.scopeKeyOf` derives it from the tool call), so
   * without it a "yes, edit this document" answered about document A is a
   * standing yes for the same tool writing document B. An allowance recorded
   * WITHOUT a `scopeKey` still matches any scope — that is what every grant
   * given before this field existed meant, and silently narrowing them would
   * turn working auto-apply into a wall of confirmations.
   */
  scopeKey?: string;
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
