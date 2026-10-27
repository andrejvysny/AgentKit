import type {
  AiChatMessage,
  AiRunEvent,
  AiToolCall,
} from "@agentkit/contracts";
import { createEventStamper } from "../events.js";
import { newRunId, nowIso } from "../ids.js";
import type { AiChatRequest } from "../providers/client.js";
import { boundedRun } from "./bounded-run.js";
import {
  type AgentLoopHooks,
  hasHooks,
  notifySettled,
  runObservers,
} from "./loop-hooks.js";
import { streamModelStep } from "./model-step.js";
import type { RunChatInput, RunChatResult } from "./run-types.js";
import type { AgentSteeringSource } from "./session-seams.js";
import { applySteering } from "./steering-step.js";
import { executeToolBatch } from "./tool-step.js";

export interface AgentLoopInput extends RunChatInput {
  /** Ordered lifecycle hooks (ADR 0015 section 6). Absent or empty: no cost. */
  hooks?: readonly AgentLoopHooks[];
  /** Per-call hook deadline; non-positive or non-finite disables it. */
  hookTimeoutMs?: number;
  /**
   * Durable user messages placed at step boundaries (ADR 0016 section 3).
   * Absent: the loop never polls, so no extra awaits.
   */
  steering?: AgentSteeringSource;
  /**
   * Lets a tool pause the run for human input via `ctx.suspend` (ADR 0016
   * section 5). Off: tools get no `suspend` and "suspended" is unreachable.
   */
  suspendable?: boolean;
  /**
   * Calls to execute FIRST, before the first model request (ADR 0019): their
   * assistant message is already in `messages`, so the batch pushes none and
   * announces nothing. It counts as step 1 without using an iteration; the
   * first model step is then step 1 too, so `run.usage.step` is unchanged.
   */
  resumeCalls?: readonly AiToolCall[];
  /** Merged into every tool context's `metadata` (host-owned, e.g. approvals). */
  toolMetadata?: Readonly<Record<string, unknown>>;
}
export type AgentLoopResult = Omit<RunChatResult, "terminal"> & {
  terminal: RunChatResult["terminal"] | "suspended";
};

const DEFAULT_MAX_TOOL_ITERATIONS = 4;
const DEFAULT_MAX_TOOL_CALLS_PER_ITERATION = 8;

/**
 * Run a multi-turn chat with tool calls. Yields normalized AiRunEvents and
 * returns a {@link RunChatResult}.
 *
 * `input.messages` is never mutated: the loop appends to a private copy and hands
 * back what it appended. Mutating a caller's array was a hidden side effect that
 * made a run impossible to retry, fan out, or run twice from the same history —
 * the second attempt would start from a conversation the first one had rewritten.
 */
export function runAgentLoop(
  input: AgentLoopInput & { suspendable?: false },
): AsyncGenerator<AiRunEvent, RunChatResult, unknown>;
export function runAgentLoop(
  input: AgentLoopInput,
): AsyncGenerator<AiRunEvent, AgentLoopResult, unknown>;
export function runAgentLoop(
  input: AgentLoopInput,
): AsyncGenerator<AiRunEvent, AgentLoopResult, unknown> {
  // No hooks: hand back the loop itself, so the hookless path pays for no
  // extra generator layer (and stays the exact code path `runChat` always was).
  if (!hasHooks(input)) return runLoop(input);
  return withSettledHooks(input);
}

function runLoop(
  input: AgentLoopInput,
): AsyncGenerator<AiRunEvent, AgentLoopResult, unknown> {
  return input.budgets || input.executionBudget
    ? boundedRun(input, runAgentLoopUnchecked)
    : runAgentLoopUnchecked(input);
}

/** Drives the (bounded or plain) loop, then fires `onSettled`. */
async function* withSettledHooks(
  input: AgentLoopInput,
): AsyncGenerator<AiRunEvent, AgentLoopResult, unknown> {
  const result = yield* runLoop(input);
  // After the terminal event, best-effort: a failing hook cannot change how
  // the run ended. A consumer that abandons the generator early (`.return()`)
  // never gets here, so it gets no `onSettled`.
  if (hasHooks(input)) {
    await notifySettled(input, {
      runId: result.runId,
      terminal: result.terminal,
      iterations: result.iterations,
    });
  }
  return result;
}

