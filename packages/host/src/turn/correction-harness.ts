import type { AiChatMessage } from "@agentkit/contracts";

/**
 * Opt-in multi-pass correction over `VerificationHook`.
 *
 * The single-shot check answers "did the work land?" and then stops, which
 * leaves the interesting half undone: the model is the only thing that can fix
 * what the verifier found, and it never gets told. The harness closes that loop
 * — verify, hand the deficiencies back, let the model try again with its tools,
 * verify again — under three bounds that keep it from becoming an unbounded
 * spend of somebody's budget:
 *
 * - a **pass cap** ({@link resolveMaxCorrectionPasses}), so the worst case costs
 *   a known number of provider calls;
 * - **shrink-or-stall** ({@link shouldRunCorrectionPass}), so a model that is
 *   not actually making progress stops being asked;
 * - **fail-closed** — a verifier that throws or answers `null` mid-harness is
 *   `"unavailable"`, never a pass, and the harness stops rather than assume.
 *
 * It is OFF unless the host asks for it: absent `TurnRunnerDeps.correction`,
 * `TurnRunner` makes exactly the one `verify()` call it always did and writes
 * nothing new to the log.
 *
 * This module is deliberately pure — decisions and message-building, no I/O — so
 * the stopping rule is testable without a provider, a store, or a clock.
 */
export interface CorrectionConfig {
  /**
   * How many CORRECTION passes may follow the run's own answer. Default
   * {@link DEFAULT_CORRECTION_MAX_PASSES}; clamped to
   * {@link CORRECTION_MAX_PASSES_CAP} because the number is a spend limit and a
   * host that types 100 into it means something it has not thought through.
   * Zero is legal and means "verify, report on the log, correct nothing".
   */
  maxPasses?: number;
}

/** Correction passes allowed when {@link CorrectionConfig.maxPasses} is absent. */
export const DEFAULT_CORRECTION_MAX_PASSES = 3;

/** The hard ceiling on {@link CorrectionConfig.maxPasses}, whatever a host asks for. */
export const CORRECTION_MAX_PASSES_CAP = 5;

export function resolveMaxCorrectionPasses(config: CorrectionConfig): number {
  const requested = config.maxPasses ?? DEFAULT_CORRECTION_MAX_PASSES;
  if (!Number.isFinite(requested) || requested <= 0) return 0;
  return Math.min(Math.floor(requested), CORRECTION_MAX_PASSES_CAP);
}

export interface CorrectionPassDecision {
  /** The status the verifier just reported. `"pass"` ends the harness. */
  status: "pass" | "partial";
  /** The deficiencies it just reported. */
  deficiencies: readonly string[];
  /**
   * What the PREVIOUS verification reported, or `undefined` for the first one
   * (the run's own answer), which has nothing to be compared against and is
   * therefore always allowed to try a correction.
   */
  previousDeficiencies: readonly string[] | undefined;
  /** Correction passes already run. 0 while deciding whether to run the first. */
  passesRun: number;
  /** The resolved cap; see {@link resolveMaxCorrectionPasses}. */
  maxPasses: number;
}

/**
 * The stopping rule, in one place.
 *
 * SHRINK-OR-STALL. A correction pass is worth paying for only when the LAST one
 * bought something, and the only evidence available is the size of the failing
 * set: continue when the new report lists strictly FEWER deficiencies than the
 * one before it, stop otherwise. Equal counts stall — a model that reports the
 * same three problems twice is not converging on them, and asking a third time
 * buys a third identical answer. A growing count stalls for the stronger reason:
 * the correction is making things worse.
 *
 * Deliberately a COUNT and not a set-difference. Deficiency lines are free-form
 * host text; "the same problem, worded differently" and "a different problem"
 * are indistinguishable to this layer, so a subset test would read a reworded
 * line as progress and loop on it. Counting cannot be fooled that way, and its
 * failure mode — stopping one pass early when the model genuinely swapped one
 * deficiency for another — costs a pass rather than a budget.
 *
 * An empty `deficiencies` list on a `"partial"` report also stops: there is
 * nothing to write back, so a correction pass would be a re-ask with no
 * instruction in it.
 */
export function shouldRunCorrectionPass(
  input: CorrectionPassDecision,
): boolean {
  if (input.status !== "partial") return false;
  if (input.deficiencies.length === 0) return false;
  if (input.passesRun >= input.maxPasses) return false;
  if (input.previousDeficiencies === undefined) return true;
  return input.deficiencies.length < input.previousDeficiencies.length;
}

/**
 * The deficiency write-back — the one user-role message a correction pass adds.
 *
 * A FIXED template, listing the host's lines verbatim. The framework does not
 * paraphrase, rank or summarise a deficiency: those lines are the host's
 * domain-truth, and a wrapper that reworded them would be putting words in the
 * verifier's mouth on the way to the model that has to act on them.
 */
export function buildDeficiencyWriteBack(
  deficiencies: readonly string[],
): string {
  return [
    `Your last attempt was verified and did not fully land. ${deficiencies.length} item(s) are still unresolved:`,
    ...deficiencies.map((line) => `- ${line}`),
    "",
    "Fix each of these now by calling your tools — do not answer with a plan or a description of what should be done. Leave the parts that already succeeded alone, and when you are finished say briefly what you changed.",
  ].join("\n");
}

export interface CorrectionMessagesInput {
  /** The chat's system prompt, or `null` when the host has none. */
  systemPrompt: string | null;
  /** The visible answer the previous pass finished with. */
  previousContent: string;
  /** {@link buildDeficiencyWriteBack} of the deficiencies being corrected. */
  writeBack: string;
}

/**
 * MINIMAL RE-CONTEXT: the system prompt, the previous pass's visible answer, and
 * the write-back. Nothing else.
 *
 * The obvious alternative — replay the whole conversation and append the
 * write-back — is what makes a correction harness unaffordable. The run that
 * just happened is the expensive one: its history already carries every tool
 * call and every tool result, and a correction pass that replays all of it pays
 * for the entire turn again on every attempt, growing with each one, precisely
 * when the model needs to be looking at a short list of specific problems.
 *
 * What the three messages preserve is what the model actually needs: who it is
 * (the system prompt), what it just claimed to have done (its own answer), and
 * what is wrong with it (the write-back). The tools are staged exactly as they
 * were, so it can re-read anything it needs from the domain rather than from a
 * transcript — which is also the more honest source, since the deficiencies were
 * found in the domain and not in the transcript.
 *
 * The assistant message is omitted when the previous pass produced no visible
 * text: an empty assistant turn is a shape several providers reject outright,
 * and it carries nothing the write-back does not already say.
 */
export function buildCorrectionMessages(
  input: CorrectionMessagesInput,
): AiChatMessage[] {
  const messages: AiChatMessage[] = [];
  if (input.systemPrompt) {
    messages.push({ role: "system", content: input.systemPrompt });
  }
  if (input.previousContent.trim().length > 0) {
    messages.push({ role: "assistant", content: input.previousContent });
  }
  messages.push({ role: "user", content: input.writeBack });
  return messages;
}
