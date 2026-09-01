import type { AiContextBinding, AiToolDefinition } from "@agentkit/contracts";

/** What a guard is shown about the tool it is being asked to judge. */
export interface ToolGuardContext {
  /** Absent when the question is chat-independent (`ToolCatalog.listTools()`). */
  chatId?: string;
  /** The run's resolved bindings — empty when there is no chat. */
  bindings: readonly AiContextBinding[];
  /** The namespace of the contributor that offered {@link tool}. */
  namespace: string;
  tool: AiToolDefinition;
}

/** A refusal must say why: the reason is fed back to the model verbatim. */
export type ToolGuardVerdict =
  | { allowed: true }
  | { allowed: false; reason: string };

/**
 * A policy that can hide a tool from the model, or refuse a call to it.
 *
 * The two hooks answer genuinely different questions and are checked at
 * different moments, which is why they are not one:
 *
 * - `isVisible` runs at REGISTRY STAGING. A tool it hides is never advertised —
 *   it is not in the registry, so the provider is never shown it and the model
 *   cannot call it. This is the hook for "this deployment does not have that
 *   feature": the cheapest refusal is the one the model never has to reason
 *   about, and an advertised-but-always-refused tool wastes context on every
 *   turn and invites the model to keep trying.
 * - `canExecute` runs at CALL TIME, on a tool that was advertised. This is the
 *   hook for state that moves within a run — a lock taken, a budget spent, a
 *   binding that went stale since staging. A refusal becomes a failed tool
 *   result carrying `phase: "guard"` and the reason; it never throws the run.
 *
 * Guards compose with AND, and an absent hook is "no opinion", not "allow":
 * a guard that only implements `isVisible` has nothing to say about execution.
 * Order is not significant — every guard is asked, and the first refusal wins,
 * so a guard must not depend on running before or after another one.
 */
export interface ToolGuard {
  /** False hides the tool from the staged registry entirely. */
  isVisible?(ctx: ToolGuardContext): boolean | Promise<boolean>;
  /** A refusal fails this one call, with the reason shown to the model. */
  canExecute?(
    ctx: ToolGuardContext,
  ): ToolGuardVerdict | Promise<ToolGuardVerdict>;
}
