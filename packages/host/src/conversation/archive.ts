import { AgentKitHostError, RecordNotFoundError } from "../errors.js";
import type { MessageRecord } from "../ports/conversation-store.js";

/** Validate the entire subtree before an adapter changes any archive flags. */
export function archiveSubtreeOf(
  records: readonly MessageRecord[],
  messageId: string,
): MessageRecord[] {
  const target = records.find((record) => record.id === messageId);
  if (!target || target.archived === true)
    throw new RecordNotFoundError(`Message not found: ${messageId}`);
  const children = new Map<string, MessageRecord[]>();
  for (const record of records) {
    if (record.parentMessageId === undefined) continue;
    const group = children.get(record.parentMessageId) ?? [];
    group.push(record);
    children.set(record.parentMessageId, group);
  }
  const subtree: MessageRecord[] = [];
  const pending = [target];
  const seen = new Set<string>();
  for (let index = 0; index < pending.length; index++) {
    const record = pending[index];
    if (!record || seen.has(record.id)) continue;
    seen.add(record.id);
    if (record.active)
      throw new AgentKitHostError(
        "active_branch_archive",
        "Cannot archive active branch. Activate a different branch first.",
      );
    subtree.push(record);
    pending.push(...(children.get(record.id) ?? []));
  }
  return subtree.filter((record) => record.archived !== true);
}
