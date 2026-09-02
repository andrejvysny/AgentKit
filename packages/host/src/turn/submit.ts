/**
 * Turning a caller's request into a durable `chat.turn` task: `submitMessage`
 * (a new question) and `regenerate` (re-answer an existing one), plus the
 * shared idempotent-redelivery check `resubmitted`. Split out of
 * `turn-runner.ts` because none of it runs a provider pass — it only writes
 * the task and the messages a pass will later read.
 */
import type { AiMessageContent } from "@agentkit/contracts";
import {
  DuplicateTaskError,
  InvalidRegenerateError,
  RecordNotFoundError,
} from "../errors.js";
import { CHAT_TURN_TASK_KIND } from "../tasks/kinds.js";
import type { TurnRunnerDeps } from "./turn-runner.js";

export interface SubmitMessageInput {
  chatId: string;
  /**
   * The turn's body — a string, or content parts for a multimodal turn. Written
   * to the user message verbatim; an image part naming a host attachment
   * (`source: { kind: "ref", ref }`) is stored as the ref and resolved per
   * provider pass, never inlined into the record.
   */
  content: AiMessageContent;
  model?: string;
  providerId?: string;
  metadata?: Record<string, unknown>;
  priority?: number;
  /**
   * Submit this turn as a NEW BRANCH under the named message, instead of at the
   * end of the conversation — the edit-and-regenerate flow.
   *
   * It is passed straight through to the user message's
   * `AppendMessageInput.parentMessageId`, which is where the branch is actually
   * made: the store creates the message active and switches the whole path to it
   * in the same write, so by the time this turn executes, `assembleMessages`
   * reads the NEW branch's history and the old leaf is off-path. Nothing else in
   * this class needs to know a branch happened, and that is deliberate — a
   * second place deciding what "the conversation so far" means is a second place
   * that could disagree with the store.
   */
  parentMessageId?: string;
  /**
   * The caller's idempotency key for this submit, used verbatim as the task id.
   *
   * Omit it and every call starts a new turn — the right default for a UI
   * holding an open socket, and the wrong one for anything that can retry.
   * Anything that CAN retry must supply one (the REST layer maps its
   * `Idempotency-Key` header onto this field): resubmitting the same key writes
   * nothing, returns the ids of the turn that already exists, and re-pokes the
   * queue, so a retried request cannot answer a user twice.
   *
   * Reusing a key for a DIFFERENT conversation is an id collision rather than a
   * retry, and throws `DuplicateTaskError` instead of answering.
   */
  taskId?: string;
  /**
   * The task kind this turn is created as. Defaults to
   * {@link CHAT_TURN_TASK_KIND}, which is the kind `ChatTurnExecutor` claims.
   *
   * Naming another kind routes the turn to the host's OWN executor while
   * everything this method does stays identical — the same user message, the
   * same placeholder, the same one transaction, the same idempotency and the
   * same branch mechanics. It is the submit half of the seam whose execute half
   * is `createRunProjector` (see [`projection.ts`](./projection.ts)): a host
   * that registers, say, `assistant.cloud-chat` for a turn it delegates to a
   * server maps the remote frames into `AiRunEvent`s and drives the projector,
   * and the conversation it leaves behind is the one a `chat.turn` would have.
   *
   * NOT VALIDATED HERE, deliberately. Whether a kind has an executor is a
   * DEPLOYMENT fact — the box that claims the task is the box that knows — and
   * a check here would have to be wrong in one of two ways: it would either
   * refuse a kind whose executor lives in another process, or pass a kind
   * nobody registered anywhere. The dispatcher answers it at claim time with
   * `ExecutorNotFoundError`, which is a terminal failure carrying the kind.
   */
  kind?: string;
}

export interface SubmitMessageResult {
  chatId: string;
  runId: string;
  /**
   * The question this turn answers. A submit mints it; a
   * {@link TurnRunner.regenerate} names the one that was already there — see
   * {@link RegenerateMessageInput}.
   */
  userMessageId: string;
  assistantMessageId: string;
}

/**
 * Answer an existing question again, as a NEW BRANCH beside the answer it
 * already has.
 *
 * The counterpart to a branch submit: that one rewrites the question, this one
 * keeps it and re-rolls the answer. Nothing is edited and nothing is deleted —
 * the old answer stays in the tree at its own `branchIndex`, off the active
 * path, exactly where {@link ConversationStore.listSiblings} will report it.
 */
