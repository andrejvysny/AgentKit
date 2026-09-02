/**
 * Chat lifecycle across the stores that hold a conversation's remains.
 *
 * A chat is not only its messages. The runs that executed in it live in
 * `TaskStore` (with their attempts, leases and event logs), and the writes a
 * model staged from it live in `ProposalStore` (with their apply outcomes).
 * Deleting a chat means all three, and no single store can do it — which is
 * exactly why the operation lives here rather than being a fourth method on
 * `ConversationStore`. A conversation store that reached into the task queue
 * would be deciding what a delete MEANS, and that decision is policy: it is the
 * reason this service refuses while work is still live, and a store has no
 * standing to make that call.
 *
 * Archiving is the other half of the same lifecycle and is a thin pass-through
 * on purpose — see {@link ConversationService.archiveChat}.
 */
import { ChatBusyError, RecordNotFoundError } from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { ChatRecord } from "../ports/conversation-store.js";
import type { Logger } from "../ports/system.js";
import type { TaskRecord, TaskStatus } from "../ports/task-store.js";

/**
 * The task statuses that make a chat undeletable.
 *
 * `running` is work a provider is executing right now; `waiting_approval` is
 * work parked on a human decision that has not been made. Both are live in the
 * sense that matters — something still intends to write to this conversation —
 * and deleting out from under either turns a run into an orphan whose next
 * write lands in a chat that no longer exists.
 *
 * `queued` is deliberately NOT here. A queued task has not started, nothing has
 * been spent on it, and `deleteByScope` removes it before any worker can claim
 * it; refusing on one would make a chat undeletable for as long as anything was
 * merely enqueued behind it.
 */
const BUSY_TASK_STATUSES: readonly TaskStatus[] = Object.freeze([
  "running",
  "waiting_approval",
]);

export interface ConversationServiceDeps {
  store: AssistantStore;
  logger?: Logger;
}

export class ConversationService {
  constructor(private readonly deps: ConversationServiceDeps) {}

  /**
   * Delete a chat and everything the host holds about it — messages (every
   * branch, not just the live path), the tasks that ran in its scope with their
   * attempts/leases/events, and the proposals it staged with their outcomes.
   *
   * REFUSES with {@link ChatBusyError} (`chat_busy`, HTTP 409) while any task
   * in the chat's scope is `running` or `waiting_approval`. Force-cancelling
   * those first would be a different operation with different consequences — it
   * ends a provider call, and possibly a staged write mid-apply — and inventing
   * it here would mean a delete button that silently cancels. The caller
   * cancels explicitly (`TaskService.cancelTask`) and deletes after, or waits.
   *
   * THE CHECK HERE IS A FAST PATH, NOT THE GUARANTEE. `TaskStore.deleteByScope`
   * makes the same refusal, atomically, and that is where the invariant
   * actually lives. The reason is that this method cannot enforce it: the
   * `listByScope` read and the `deleteByScope` write are separated by an
   * `await`, and a check-then-act invariant that spans an await is not atomic
   * no matter how carefully the body is written — only a single synchronous
   * statement or transaction inside the adapter is. The reference adapters now
   * make a concurrent `claimNext` (and every other unrelated write) QUEUE
   * behind this transaction rather than join it, which closes the specific race
   * where a worker moved a task `queued → running` inside the gap; but that is
   * an adapter's courtesy, not something {@link AssistantStore} promises of
   * every store. This check survives because it refuses BEFORE the conversation
   * is deleted, which is the better error for the overwhelmingly common case;
   * the race is closed underneath it either way.
   *
   * All three deletes still share one transaction, and no foreign async work
   * happens in here: over `bun:sqlite`, awaiting anything else would let an
   * unrelated store call join this transaction and roll back with it.
   *
   * THE SCOPE IS THE CHAT ID. That is the convention `TurnRunner` writes with
   * (`scopeId: input.chatId`, so two turns in one conversation cannot run at
   * once); a host that scopes its own task kinds on something else keeps those
   * tasks by definition, and should delete them itself.
   */
  async deleteChat(chatId: string): Promise<void> {
    await this.deps.store.transaction(async (tx) => {
      const chat = await tx.conversations.getChat(chatId);
      if (chat === null) {
        throw new RecordNotFoundError(`Chat not found: ${chatId}`, { chatId });
      }
      const tasks = await tx.tasks.listByScope(chatId);
      assertNotBusy(chatId, tasks);
      // Conversation first, so the FK-shaped half of the work is done while the
      // transaction is youngest; the other two touch tables nothing else in
      // this call references.
      await tx.conversations.deleteChat(chatId);
      const deletedTasks = await tx.tasks.deleteByScope(chatId);
      const deletedProposals = await tx.proposals.deleteByChat(chatId);
      this.deps.logger?.info("chat deleted", {
        chatId,
        deletedTasks,
        deletedProposals,
      });
    });
  }

  /**
   * Hide a chat from the default listing, and answer with the updated record.
   *
   * A named wrapper over `updateChat({ archived: true })` rather than a method
   * with its own logic: archiving IS one field, and the value of the name is
   * that a call site reads as the intent instead of as a patch a reader has to
   * decode. Anything more here — cancelling runs, invalidating proposals —
   * would make archiving a destructive operation wearing a reversible name.
   */
  async archiveChat(chatId: string): Promise<ChatRecord> {
    return this.deps.store.conversations.updateChat(chatId, { archived: true });
  }

  /** The inverse of {@link ConversationService.archiveChat}. */
  async unarchiveChat(chatId: string): Promise<ChatRecord> {
    return this.deps.store.conversations.updateChat(chatId, {
      archived: false,
    });
  }
}

/**
 * Refuse the delete while anything in the scope is live, naming what is holding
 * it.
 *
 * `details.taskIds` is not decoration: a UI told only "busy" can offer nothing
 * but "try again", while one told which runs are live can point at them and let
 * the user cancel or wait for the right one.
 */
function assertNotBusy(chatId: string, tasks: readonly TaskRecord[]): void {
  const busy = tasks.filter((task) => BUSY_TASK_STATUSES.includes(task.status));
  if (busy.length === 0) return;
  throw new ChatBusyError(
    `Chat ${chatId} has ${busy.length} task(s) still running or awaiting approval; cancel or await them before deleting.`,
    {
      chatId,
      taskIds: busy.map((task) => task.taskId),
      statuses: busy.map((task) => task.status),
    },
  );
}
