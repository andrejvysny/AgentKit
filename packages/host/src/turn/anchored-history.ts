import { AgentKitHostError } from "../errors.js";
import type {
  ConversationStore,
  MessageRecord,
} from "../ports/conversation-store.js";

async function requireRecord(
  conversations: ConversationStore,
  chatId: string,
  messageId: string,
): Promise<MessageRecord> {
  const record = await conversations.getMessage(messageId);
  if (!record || record.chatId !== chatId || record.archived === true) {
    throw new AgentKitHostError(
      "invalid_history_anchor",
      "Run history contains a missing or foreign message",
      { chatId, messageId },
    );
  }
  return record;
}

function visit(record: MessageRecord, seen: Set<string>): void {
  if (seen.has(record.id)) {
    throw new AgentKitHostError(
      "invalid_history_anchor",
      "Run history contains a parent cycle",
      { messageId: record.id },
    );
  }
  seen.add(record.id);
}

async function ownRunRecords(
  conversations: ConversationStore,
  anchor: MessageRecord,
  seen: Set<string>,
): Promise<MessageRecord[]> {
  if (anchor.runId === undefined) return [];
  let record = await conversations.lastMessageOfRun(
    anchor.chatId,
    anchor.runId,
  );
  const records: MessageRecord[] = [];
  while (record && record.id !== anchor.id) {
    visit(record, seen);
    const internal =
      record.metadata["internal"] === true ||
      record.role === "tool" ||
      record.role === "system" ||
      record.metadata["correctionPass"] !== undefined;
    if (
      record.chatId !== anchor.chatId ||
      record.archived === true ||
      record.runId !== anchor.runId ||
      !internal ||
      record.parentMessageId === undefined
    ) {
      throw new AgentKitHostError(
        "invalid_history_anchor",
        "Run records do not descend from their own assistant placeholder",
        { messageId: record.id, assistantMessageId: anchor.id },
      );
    }
    records.push(record);
    record = await requireRecord(
      conversations,
      anchor.chatId,
      record.parentMessageId,
    );
  }
  return records.reverse();
}

/**
 * What replays for turns BEFORE the one being answered. `final_answers` is the
 * long-standing behaviour; `transcript` adds each prior turn's own run records
 * (tool calls, results, steering) -- see {@link anchoredHistory}.
 */
export type HistoryPolicy = "final_answers" | "transcript";

/**
 * A run keeps its original ancestry even if a later turn moves the active path.
 *
 * In `transcript` mode every prior visible assistant placeholder on the path
 * also brings its own run's records (they live off the active path, hanging
 * under the placeholder). `orderMessagesForProvider` then slots them before
 * that placeholder's final content, because the placeholder carries the run's
 * lowest order key. A prior run whose records no longer descend from its
 * placeholder is replayed as its final answer alone: one odd old turn must not
 * fail every later turn of the session.
 */
export async function anchoredHistory(
  conversations: ConversationStore,
  chatId: string,
  assistantMessageId: string,
  historyLimit: number,
  policy: HistoryPolicy = "final_answers",
): Promise<MessageRecord[]> {
  const anchor = await requireRecord(conversations, chatId, assistantMessageId);
  if (anchor.role !== "assistant" || anchor.metadata["internal"] === true) {
    throw new AgentKitHostError(
      "invalid_history_anchor",
      "Run history requires a visible assistant placeholder",
      { assistantMessageId },
    );
  }
  const seen = new Set([anchor.id]);
  const own = await ownRunRecords(conversations, anchor, seen);
  const ancestors: MessageRecord[] = [];
  let parentId = anchor.parentMessageId;
  while (
    parentId !== undefined &&
    ancestors.length < Math.max(0, historyLimit - 1)
  ) {
    const parent = await requireRecord(conversations, chatId, parentId);
    visit(parent, seen);
    ancestors.push(parent);
    parentId = parent.parentMessageId;
  }
  // Current-run tool records are not a history window: a resumed call must keep
  // every result it already produced, even when the chain exceeds that window.
  if (policy === "transcript") {
    // A turn submitted under the previous run's last record already has that
    // run's records among its ancestors; one converted from a queued follow-up
    // hangs under the placeholder and does not. Either way each record appears
    // once, and a run is walked with its own cycle guard.
    const onPath = new Set(ancestors.map((record) => record.id));
    const priorRuns: MessageRecord[] = [];
    for (const ancestor of ancestors) {
      if (
        ancestor.role !== "assistant" ||
        ancestor.metadata["internal"] === true ||
        ancestor.runId === undefined ||
        ancestor.runId === anchor.runId
      )
        continue;
      try {
        const records = await ownRunRecords(
          conversations,
          ancestor,
          new Set([ancestor.id]),
        );
        priorRuns.push(...records.filter((record) => !onPath.has(record.id)));
      } catch (error) {
        if (
          !(error instanceof AgentKitHostError) ||
          error.code !== "invalid_history_anchor"
        )
          throw error;
      }
    }
    return [...ancestors.reverse(), ...priorRuns, anchor, ...own];
  }
  return [...ancestors.reverse(), anchor, ...own];
}
