/**
 * The reasoning-effort levels a turn may request. Mirrors
 * `RunReasoningEffortSchema` in `@agentkit/contracts` (`prompt.ts`), which is
 * not re-exported from the package barrel — keep the two lists in step.
 */
export const REASONING_EFFORTS = ["minimal", "low", "medium", "high"] as const;
export type SessionReasoningEffort = (typeof REASONING_EFFORTS)[number];
