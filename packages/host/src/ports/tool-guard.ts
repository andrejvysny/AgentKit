import type { AiContextBinding, AiToolDefinition } from "@agentkit/contracts";

/** What a guard is shown about the tool it is being asked to judge. */
export interface ToolGuardContext {
  /** Absent when the question is chat-independent (`ToolCatalog.listTools()`). */
  chatId?: string;
  /**
   * The run's resolved bindings — empty when there is no chat.
   *
   * A STAGING-TIME SNAPSHOT, even in `canExecute`. The context object is built
   * once, when the registry is staged, and the same one is handed to every
   * later call: it is what the run was bound to when its tool set was decided,
   * not what it is bound to now. That is the right input for `isVisible`, which
   * IS a staging-time question. A `canExecute` guard whose verdict turns on
   * state that moves within a run — a binding swapped, a lock taken, a budget
   * spent — must re-read that state itself, from whatever owns it; reading it
   * here would be reading a photograph.
   */
  bindings: readonly AiContextBinding[];
  /** The namespace of the contributor that offered {@link tool}. */
  namespace: string;
  tool: AiToolDefinition;
  /**
   * WHO the tools are being staged for, when the caller knows.
   *
   * Absent on the turn-runner path, where a run is already attributed by its
   * chat and its task. Set by call paths that serve a named caller — today
   * `@agentkit/mcp-server`, whose session scope resolves one at `initialize`
   * and threads it here so a guard can answer "may THIS principal see/run this"
   * rather than only "does this chat have the binding".
   */
  principal?: string;
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
  /**
   * False hides the tool from the staged registry entirely.
   *
   * THROWING ALSO HIDES IT (with a warning on the host's logger): a hook that
   * could not answer has not allowed anything. The failure is scoped to the one
   * tool being judged, so a guard broken for a single tool costs that tool and
   * not the run's whole tool set.
   */
  isVisible?(ctx: ToolGuardContext): boolean | Promise<boolean>;
  /**
   * A refusal fails this one call, with the reason shown to the model.
   *
   * THROWING IS A REFUSAL, reported to the model as `phase: "guard"` with the
   * fixed reason `"guard error"` — the thrown message is deliberately not
   * forwarded, because a guard's reason goes to the model verbatim.
   */
  canExecute?(
    ctx: ToolGuardContext,
  ): ToolGuardVerdict | Promise<ToolGuardVerdict>;
}
