/**
 * Driving {@link VerificationHook} across possibly several provider passes:
 * the multi-pass correction loop ({@link runCorrectionHarness}), the
 * fail-closed wrapper around one `verify()` call, and the deficiency-list
 * banner text. Split out of `turn-runner.ts` because it is the one piece of
 * `runTurn` that itself drives more passes, beside `correction-harness.ts`
 * (the pure stopping-rule logic this file calls).
 */
import type { AiRunEventDraft, AiToolRegistry } from "@agentkit/core";
import type {
  DeficiencyReport,
  VerificationHook,
  VerificationInput,
} from "../ports/verification.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";
import {
  buildCorrectionMessages,
  buildDeficiencyWriteBack,
  shouldRunCorrectionPass,
} from "./correction-harness.js";
import type { ResolvedHookTimeouts } from "./hook-deadline.js";
import { withHookDeadline } from "./hook-deadline.js";
import type { PassTerminal } from "./retry.js";
import type {
  PassInput,
  PassResult,
  PassState,
  TurnRunnerDeps,
} from "./turn-runner.js";

/**
 * Everything {@link runCorrectionHarness} needs from `TurnRunner` besides the
 * deps it already carries: the verify deadline, and the private methods it
 * shares with the rest of a run — running a provider pass, resetting pass
 * state, and appending to the durable log — kept as callbacks so this module
 * never depends on the class itself.
 */
export interface CorrectionHarnessContext {
  deps: TurnRunnerDeps;
  hookTimeouts: Pick<ResolvedHookTimeouts, "verify">;
  runPass(input: PassInput): Promise<PassResult>;
  resetPass(state: PassState): void;
  appendHostEvent(
    ctx: TaskExecutionContext,
    draft: AiRunEventDraft,
  ): Promise<void>;
  emitPassBoundary(
    ctx: TaskExecutionContext,
    pass: number,
    reason: "chat_only" | "empty_response" | "correction",
    message: string,
  ): Promise<void>;
}

/**
 * `verify()`, under its deadline, with a throw folded into the `null` answer.
 *
 * Only the harness calls this. The single-shot path deliberately lets a throw
 * out (a host wired that check before this existed and gets the failure it
 * always got); inside the harness a broken verifier must not take a run down
 * that already produced an answer, so the fault is logged, reported as
 * `"unavailable"` on the durable log, and the harness stops. A verifier that
 * runs past its deadline is the same case — "could not answer" — and takes
 * the same fail-closed path, which is why the timeout needs no branch here.
 */
async function verifyQuietly(
  context: CorrectionHarnessContext,
  verification: VerificationHook,
  input: VerificationInput,
): Promise<DeficiencyReport | null> {
  try {
    return await withHookDeadline({
      hook: "verify",
      timeoutMs: context.hookTimeouts.verify,
      run: () => verification.verify(input),
    });
  } catch (err) {
    context.deps.logger?.warn("verification hook failed", {
      taskId: input.runId,
      chatId: input.chatId,
      error: err instanceof Error ? err.message : String(err),
    });
    return null;
  }
}

/** One `run.verification` event on the run's durable log. */
async function emitVerification(
  context: CorrectionHarnessContext,
  ctx: TaskExecutionContext,
  pass: number,
  status: "pass" | "partial" | "unavailable",
  deficiencies: readonly string[],
): Promise<void> {
  await context.appendHostEvent(ctx, {
    type: "run.verification",
    runId: ctx.task.taskId,
    timestamp: context.deps.clock.nowIso(),
    data: { pass, status, deficiencies: [...deficiencies] },
  });
}

/**
 * The multi-pass correction harness: verify, hand the deficiencies back, let
 * the model fix them with its tools, verify again — bounded three ways.
 *
 * THE LOOP. Pass 0 verifies the run's own answer; each `run.verification`
 * event names its pass number, so a log reader can tell "verified once and it
 * was fine" from "corrected twice and it still is not". A correction pass is a
 * full {@link PassInput}-driven pass on the SAME registry and the same task
 * log: tools staged exactly as the run had them, `seq` continuing unbroken,
 * {@link TurnRunnerDeps.usage} asked before it and told after it. There is no
 * second code path for a correction pass, which is what makes "the harness
 * cannot bypass spend control" true by construction rather than by review.
 *
 * WHAT STOPS IT (see `correction-harness.ts` for the rule itself):
 * - `status: "pass"` — the work landed; nothing to correct.
 * - shrink-or-stall — the new deficiency list is not strictly shorter than the
 *   last one, so the previous pass bought nothing and the next one would not
 *   either.
 * - the pass cap.
 * - a pass that did not complete. A failed or cancelled correction pass ends
 *   the harness rather than being re-verified: re-asking a provider that just
 *   errored, or a run the user just cancelled, spends money to learn nothing.
 * - FAIL-CLOSED: `verify()` threw, or answered `null`, part-way through. That
 *   is `"unavailable"` on the log and a full stop — never a pass. A verifier
 *   that cannot answer is the case where assuming success is most expensive,
 *   and the durable event is what lets an operator tell "checked and clean"
 *   apart from "never actually checked".
 *
 * WHAT IT DOES NOT DO: change the run's outcome. A run whose deficiencies
 * survive every pass still completes — exactly as the single-shot check leaves
 * it — with the banner and the final `run.verification` event telling the
 * story. Failing a turn on a partial verification would be a policy decision,
 * and the host that wrote the checks is the only layer entitled to make it.
 * The returned terminal is whatever the LAST pass reached, for the same reason
 * the recovery passes' terminal wins: a provider error on a correction pass is
 * a real failure of this run, not a verification verdict.
 */
