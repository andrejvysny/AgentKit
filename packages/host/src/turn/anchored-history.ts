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

/** A run keeps its original ancestry even if a later turn moves the active path. */
export async function anchoredHistory(
  conversations: ConversationStore,
  chatId: string,
  assistantMessageId: string,
  historyLimit: number,
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
  return [...ancestors.reverse(), anchor, ...own];
}