export interface RegenerateMessageInput {
  chatId: string;
  /**
   * The assistant message to re-answer. Must be IN this chat, `role:
   * "assistant"`, not a replay-only (`internal: true`) record, and must have a
   * parent — anything else raises {@link RecordNotFoundError} or
   * {@link InvalidRegenerateError} rather than being reinterpreted.
   *
   * Its PARENT is what the new branch hangs off, which is why a root is
   * refused: a message with nothing above it answers no question, so there is
   * nothing to ask again.
   */
  messageId: string;
  model?: string;
  providerId?: string;
  /** Metadata for the new placeholder, merged over `{ placeholder: true }`. */
  metadata?: Record<string, unknown>;
  priority?: number;
  /**
   * The caller's idempotency key, used verbatim as the task id — same contract
   * as {@link SubmitMessageInput.taskId}: resubmitting one writes nothing and
   * returns the ids of the regenerate that already exists.
   */
  taskId?: string;
  /**
   * The task kind this regenerate is created as. Defaults to
   * {@link CHAT_TURN_TASK_KIND}; same contract as
   * {@link SubmitMessageInput.kind}, including that an unknown kind is the
   * dispatcher's `ExecutorNotFoundError` at claim time rather than a refusal
   * here.
   */
  kind?: string;
}

/**
 * What a `chat.turn` task's `payload` carries for this worker.
 *
 * `chatId` lives here rather than on {@link TaskRecord}: the durable record is
 * kind-agnostic — it knows about scopes, attempts and leases — and a reindex
 * task has no conversation to name. A turn's scope happens to BE its chat, but
 * that is this kind's choice, not the queue's, and a host that scopes a turn on
 * a shared document instead still needs the chat spelled out here.
 */
export interface TurnRequest {
  chatId: string;
  model?: string;
  providerId?: string;
  assistantMessageId: string;
  userMessageId?: string;
}

/**
 * Record a turn and hand it to the queue. Never waits on the model: the task
 * is durable the moment this returns, and the answer arrives through the event
 * log.
 *
 * Both message ids are minted HERE rather than taken from what the store
 * assigns, because the task row has to be written before them (see below) and
 * its payload names them.
 */
export async function submitMessage(
  deps: TurnRunnerDeps,
  input: SubmitMessageInput,
): Promise<SubmitMessageResult> {
  const taskId = input.taskId ?? deps.ids.taskId();
  const kind = input.kind ?? CHAT_TURN_TASK_KIND;
  const userMessageId = deps.ids.messageId();
  const assistantMessageId = deps.ids.messageId();
  const payload: TurnRequest = {
    chatId: input.chatId,
    ...(input.model === undefined ? {} : { model: input.model }),
    ...(input.providerId === undefined ? {} : { providerId: input.providerId }),
    assistantMessageId,
    userMessageId,
  };

  try {
    await deps.store.transaction(async (tx) => {
      // The task goes in FIRST, ahead of both messages. It is the only write
      // in this transaction that can reject, and on a resubmitted
      // `input.taskId` it must reject before anything else lands: a store
      // whose `transaction` is a real BEGIN/ROLLBACK would undo the messages
      // anyway, but one where it is only a logical grouping (the reference
      // `MemoryAssistantStore`, the host's own test fakes) would leave two
      // orphan messages in the chat for every retried request.
      await tx.tasks.createTask({
        taskId,
        kind,
        // Scope on the chat: two turns in one conversation must not run at
        // once, or they would interleave into the same message history.
        scopeId: input.chatId,
        payload: payload as unknown as Record<string, unknown>,
        // And, by default, must not be ACCEPTED at once either — the refusal
        // is the store's, inside this transaction, so two racing submits
        // cannot both get past it. See `allowConcurrentSubmit`.
        exclusiveScope: deps.allowConcurrentSubmit !== true,
        ...(input.priority === undefined ? {} : { priority: input.priority }),
      });
      await tx.conversations.appendMessage({
        id: userMessageId,
        chatId: input.chatId,
        role: "user",
        content: input.content,
        // Absent on the ordinary submit, which appends to the active leaf
        // exactly as it always did; present, it makes this turn a branch.
        ...(input.parentMessageId === undefined
          ? {}
          : { parentMessageId: input.parentMessageId }),
        metadata: input.metadata ?? {},
      });
      // The placeholder exists so the UI has a message to stream into, and so
      // the task has one durable target to write the answer to no matter how
      // many provider passes it takes.
      await tx.conversations.appendMessage({
        id: assistantMessageId,
        chatId: input.chatId,
        runId: taskId,
        role: "assistant",
        content: "",
        // Named EXPLICITLY rather than left to the store's
        // append-to-the-active-leaf default, even though the user message just
        // written IS that leaf. Saying so costs nothing and makes the pairing
        // structural: the answer is a child of the question it answers, and a
        // branch submit cannot end up with its placeholder hanging off the
        // branch it replaced.
        parentMessageId: userMessageId,
        metadata: { placeholder: true },
      });
    });
  } catch (err) {
    const existing = await resubmitted(deps, err, { ...input, kind }, taskId);
    if (existing === null) throw err;
    // Re-poked deliberately. `enqueue` is idempotent per the port's contract,
    // so this is a no-op for a task already running or finished — and the
    // rescue for the case the redelivery exists to cover, where the FIRST
    // submit committed its transaction and then died before it could poke.
    await deps.taskRunner.enqueue({ taskId, scopeId: input.chatId });
    return existing;
  }

  await deps.taskRunner.enqueue({ taskId, scopeId: input.chatId });
  return {
    chatId: input.chatId,
    runId: taskId,
    userMessageId,
    assistantMessageId,
  };
}