async function* runAgentLoopUnchecked(
  input: AgentLoopInput,
): AsyncGenerator<AiRunEvent, AgentLoopResult, unknown> {
  const runId = input.runId ?? newRunId();
  // Clamped: `0` (or a negative) used to mean "never call the provider" and end
  // the run as a silent empty success, which reads as a model that answered
  // nothing rather than as the misconfiguration it is.
  const maxIterations = Math.max(
    1,
    input.maxToolIterations ?? DEFAULT_MAX_TOOL_ITERATIONS,
  );
  const maxCallsPerIter =
    input.maxToolCallsPerIteration ?? DEFAULT_MAX_TOOL_CALLS_PER_ITERATION;
  const toolDefinitions = input.registry.listDefinitions();
  const messages: AiChatMessage[] = input.messages.slice();
  const initialLength = messages.length;
  // EVERY event leaves through this stamper — loop-originated and re-yielded
  // provider events alike — so one run emits one unbroken, gap-detectable
  // sequence. A client's own numbering is deliberately overwritten: it counts
  // per call, and a run spans several calls from possibly several clients.
  const stamp = createEventStamper({
    firstSeq: input.firstSeq,
    attemptId: input.attemptId,
  });
  let continuation = input.continuation;
  const onContinuation: AiChatRequest["onContinuation"] = input.onContinuation
    ? async (state) => {
        await input.onContinuation?.(state);
        continuation = state;
      }
    : undefined;
  let iteration = 0;
  let finishReason: string | undefined;

  /** Build the generator's return value for whichever terminal we reached. */
  const finish = (terminal: AgentLoopResult["terminal"]): AgentLoopResult => ({
    runId,
    terminal,
    appendedMessages: messages.slice(initialLength),
    iterations: iteration,
  });

  // Steers placed so far; a source may redeliver an id it has not yet marked.
  const appliedSteers = new Set<string>();

  const hooked = hasHooks(input);
  if (hooked) {
    yield* runObservers(input, runId, stamp, "onRunStart", {
      runId,
      model: input.model,
      toolNames: toolDefinitions.map((d) => d.name),
      messageCount: messages.length,
      signal: input.signal,
    });
  }

  if (input.resumeCalls && input.resumeCalls.length > 0) {
    iteration = 1;
    const outcome = yield* executeToolBatch({
      input,
      runId,
      step: 1,
      stamp,
      messages,
      maxCallsPerIter: input.resumeCalls.length,
      assistantContent: "",
      toolCalls: input.resumeCalls.slice(),
      duplicateIds: [],
      announcedToolCallIds: new Set(),
      assistantInHistory: true,
    });
    if (outcome === "cancelled") return finish("cancelled");
    if (outcome === "suspended") return finish("suspended");
  }

  for (let i = 0; i < maxIterations; i++) {
    iteration = i + 1;
    if (input.signal?.aborted) {
      yield stamp({
        type: "run.cancelled",
        runId,
        timestamp: nowIso(),
        data: { reason: "aborted" },
      });
      return finish("cancelled");
    }
    if (hooked) {
      yield* runObservers(input, runId, stamp, "onStepStart", {
        runId,
        step: iteration,
        messages: Object.freeze(messages.slice()),
        signal: input.signal,
      });
      // Hooks are awaited: the signal can have moved while they ran.
      if (input.signal?.aborted) {
        yield stamp({
          type: "run.cancelled",
          runId,
          timestamp: nowIso(),
          data: { reason: "aborted" },
        });
        return finish("cancelled");
      }
    }

    const step = yield* streamModelStep({
      input,
      runId,
      iteration,
      messages,
      toolDefinitions,
      stamp,
      getContinuation: () => continuation,
      onContinuation,
    });
    if (step.kind !== "response") return finish(step.kind);
    const assistantContent = step.content;
    const turnFinishReason = step.finishReason;
    if (hooked) {
      yield* runObservers(input, runId, stamp, "onModelResponse", {
        runId,
        step: iteration,
        content: assistantContent,
        toolCalls: step.toolCalls,
        finishReason: turnFinishReason,
        signal: input.signal,
      });
    }

    // Hoisted above the tool-call branch on purpose: a turn cut off at the
    // token budget usually breaks the TOOL ARGUMENTS (which then fail as
    // bad_args with no hint why), and that is exactly the case the old
    // no-tool-calls-only placement never warned about.
    if (turnFinishReason === "length") {
      yield stamp({
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "truncated",
          message:
            "Response truncated (finish_reason=length); increase max output tokens.",
        },
      });
    }

    // If the assistant produced tool calls, append the assistant message with tool_calls
    // and execute each tool, appending role:'tool' messages with tool_call_id.
    if (step.toolCalls.length > 0) {
      const outcome = yield* executeToolBatch({
        input,
        runId,
        step: iteration,
        stamp,
        messages,
        maxCallsPerIter,
        assistantContent,
        toolCalls: step.toolCalls,
        duplicateIds: step.duplicateIds,
        announcedToolCallIds: step.announcedToolCallIds,
      });
      if (outcome === "cancelled") return finish("cancelled");
      // No terminal event: run.waiting was the last one the batch yielded.
      if (outcome === "suspended") return finish("suspended");
      // Steers are polled ONLY here: after tools ran and another model call
      // follows. Not before step 1, after a final answer, or on the last
      // iteration; leftovers stay pending and the host turns them into
      // follow-ups at settlement. (Pi would keep going after a final answer;
      // we do not, so the persisted history is exactly what the model saw.)
      if (input.steering && i + 1 < maxIterations) {
        yield* applySteering({
          source: input.steering,
          runId,
          step: iteration + 1,
          stamp,
          messages,
          applied: appliedSteers,
          hookTimeoutMs: input.hookTimeoutMs,
          signal: input.signal,
        });
      }
      // Loop again so the model can react to tool results.
      continue;
    }

    // No tool calls; commit final assistant message and finish.
    // `turnFinishReason` may be the synthetic "incomplete" (stream cut before
    // the provider said why) — passed through untouched, because defaulting it
    // to "stop" is precisely the lie that made a half answer look final.
    finishReason = turnFinishReason ?? "stop";
    // #4: surface finish_reason="tool_calls" with no reconstructable call — but
    // only if the provider didn't already emit it during streaming (dedup).
    if (finishReason === "tool_calls" && !step.sawNoToolCallWarning) {
      yield stamp({
        type: "run.warning",
        runId,
        timestamp: nowIso(),
        data: {
          code: "tool_call_unparseable",
          message:
            "Provider reported finish_reason=tool_calls but emitted no usable tool call.",
        },
      });
    }
    // Cancellation arriving while the last chunk was in flight must not be
    // committed as a finished answer. Checked HERE, immediately before the
    // commit and its terminal event: every yield above handed control back to
    // the consumer, so the signal can have moved since the loop-top check.
    if (input.signal?.aborted) {
      yield stamp({
        type: "run.cancelled",
        runId,
        timestamp: nowIso(),
        data: { reason: "aborted" },
      });
      return finish("cancelled");
    }
    messages.push({ role: "assistant", content: assistantContent });
    yield stamp({
      type: "run.completed",
      runId,
      timestamp: nowIso(),
      data: { iterations: iteration, finishReason },
    });
    return finish("completed");
  }

  yield stamp({
    type: "run.warning",
    runId,
    timestamp: nowIso(),
    data: {
      code: "max_iterations",
      message: `Reached maxToolIterations=${maxIterations} without final answer.`,
    },
  });
  // Exhausting the iteration budget still ends on run.completed (with a warning),
  // so the terminal reported here matches the event stream.
  yield stamp({
    type: "run.completed",
    runId,
    timestamp: nowIso(),
    data: { iterations: iteration, finishReason: "max_iterations" },
  });
  return finish("completed");
}
