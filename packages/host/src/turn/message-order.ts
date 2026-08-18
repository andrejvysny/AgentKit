import type { MessageRecord } from "../ports/conversation-store.js";

/**
 * Provider-legal ordering rank WITHIN one run.
 *
 * A turn that used tools produces three kinds of record, and the provider only
 * accepts one order for them: the internal assistant turn that requested the
 * calls, then the tool results answering it, then the visible answer. Chat
 * order does not guarantee that — the visible assistant message is created
 * first, as an empty placeholder, the moment the user hits send — so replaying
 * a stored conversation verbatim would hand the provider tool results with no
 * preceding tool_calls, which it rejects outright.
 */
function providerTurnRank(record: MessageRecord): number {
  const internal = record.metadata["internal"] === true;
  if (internal && record.role === "assistant") return 0;
  // Any tool record ranks second, internal flag or not: a tool result is never
  // visible chat content, and one that arrived without the flag would otherwise
  // sort alongside the visible answer and land AFTER it — the exact orphaning
  // this function exists to prevent.
  if (record.role === "tool") return 1;
  return 2;
}

/**
 * Order stored messages the way a provider requires.
 *
 * Ordering is by `orderKey` across the conversation, with one exception: records
 * belonging to the SAME run are ordered by {@link providerTurnRank} first. The
 * exception is scoped to a run because that is the only place the three kinds
 * interleave; between runs, chat order is the truth and must be preserved.
 *
 * Stable: equal-ranked, equal-keyed records keep their input order.
 */
export function orderMessagesForProvider(
  records: readonly MessageRecord[],
): MessageRecord[] {
  return records
    .map((record, index) => ({ record, index }))
    .sort((a, b) => {
      const left = a.record;
      const right = b.record;
      if (left.runId !== undefined && left.runId === right.runId) {
        const delta = providerTurnRank(left) - providerTurnRank(right);
        if (delta !== 0) return delta;
      }
      const keyDelta = left.orderKey - right.orderKey;
      if (keyDelta !== 0) return keyDelta;
      return a.index - b.index;
    })
    .map(({ record }) => record);
}