/**
 * The already-recorded turn when `err` says this submit is a redelivery of
 * one; null when the caller should see the original throw.
 *
 * Everything it checks is about the key identifying THE SAME turn:
 *
 * - a MINTED id that collides is not a redelivery, it is a broken
 *   `IdGenerator`, and swallowing it would hand the caller someone else's
 *   conversation;
 * - a task under this key whose kind is not the one this submit asked for, or
 *   which is a turn in another chat, is an id collision between two unrelated
 *   callers — same reasoning, louder failure mode;
 * - a payload missing either message id cannot answer the caller at all, and
 *   a turn row without them did not come from this method;
 * - and, where the caller can name the question it is re-answering (a
 *   regenerate, which branches under an EXISTING message), a payload naming a
 *   different one is two different regenerates that reused one `taskId`, not
 *   a redelivery.
 */
async function resubmitted(
  deps: TurnRunnerDeps,
  err: unknown,
  input: {
    chatId: string;
    taskId?: string;
    kind: string;
    /**
     * The question the caller means to re-answer, when it HAS one. A submit
     * mints its user message here and so cannot check it; a regenerate does
     * not, and must — see the `userMessageId` clause below.
     */
    expectUserMessageId?: string;
  },
  taskId: string,
): Promise<SubmitMessageResult | null> {
  if (!(err instanceof DuplicateTaskError) || input.taskId === undefined) {
    return null;
  }
  const existing = await deps.store.tasks.getTask(taskId);
  if (!existing || existing.kind !== input.kind) return null;
  const payload = existing.payload as unknown as Partial<TurnRequest>;
  if (
    payload.chatId !== input.chatId ||
    typeof payload.userMessageId !== "string" ||
    typeof payload.assistantMessageId !== "string"
  ) {
    return null;
  }
  // Same task id, same chat, same kind — but a DIFFERENT question. Two
  // regenerates that reused one caller-supplied `taskId` against different
  // messages are not a redelivery of each other, and answering the second
  // with the first one's run would point the caller at a branch under a
  // message it never named.
  if (
    input.expectUserMessageId !== undefined &&
    payload.userMessageId !== input.expectUserMessageId
  ) {
    return null;
  }
  return {
    chatId: payload.chatId,
    runId: existing.taskId,
    userMessageId: payload.userMessageId,
    assistantMessageId: payload.assistantMessageId,
  };
}

/**
 * Re-answer a question that already has an answer, as a NEW BRANCH beside it.
 *
 * Mechanically {@link submitMessage} minus the user message: one transaction
 * writes the task and an empty assistant placeholder, the placeholder hangs
 * off the TARGET'S PARENT (so the store gives it the next `branchIndex` and
 * switches the active path to it in the same write), and the queue is poked
 * afterwards. Idempotent per caller-supplied `taskId` by the same route the
 * submit is.
 *
 * Nothing about the old answer changes — it keeps its id, its index and its
 * place in the tree, and simply stops being active. That is the whole reason
 * the placeholder is appended rather than the old record rewritten: a
 * regenerate a user dislikes must be undoable by activating the branch they
 * had before, and a rewrite has nothing left to go back to.
 *
 * The turn itself needs no notion that this happened. By the time the task
 * executes, the active path ends at the parent question, so `assembleMessages`
 * replays exactly the history the original answer saw — and the answer it
 * skips (`record.id === assistantMessageId`) is the new placeholder, not the
 * old sibling, which is off-path and never read.
 */
