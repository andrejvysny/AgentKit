import type { RiskLevel } from "../ports/proposal-store.js";
import { defaultClock, type Clock } from "../ports/system.js";
import type {
  AutoApplyQuery,
  WriteAllowance,
  WriteAllowanceInput,
  WritePolicy,
  WritePolicyMode,
} from "../ports/write-policy.js";

/**
 * Risk ordering. An allowance at rank N covers every proposal at rank ≤ N, so
 * "yes to destructive edits here" implies "yes to low-risk ones" but never the
 * reverse.
 */
export const RISK_RANK: Readonly<Record<RiskLevel, number>> = Object.freeze({
  low: 0,
  medium: 1,
  high: 2,
  destructive: 3,
});

/** The key an allowance is stored under: one grant per chat + tool + kind. */
export function writeAllowanceKey(
  chatId: string,
  toolName: string,
  proposalKind: string,
): string {
  return `${chatId}:${toolName}:${proposalKind}`;
}

export interface SessionWritePolicyOptions {
  mode?: WritePolicyMode;
  clock?: Clock;
}

/**
 * In-memory {@link WritePolicy}: allowances live for the life of the process and
 * are never persisted.
 *
 * That is the security property, not a limitation. "Yes, go ahead" is consent
 * given in a conversation the user is watching; persisting it would silently
 * carry that consent into a session they are not, where the same grant now
 * covers writes they never saw proposed. A host that genuinely wants standing
 * grants should implement its own persisted policy and be explicit about it.
 */
export class SessionWritePolicy implements WritePolicy {
  private readonly allowances = new Map<string, WriteAllowance>();
  private currentMode: WritePolicyMode;
  private readonly clock: Clock;

  constructor(options: SessionWritePolicyOptions = {}) {
    this.currentMode = options.mode ?? "auto_readonly_confirm_writes";
    this.clock = options.clock ?? defaultClock;
  }

  mode(): WritePolicyMode {
    return this.currentMode;
  }

  /** Switch posture at runtime (a settings change mid-session). */
  setMode(mode: WritePolicyMode): void {
    this.currentMode = mode;
  }

  isAutoApplyAllowed(query: AutoApplyQuery): boolean {
    // `confirm_all_writes` overrides every allowance rather than clearing them:
    // turning confirmation back off must not silently re-arm grants the user
    // gave before, and turning it on must take effect immediately.
    if (this.currentMode === "confirm_all_writes") return false;
    if (this.currentMode === "auto_all") return true;
    const allowance = this.allowances.get(
      writeAllowanceKey(query.chatId, query.toolName, query.proposalKind),
    );
    if (!allowance) return false;
    return RISK_RANK[query.risk] <= RISK_RANK[allowance.maxRisk];
  }

  allow(input: WriteAllowanceInput): WriteAllowance {
    const key = writeAllowanceKey(
      input.chatId,
      input.toolName,
      input.proposalKind,
    );
    const allowance: WriteAllowance = {
      key,
      chatId: input.chatId,
      toolName: input.toolName,
      proposalKind: input.proposalKind,
      maxRisk: input.maxRisk,
      createdAt: this.clock.nowIso(),
    };
    this.allowances.set(key, allowance);
    return allowance;
  }

  /**
   * Revoke by key, scoped to the chat that owns it — one chat cannot revoke
   * another's grant by guessing a key.
   */
  revoke(chatId: string, key: string): void {
    const allowance = this.allowances.get(key);
    if (allowance?.chatId === chatId) this.allowances.delete(key);
  }

  list(chatId: string): WriteAllowance[] {
    return [...this.allowances.values()].filter(
      (allowance) => allowance.chatId === chatId,
    );
  }
}