export async function runCorrectionHarness(
  context: CorrectionHarnessContext,
  input: {
    basePass: Omit<PassInput, "messages" | "registry" | "maxToolIterations">;
    registry: AiToolRegistry;
    systemPrompt: string | null;
    /** The turn's originating request, or null — see `CorrectionConfig.includeUserRequest`. */
    userRequest: string | null;
    verification: VerificationHook;
    maxPasses: number;
    terminal: PassTerminal;
    /** Provider passes the run has already made; the boundary warning names the next. */
    passesRun: number;
  },
): Promise<PassTerminal> {
  const { basePass, registry, systemPrompt, verification, maxPasses } = input;
  const { ctx, state, chatId, task, assistantMessageId } = basePass;
  const { store } = context.deps;

  let terminal = input.terminal;
  let pass = 0;
  let previousDeficiencies: readonly string[] | undefined;
  let lastReport: DeficiencyReport | null = null;

  for (;;) {
    const report = await verifyQuietly(context, verification, {
      runId: task.taskId,
      chatId,
      scopeId: task.scopeId,
      attemptId: ctx.attemptId,
      toolCallCount: state.toolCallIds.size,
      finalContent: state.content,
      signal: ctx.signal,
    });
    if (report === null) {
      await emitVerification(context, ctx, pass, "unavailable", []);
      break;
    }
    lastReport = report;
    await emitVerification(
      context,
      ctx,
      pass,
      report.status,
      report.deficiencies,
    );
    if (terminal !== "completed") break;
    if (
      !shouldRunCorrectionPass({
        status: report.status,
        deficiencies: report.deficiencies,
        previousDeficiencies,
        passesRun: pass,
        maxPasses,
      })
    ) {
      break;
    }

    previousDeficiencies = report.deficiencies;
    pass += 1;
    // The boundary goes on the log BEFORE the pass's own events, and before
    // the placeholder is blanked below: a consumer that has been streaming
    // the superseded answer has to be told to drop it, and it learns that
    // from this event. See `emitPassBoundary`.
    await context.emitPassBoundary(
      ctx,
      input.passesRun + pass,
      "correction",
      `Verification found ${report.deficiencies.length} unresolved item(s); correcting them (pass ${pass} of ${maxPasses}).`,
    );
    const writeBack = buildDeficiencyWriteBack(report.deficiencies);
    const messages = buildCorrectionMessages({
      systemPrompt,
      userRequest: input.userRequest,
      previousContent: state.content,
      writeBack,
    });
    // The write-back is persisted like every other record this run writes: a
    // CHAIN append off the run's own last write. It is `role: "user"` because
    // that is the role it was sent as, and a stored history that claims the
    // model corrected itself unprompted is a history that replays wrong.
    state.lastMessageId = (
      await store.conversations.appendMessage({
        chatId,
        runId: task.taskId,
        role: "user",
        content: writeBack,
        parentMessageId: state.lastMessageId,
        activate: false,
        metadata: { internal: true, correctionPass: pass },
      })
    ).id;

    // Start the answer over, as the recovery passes do: the corrected answer
    // REPLACES the one the verifier just rejected rather than being glued to
    // the end of it, so the reader is not left with the superseded claim and
    // its correction as one rambling reply. The superseded text is not lost —
    // it went to the provider as this pass's assistant message, and the pass's
    // own tool calls and results are on the log.
    const supersededContent = state.content;
    context.resetPass(state);
    await store.conversations.updateMessage(assistantMessageId, {
      content: "",
    });
    const corrected = await context.runPass({
      ...basePass,
      messages,
      registry,
      ...(context.deps.maxToolIterations === undefined
        ? {}
        : { maxToolIterations: context.deps.maxToolIterations }),
    });
    terminal = corrected.terminal;
    // A correction pass that fixed things silently — all tools, no words —
    // must not blank the answer the user is looking at. Keep what it
    // superseded rather than replacing a real answer with nothing.
    if (state.content.trim().length === 0) {
      state.content = supersededContent;
      await store.conversations.updateMessage(assistantMessageId, {
        content: state.content,
      });
    }
  }

  // One banner, for the last report that actually said something — not one per
  // pass. A conversation showing four increasingly short lists of the same
  // problems tells a reader less than the list that survived.
  if (lastReport !== null && lastReport.status !== "pass") {
    state.lastMessageId = (
      await store.conversations.appendMessage({
        chatId,
        runId: task.taskId,
        role: "system",
        content: describeDeficiencies(lastReport.deficiencies),
        parentMessageId: state.lastMessageId,
        activate: false,
        metadata: { banner: "verification", status: lastReport.status },
      })
    ).id;
  }
  return terminal;
}

export function describeDeficiencies(deficiencies: string[]): string {
  if (deficiencies.length === 0) {
    return "Verification did not pass, but reported no specific deficiency.";
  }
  return [
    `Verification found ${deficiencies.length} unresolved item(s):`,
    ...deficiencies.map((line) => `- ${line}`),
  ].join("\n");
}