export async function regenerate(
  deps: TurnRunnerDeps,
  input: RegenerateMessageInput,
): Promise<SubmitMessageResult> {
  const parentMessageId = await regenerateParentOf(deps, input);
  const taskId = input.taskId ?? deps.ids.taskId();
  const kind = input.kind ?? CHAT_TURN_TASK_KIND;
  const assistantMessageId = deps.ids.messageId();
  const payload: TurnRequest = {
    chatId: input.chatId,
    ...(input.model === undefined ? {} : { model: input.model }),
    ...(input.providerId === undefined ? {} : { providerId: input.providerId }),
    assistantMessageId,
    // The question this run answers. It is not a message this call created —
    // that is the difference between a regenerate and a submit — but it is
    // what `resubmitted` reads to answer a redelivery, and what the caller
    // renders the new branch under.
    userMessageId: parentMessageId,
  };

  try {
    await deps.store.transaction(async (tx) => {
      // Task first, for the reason spelled out in `submitMessage`: it is the
      // only write here that can reject, and on a redelivered `taskId` it
      // must reject before the placeholder lands — otherwise a store whose
      // `transaction` is only a logical grouping leaves an orphan branch in
      // the chat, ACTIVE, for every retried request.
      await tx.tasks.createTask({
        taskId,
        kind,
        scopeId: input.chatId,
        payload: payload as unknown as Record<string, unknown>,
        // Same exclusivity as a submit: re-rolling an answer while the chat
        // is still producing one is the same interleaving, reached a
        // different way. See `allowConcurrentSubmit`.
        exclusiveScope: deps.allowConcurrentSubmit !== true,
        ...(input.priority === undefined ? {} : { priority: input.priority }),
      });
      await tx.conversations.appendMessage({
        id: assistantMessageId,
        chatId: input.chatId,
        runId: taskId,
        role: "assistant",
        content: "",
        // Naming the parent is what makes this a branch: the store assigns
        // the next `branchIndex` under it and activates the new path in the
        // same write. `activate` is left at its default — a regenerate IS a
        // request to look at the new answer.
        parentMessageId,
        // `placeholder` last, so a caller's metadata can decorate the record
        // but cannot unset the flag the run projects onto.
        metadata: { ...(input.metadata ?? {}), placeholder: true },
      });
    });
  } catch (err) {
    const existing = await resubmitted(
      deps,
      err,
      { ...input, kind, expectUserMessageId: parentMessageId },
      taskId,
    );
    if (existing === null) throw err;
    await deps.taskRunner.enqueue({ taskId, scopeId: input.chatId });
    return existing;
  }

  await deps.taskRunner.enqueue({ taskId, scopeId: input.chatId });
  return {
    chatId: input.chatId,
    runId: taskId,
    userMessageId: parentMessageId,
    assistantMessageId,
  };
}

/**
 * The message a regenerate branches under, or a refusal naming why there
 * isn't one.
 *
 * `listSiblings` is the lookup because the conversation port has no
 * `getMessage`: it is self-inclusive, so the target comes back among its own
 * siblings, and it already raises `not_found` for an id nothing has.
 *
 * The four checks are all "is this a message that answered a question?", and
 * each is refused rather than repaired:
 *
 * - another chat's message — the caller named a pair that does not go
 *   together, and running the turn in `input.chatId` would write this chat's
 *   answer into that one's history;
 * - a `user`/`tool`/`system` record — regenerating a question, or a tool
 *   result, is not an operation this class has;
 * - an `internal: true` record — the replay-only assistant turn that carries
 *   `toolCalls`. Branching under its parent would strand the tool results
 *   answering it on a path nothing will replay;
 * - a root — nothing above it to re-ask.
 */
async function regenerateParentOf(
  deps: TurnRunnerDeps,
  input: RegenerateMessageInput,
): Promise<string> {
  const siblings = await deps.store.conversations.listSiblings(input.messageId);
  const target = siblings.find((record) => record.id === input.messageId);
  if (target === undefined || target.chatId !== input.chatId) {
    throw new RecordNotFoundError(
      `Message not found in chat ${input.chatId}: ${input.messageId}`,
      { chatId: input.chatId, messageId: input.messageId },
    );
  }
  if (target.role !== "assistant" || target.metadata["internal"] === true) {
    throw new InvalidRegenerateError(
      `Message ${input.messageId} is not a regeneratable answer; only a visible assistant message can be re-answered.`,
      {
        chatId: input.chatId,
        messageId: input.messageId,
        role: target.role,
        internal: target.metadata["internal"] === true,
      },
    );
  }
  if (target.parentMessageId === undefined) {
    throw new InvalidRegenerateError(
      `Message ${input.messageId} is a root and answers no question; there is nothing to ask again.`,
      { chatId: input.chatId, messageId: input.messageId },
    );
  }
  return target.parentMessageId;
}
