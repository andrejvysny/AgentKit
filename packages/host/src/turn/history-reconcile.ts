import type { AiChatMessage } from "@agentkit/contracts";
import {
  UNCERTAIN_TOOL_RESULT_CODE,
  UNCERTAIN_TOOL_RESULT_MESSAGE,
  type UncertainCall,
} from "./side-effects.js";

/** Error code stamped on a synthesized replacement for a lost tool result. */
export const MISSING_TOOL_RESULT_CODE = "tool_result_missing";

export const MISSING_TOOL_RESULT_MESSAGE =
  "missing tool result for persisted tool call; reconciled as synthetic failure";

/**
 * The slim envelope a synthetic tool result carries.
 *
 * Byte-identical in shape to the one `TurnRunner` persists for a real
 * `run.tool.failed` (and to core's `errorEnvelope`), because the model must not
 * be able to tell a reconciled hole from a tool that genuinely failed — and
 * because a second envelope shape would be one more thing to keep in sync.
 */
function missingToolResultEnvelope(uncertain?: UncertainCall): string {
  const message =
    uncertain === undefined
      ? MISSING_TOOL_RESULT_MESSAGE
      : `${UNCERTAIN_TOOL_RESULT_MESSAGE} (the interrupted call was ${uncertain.toolName})`;
  return JSON.stringify({
    ok: false,
    status: "error",
    summary: message,
    warnings: [],
    truncated: false,
    data: {
      errorCode:
        uncertain === undefined
          ? MISSING_TOOL_RESULT_CODE
          : UNCERTAIN_TOOL_RESULT_CODE,
      errorMessage: message,
    },
  });
}

/**
 * The declared calls of `messages` that no tool message answers. Exposed so a
 * caller can ask the durable log what happened to them (see
 * `findUncertainCalls`) before deciding which envelope each deserves.
 */
export function findOrphanToolCalls(
  messages: readonly AiChatMessage[],
  exceptToolCallIds?: ReadonlySet<string>,
): { toolCallId: string; toolName: string }[] {
  const answered = new Set<string>(exceptToolCallIds);
  for (const message of messages)
    if (message.role === "tool" && message.toolCallId !== undefined)
      answered.add(message.toolCallId);
  const orphans: { toolCallId: string; toolName: string }[] = [];
  const seen = new Set<string>();
  for (const message of messages) {
    for (const call of message.toolCalls ?? []) {
      if (answered.has(call.id) || seen.has(call.id)) continue;
      seen.add(call.id);
      orphans.push({ toolCallId: call.id, toolName: call.name });
    }
  }
  return orphans;
}

/**
 * Restore the balanced-history invariant every provider enforces: an assistant
 * turn that declares `tool_calls` must be followed by exactly one tool message
 * per `tool_call_id`.
 *
 * WHY THIS CAN HAPPEN. The run projection writes the internal assistant
 * message (with its `toolCalls`) on `run.message.completed`, and each tool
 * result on the `run.tool.succeeded` / `run.tool.failed` that follows. Those are
 * SEPARATE writes, not one transaction. Anything that stops the worker between
 * them — a crash, a lost lease, a projection error, a kill during a long tool —
 * leaves a durable assistant turn whose calls were never answered. The turn that
 * died is not the problem; the NEXT turn is, because replaying that history
 * hands the provider an unanswered `tool_call_id` and it rejects the whole
 * request. One interrupted turn would otherwise brick the conversation forever.
 *
 * Reconciliation is in-memory and never persisted: the records are the truth
 * about what happened, and what happened is that no result was ever produced.
 * Writing a fake result would make a crash indistinguishable from a tool that
 * ran — and would do it permanently. The one exception is honest labelling: a
 * call the durable log shows had STARTED (`uncertain`, see
 * {@link findUncertainCalls}) gets an envelope that says so, because telling a
 * model "this failed" about a command that may have completed is how a rerun
 * becomes the second time.
 *
 * SCOPE. This runs over the history assembled from the ConversationStore, which
 * by construction contains only COMMITTED records from past turns: the current
 * turn's placeholder is skipped by the caller, and its live tool traffic lives in
 * the run loop's own message array, never in this one. So every orphan seen here
 * belongs to a turn that is already over, and reconciling all of them is correct.
 *
 * Insertion is positional — each synthetic result goes directly after the
 * assistant message that declared it, in declaration order — so the result stays
 * adjacent to its call even when the conversation continues afterwards.
 */
export function reconcileOrphanToolCalls(
  messages: readonly AiChatMessage[],
  exceptToolCallIds?: ReadonlySet<string>,
  uncertain?: ReadonlyMap<string, UncertainCall>,
): AiChatMessage[] {
  // An approved call about to execute is neither answered nor lost.
  const answered = new Set<string>(exceptToolCallIds);
  for (const message of messages) {
    if (message.role === "tool" && message.toolCallId !== undefined) {
      answered.add(message.toolCallId);
    }
  }
  // Nothing declared, or everything answered: hand back the input untouched so
  // the overwhelmingly common case allocates nothing.
  const hasOrphan = messages.some((message) =>
    (message.toolCalls ?? []).some((call) => !answered.has(call.id)),
  );
  if (!hasOrphan) return [...messages];

  const reconciled: AiChatMessage[] = [];
  for (const message of messages) {
    reconciled.push(message);
    if (message.role !== "assistant") continue;
    for (const call of message.toolCalls ?? []) {
      if (answered.has(call.id)) continue;
      // Guards a (malformed) turn that declared the same id twice: one synthetic
      // answer per id, or the history is unbalanced in the other direction.
      answered.add(call.id);
      reconciled.push({
        role: "tool",
        content: missingToolResultEnvelope(uncertain?.get(call.id)),
        toolCallId: call.id,
        name: call.name,
      });
    }
  }
  return reconciled;
}
