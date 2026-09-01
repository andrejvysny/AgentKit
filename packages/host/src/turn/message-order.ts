import type { MessageRecord } from "../ports/conversation-store.js";

/** A replay-only assistant turn — the one that asked for tools, not the answer. */
function isInternalAssistant(record: MessageRecord): boolean {
  return record.role === "assistant" && record.metadata["internal"] === true;
}

/**
 * Records that must replay BEFORE the run's visible answer: the internal
 * assistant turns that requested tools, and the tool results answering them.
 *
 * Any tool record qualifies, internal flag or not: a tool result is never
 * visible chat content, and one that arrived without the flag would otherwise
 * sort alongside the visible answer and land AFTER it — the exact orphaning
 * this module exists to prevent.
 */
function isReplayPrefix(record: MessageRecord): boolean {
  return isInternalAssistant(record) || record.role === "tool";
}

/**
 * Provider-legal order for the records of ONE run.
 *
 * The unit of ordering is the TOOL-CALL LINKAGE, not the record's kind. Every
 * provider requires an assistant turn that declares `tool_calls` to be followed
 * IMMEDIATELY by exactly the tool results answering those ids — so each internal
 * assistant is emitted with its own results attached, and the groups follow one
 * another in the order the assistant turns were written.
 *
 * WHY LINKAGE AND NOT KIND. Bucketing every internal assistant ahead of every
 * tool result is correct only while a run has ONE tool-calling pass. A run with
 * two — which the correction harness produces routinely, since its write-back
 * tells the model to call its tools again on the SAME run id — would replay as
 * `assistant(tool_calls)`, `assistant(tool_calls)`, then both sets of results:
 * back-to-back tool-call turns with nothing answering the first, which providers
 * reject outright. The linkage is derivable from what is already stored (the
 * assistant's declared ids and each tool record's `toolCallId`), so nothing new
 * has to be persisted to know it.
 *
 * A tool result is CLAIMED by the first internal assistant declaring its id;
 * one no assistant in the run declared stays where chat order put it, still
 * ahead of the visible answer (the caller drops it as an orphan, which is what
 * it is). Everything else the run wrote — the visible answer, the correction
 * write-backs, the host's banners — keeps chat order and goes last, because the
 * visible answer is created as an empty placeholder at submit time and so
 * carries the LOWEST order key of the whole run.
 */
function orderRunRecords(records: readonly MessageRecord[]): MessageRecord[] {
  const prefix = records.filter(isReplayPrefix);
  const tail = records.filter((record) => !isReplayPrefix(record));
  if (prefix.length === 0) return tail;

  const unclaimedByCallId = new Map<string, MessageRecord[]>();
  for (const record of prefix) {
    if (record.role !== "tool" || record.toolCallId === undefined) continue;
    const bucket = unclaimedByCallId.get(record.toolCallId);
    if (bucket === undefined) {
      unclaimedByCallId.set(record.toolCallId, [record]);
    } else {
      bucket.push(record);
    }
  }

  /** Each internal assistant's own tool results, in DECLARATION order. */
  const resultsOf = new Map<MessageRecord, MessageRecord[]>();
  const claimed = new Set<MessageRecord>();
  for (const record of prefix) {
    if (!isInternalAssistant(record)) continue;
    const group: MessageRecord[] = [];
    for (const call of record.toolCalls ?? []) {
      const bucket = unclaimedByCallId.get(call.id);
      if (bucket === undefined) continue;
      // Deleted, not merely read: a malformed run that declared the same id
      // from two turns must not attach the same result to both of them.
      unclaimedByCallId.delete(call.id);
      for (const result of bucket) {
        claimed.add(result);
        group.push(result);
      }
    }
    if (group.length > 0) resultsOf.set(record, group);
  }

  const ordered: MessageRecord[] = [];
  for (const record of prefix) {
    // Claimed results are emitted by their assistant, wherever it sits.
    if (claimed.has(record)) continue;
    ordered.push(record);
    const results = resultsOf.get(record);
    if (results !== undefined) ordered.push(...results);
  }
  ordered.push(...tail);
  return ordered;
}

/**
 * Order stored messages the way a provider requires.
 *
 * Ordering is by `orderKey` across the conversation, with one exception: the
 * records of a single run are permuted among the SLOTS chat order already gave
 * them, into the provider order {@link orderRunRecords} defines. The exception
 * is scoped to a run because that is the only place the kinds interleave;
 * between runs, chat order is the truth and must be preserved.
 *
 * Replaying a stored conversation verbatim would not work: the visible
 * assistant message is created first, as an empty placeholder, the moment the
 * user hits send — so `orderKey` alone hands the provider tool results with no
 * preceding `tool_calls`, which it rejects outright.
 *
 * Stable: equal-keyed records keep their input order, and a run whose records
 * are already provider-ordered is returned untouched.
 */
export function orderMessagesForProvider(
  records: readonly MessageRecord[],
): MessageRecord[] {
  const chatOrder = records
    .map((record, index) => ({ record, index }))
    .sort((a, b) => {
      const keyDelta = a.record.orderKey - b.record.orderKey;
      if (keyDelta !== 0) return keyDelta;
      return a.index - b.index;
    })
    .map(({ record }) => record);

  /** Where each run's records sit in chat order — the slots it may permute. */
  const slotsByRun = new Map<string, number[]>();
  chatOrder.forEach((record, position) => {
    if (record.runId === undefined) return;
    const slots = slotsByRun.get(record.runId);
    if (slots === undefined) {
      slotsByRun.set(record.runId, [position]);
    } else {
      slots.push(position);
    }
  });

  const ordered = [...chatOrder];
  for (const slots of slotsByRun.values()) {
    if (slots.length < 2) continue;
    const run = orderRunRecords(
      slots.map((position) => chatOrder[position] as MessageRecord),
    );
    slots.forEach((position, index) => {
      ordered[position] = run[index] as MessageRecord;
    });
  }
  return ordered;
}
