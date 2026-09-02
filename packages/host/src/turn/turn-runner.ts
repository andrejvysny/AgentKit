import type {
  AiChatMessage,
  AiContentPart,
  AiContextBinding,
  AiMessageContent,
  AiProviderConfig,
  AiRunEvent,
  AiToolLimits,
} from "@agentkit/contracts";
import {
  AiToolRegistry,
  createEventStamper,
  messageContentToText,
  resolveToolLimits,
  runChat,
  type AiProviderClient,
  type AiRunEventDraft,
} from "@agentkit/core";
import {
  AgentKitHostError,
  DuplicateTaskError,
  InvalidRegenerateError,
  LeaseLostError,
  RecordNotFoundError,
  UsageDeniedError,
} from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type {
  AttachmentResolver,
  ResolvedAttachment,
} from "../ports/attachment-resolver.js";
import type { ContextProvider } from "../ports/context-provider.js";
import type { SecretStore } from "../ports/secret-store.js";
import type { Clock, IdGenerator, Logger } from "../ports/system.js";
import type {
  TaskExecution,
  TaskRunner,
  TaskWorker,
} from "../ports/task-runner.js";
import type { ToolSetContributor } from "../ports/tool-contributor.js";
import type { ToolGuard } from "../ports/tool-guard.js";
import type { UsageAuthorizer } from "../ports/usage-authorizer.js";
import type { AssistantSettings } from "../ports/settings-store.js";
import type { TaskRecord, TaskStatus } from "../ports/task-store.js";
import type {
  DeficiencyReport,
  VerificationHook,
  VerificationInput,
} from "../ports/verification.js";
import { CHAT_TURN_TASK_KIND } from "../tasks/kinds.js";
import { loadExecutableTask } from "../tasks/load-executable-task.js";
import type {
  TaskExecutionContext,
  TaskExecutor,
} from "../tasks/task-executor.js";
import {
  buildCorrectionMessages,
  buildDeficiencyWriteBack,
  resolveMaxCorrectionPasses,
  shouldRunCorrectionPass,
  type CorrectionConfig,
} from "./correction-harness.js";
import {
  EMULATED_TOOL_CALL_MESSAGE,
  looksLikeEmulatedToolCall,
} from "./emulated-tool-call.js";
import {
  isHookTimeout,
  resolveHookTimeouts,
  withHookDeadline,
  type HookTimeouts,
  type ResolvedHookTimeouts,
} from "./hook-deadline.js";
import { reconcileOrphanToolCalls } from "./history-reconcile.js";
import { orderMessagesForProvider } from "./message-order.js";
import {
  createRunProjector,
  type RunProjectionState,
  type RunProjector,
} from "./projection.js";
import { stageRegistry } from "./registry-staging.js";
import {
  RETRY_MAX_TOOL_ITERATIONS,
  filterToolTurns,
  shouldRetryChatOnly,
  shouldRetryEmptyResponse,
  type PassTerminal,
} from "./retry.js";

/** Provider metadata key holding the {@link SecretStore} ref for the API key. */
export const PROVIDER_SECRET_REF_KEY = "apiKeySecretRef";

/** Default number of messages replayed to the provider. */
const DEFAULT_HISTORY_LIMIT = 200;

/**
 * Default attachment budgets for ONE provider pass.
 *
 * Borrowed from OpenPCB's `MENTION_LIMITS`, which are the numbers a shipping
 * product arrived at against real vision models rather than a guess: 5 MiB is
 * comfortably above a full-resolution screenshot and below the request size
 * providers start rejecting; 20 MiB and 16 images bound what a long conversation
 * full of attachments can cost on EVERY pass, since history is replayed whole.
 *
 * They are ceilings on what the {@link AttachmentResolver} contributes, not on
 * the request: a caller that inlines its own base64 `data` sources has already
 * decided how big its messages are, and second-guessing that here would drop
 * images the host never asked this port about.
 */
const DEFAULT_MAX_BYTES_PER_IMAGE = 5 * 1024 * 1024;
const DEFAULT_MAX_TOTAL_BYTES = 20 * 1024 * 1024;
const DEFAULT_MAX_IMAGES = 16;

/**
 * Caps on what resolving attachments may add to one provider pass. Every field
 * is optional and falls back to the constant above.
 */
export interface AttachmentBudgets {
  /** Decoded bytes one image may contribute. Default 5 MiB. */
  maxBytesPerImage?: number;
  /** Decoded bytes ALL resolved images may contribute to one pass. Default 20 MiB. */
  maxTotalBytes?: number;
  /** How many resolved images one pass may carry. Default 16. */
  maxImages?: number;
}

interface ResolvedBudgets {
  maxBytesPerImage: number;
  maxTotalBytes: number;
  maxImages: number;
}

/**
 * Decoded size of a base64 payload, without decoding it.
 *
 * Four base64 characters encode three bytes, so `length * 3 / 4` is the size to
 * within the two padding characters — an over-estimate by at most 2 B, on a
 * budget measured in mebibytes. Decoding to find out exactly would allocate the
 * whole image to answer a question about whether to allocate the whole image.
 */
function decodedByteLength(base64: string): number {
  return Math.floor((base64.length * 3) / 4);
}

/** Whether a message body carries at least one `ref`-sourced image part. */
function hasRefImage(content: AiMessageContent): boolean {
  return (
    typeof content !== "string" &&
    content.some((part) => part.type === "image" && part.source.kind === "ref")
  );
}

/**
 * Why a resolved attachment may not join this pass, or `null` when it may.
 *
 * The three caps are checked in order of how local the failure is — this
 * image's own size, then what the pass has already spent, then how many images
 * it already carries — so the reason a caller is told is the most specific one
 * that applies. The returned string is the tail of the warning message, and it
 * names the number that was hit: "over budget" with no figure in it is a
 * warning nobody can act on.
 */
function budgetRefusal(input: {
  bytes: number;
  totalBytes: number;
  images: number;
  budgets: ResolvedBudgets;
}): string | null {
  const { bytes, totalBytes, images, budgets } = input;
  if (bytes > budgets.maxBytesPerImage) {
    return `its ${bytes} decoded bytes exceed the ${budgets.maxBytesPerImage}-byte per-image budget.`;
  }
  if (totalBytes + bytes > budgets.maxTotalBytes) {
    return `its ${bytes} decoded bytes would push this pass past the ${budgets.maxTotalBytes}-byte total budget (${totalBytes} already used).`;
  }
  if (images + 1 > budgets.maxImages) {
    return `this pass already carries its budgeted ${budgets.maxImages} image(s).`;
  }
  return null;
}

/**
 * Everything the worker needs, injected.
 *
 * There is deliberately no `WritePolicy` here: the runner never asks whether a
 * write may apply itself. That question belongs to the write tool, which is the
 * only place that knows what is being written — `createProposalBuilderTool`
 * consults the policy inside `execute`, on the staged proposal, and a copy of
 * the policy on the runner would be a second consultation point that could
 * disagree with the first.
 */
export interface TurnRunnerDeps {
  store: AssistantStore;
  taskRunner: TaskRunner;
  /** Builds a client for a resolved provider config (key already injected). */
  providerFactory(config: AiProviderConfig): AiProviderClient;
  secrets?: SecretStore;
  contributors: ToolSetContributor[];
  /**
   * Visibility/executability policy applied to every contributed tool. Absent —
   * the default — nothing is asked and every contributed tool is staged and
   * callable, exactly as before the port existed. See {@link ToolGuard}.
   */
  toolGuards?: ToolGuard[];
  context?: ContextProvider;
  verification?: VerificationHook;
  /**
   * Opt into the multi-pass correction harness over {@link verification}.
   *
   * Absent — the default — {@link verification} is a SINGLE post-run check whose
   * deficiencies are posted as a banner and nothing more: exactly what this
   * class did before the harness existed, down to the events on the log (there
   * are no extra ones) and the number of `verify()` calls (one).
   *
   * Present AND with {@link verification} wired, the deficiencies are fed back
   * to the model for bounded correction passes, each one a full `runPass` — so
   * {@link usage} gates it and its `run.usage` events are recorded like any
   * other pass — and every verification, including the first, is reported on the
   * durable log as a `run.verification` event. Present WITHOUT
   * {@link verification} it does nothing at all: there is no check to iterate
   * on, and inventing one is not this class's business.
   *
   * See [`correction-harness.ts`](./correction-harness.ts) for the stopping
   * rules.
   */
  correction?: CorrectionConfig;
  /**
   * Spend control. Absent — the default — nothing is asked and nothing is
   * recorded, and this class behaves exactly as it did before the port existed;
   * a host that does not wire it gets no enforcement, which is the whole point
   * of the port being optional rather than of it being ignored.
   *
   * Wired, `authorize()` is consulted before EVERY provider pass (the first one
   * and each recovery pass — a retry bills again, so it must be asked again),
   * and `record()` is called for every `run.usage` event the provider emits.
   */
  usage?: UsageAuthorizer;
  /**
   * Turns the `ref` image sources in stored messages into bytes a provider can
   * be shown. Absent — the default — nothing resolves: a conversation whose
   * messages carry refs still runs, with each ref-sourced image dropped from the
   * pass and one `attachment_unresolved` warning on the log per dropped part. A
   * host that never writes refs never notices this port exists.
   *
   * Resolution is IN-MEMORY and PER PASS. The stored message always keeps the
   * ref; see {@link AttachmentResolver}.
   */
  attachments?: AttachmentResolver;
  /** Overrides the defaults for what {@link attachments} may add to one pass. */
  attachmentBudgets?: AttachmentBudgets;
  /** Overrides the limits derived from settings + provider capabilities. */
  limits?: AiToolLimits;
  /**
   * Deadlines for the host's own hooks. Absent, the defaults in
   * {@link DEFAULT_HOOK_TIMEOUTS_MS} apply; a non-positive value on any field
   * turns that deadline off. See {@link HookTimeouts}.
   */
  hookTimeoutsMs?: HookTimeouts;
  /**
   * Let a chat run two turns at once. Default `false`, and the default is the
   * safe one.
   *
   * With it off, {@link TurnRunner.submitMessage} and
   * {@link TurnRunner.regenerate} create their task with
   * `exclusiveScope: true`, so a submit into a chat that already has an
   * unfinished turn is refused with `ChatBusyError` (`chat_busy`, HTTP 409) by
   * the STORE, inside the same transaction that would have written it.
   *
   * WHY IT IS THE DEFAULT. Nothing about a second concurrent turn works: its
   * user message takes the active-leaf slot underneath the live run's internal
   * assistant record, so the live run's next chain append lands `active: false`
   * (see `AppendMessageInput.activate`) and its tool results end up off the path
   * every later turn replays — the run answers into a conversation nobody is
   * reading. "The user typed while it was generating" is an everyday event, and
   * a refusal a UI can render ("still answering") is a far better outcome than
   * a conversation that quietly loses half a turn.
   *
   * Turning it ON is for a host that queues turns deliberately and has thought
   * about the interleaving — the tasks still serialize per scope in the queue,
   * so they run one at a time; what changes is that the SECOND one is accepted
   * while the first is live, and its user message is written straight away.
   */
  allowConcurrentSubmit?: boolean;
  clock: Clock;
  ids: IdGenerator;
  logger?: Logger;
  maxToolIterations?: number;
  historyLimit?: number;
}

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
interface TurnRequest {
  chatId: string;
  model?: string;
  providerId?: string;
  assistantMessageId: string;
  userMessageId?: string;
}

/**
 * Mutable state accumulated across one provider pass.
 *
 * It is the projection seam's own state — see {@link RunProjectionState} — not
 * a second model beside it: every field this class reads between passes
 * (`content` and `toolCallIds` for the retry decisions, `lastMessageId` for the
 * banners it chains) is one the projector maintains, and two structures
 * tracking the same run would be two answers to "what has this turn written".
 */
type PassState = RunProjectionState;

interface PassInput {
  task: TaskRecord;
  /** The conversation this turn belongs to, read once from the payload. */
  chatId: string;
  ctx: TaskExecutionContext;
  client: AiProviderClient;
  /** The resolved provider's id — what the usage port bills against. */
  providerId: string;
  model: string;
  messages: AiChatMessage[];
  registry: AiToolRegistry;
  bindings: AiContextBinding[];
  limits: AiToolLimits;
  assistantMessageId: string;
  state: PassState;
  maxToolIterations?: number;
}

interface PassResult {
  terminal: PassTerminal;
  appendedMessages: readonly AiChatMessage[];
}

/**
 * Turns a submitted message into a durable task of kind `chat.turn`, and
 * executes that task.
 *
 * Two halves, deliberately far apart in time:
 *
 * `submitMessage` writes a queued task, the user message and an empty assistant
 * placeholder — in ONE transaction — and returns immediately. The caller gets
 * ids it can render against before a single token exists, and a crash one
 * millisecond later loses nothing, because the work is already recorded. Given
 * a caller-supplied `taskId` it is idempotent: the second submit of one key
 * writes nothing and returns the first submit's ids.
 *
 * `executeTask` is what the executor registry dispatches to (and what the legacy
 * {@link TurnRunner.execute} delegates to). It drives `runChat`, and every event
 * it yields goes two places: appended to the task's durable log (the replayable
 * truth) and projected onto conversation state (what the next turn replays to
 * the provider). Retries stay inside ONE task id, each pass continuing the same
 * `seq` sequence via `TaskStore.nextSeq`, so a consumer that reconnects
 * mid-retry still sees one unbroken, gap-detectable stream.
 */
export class TurnRunner implements TaskWorker {
  /**
   * Whether {@link disposeContributors} has already run. A host wires the call
   * into a signal handler, and a signal can arrive twice.
   */
  private contributorsDisposed = false;

  /**
   * The event → conversation projection, shared verbatim with any host executor
   * that drives a turn of its own. See [`projection.ts`](./projection.ts).
   */
  private readonly projector: RunProjector;

  /** Resolved once: {@link TurnRunnerDeps.hookTimeoutsMs} over the defaults. */
  private readonly hookTimeouts: ResolvedHookTimeouts;

  constructor(private readonly deps: TurnRunnerDeps) {
    this.hookTimeouts = resolveHookTimeouts(deps.hookTimeoutsMs);
    this.projector = createRunProjector({
      store: deps.store,
      clock: deps.clock,
      ...(deps.usage === undefined ? {} : { usage: deps.usage }),
      ...(deps.logger === undefined ? {} : { logger: deps.logger }),
    });
  }

  /**
   * Release every contributor that holds something open — the shutdown half of
   * `ToolSetContributor`'s lifecycle.
   *
   * Idempotent by design: a second call does nothing, because the natural place
   * to wire this is a SIGINT/SIGTERM handler and the second Ctrl-C must not
   * close a client connection twice. A contributor that throws is logged and
   * skipped rather than allowed to strand the ones after it — shutdown that
   * gives up halfway leaks more than the error it reported.
   */
  async disposeContributors(): Promise<void> {
    if (this.contributorsDisposed) return;
    this.contributorsDisposed = true;
    for (const contributor of this.deps.contributors) {
      if (contributor.dispose === undefined) continue;
      try {
        await contributor.dispose();
      } catch (err) {
        this.deps.logger?.warn("tool contributor dispose failed", {
          namespace: contributor.namespace,
          error: err instanceof Error ? err.message : String(err),
        });
      }
    }
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
  async submitMessage(input: SubmitMessageInput): Promise<SubmitMessageResult> {
    const taskId = input.taskId ?? this.deps.ids.taskId();
    const kind = input.kind ?? CHAT_TURN_TASK_KIND;
    const userMessageId = this.deps.ids.messageId();
    const assistantMessageId = this.deps.ids.messageId();
    const payload: TurnRequest = {
      chatId: input.chatId,
      ...(input.model === undefined ? {} : { model: input.model }),
      ...(input.providerId === undefined
        ? {}
        : { providerId: input.providerId }),
      assistantMessageId,
      userMessageId,
    };

    try {
      await this.deps.store.transaction(async (tx) => {
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
          exclusiveScope: this.deps.allowConcurrentSubmit !== true,
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
      const existing = await this.resubmitted(err, { ...input, kind }, taskId);
      if (existing === null) throw err;
      // Re-poked deliberately. `enqueue` is idempotent per the port's contract,
      // so this is a no-op for a task already running or finished — and the
      // rescue for the case the redelivery exists to cover, where the FIRST
      // submit committed its transaction and then died before it could poke.
      await this.deps.taskRunner.enqueue({ taskId, scopeId: input.chatId });
      return existing;
    }

    await this.deps.taskRunner.enqueue({ taskId, scopeId: input.chatId });
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
  private async resubmitted(
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
    const existing = await this.deps.store.tasks.getTask(taskId);
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
   * Mechanically {@link TurnRunner.submitMessage} minus the user message: one
   * transaction writes the task and an empty assistant placeholder, the
   * placeholder hangs off the TARGET'S PARENT (so the store gives it the next
   * `branchIndex` and switches the active path to it in the same write), and
   * the queue is poked afterwards. Idempotent per caller-supplied `taskId` by
   * the same route the submit is.
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
  async regenerate(
    input: RegenerateMessageInput,
  ): Promise<SubmitMessageResult> {
    const parentMessageId = await this.regenerateParentOf(input);
    const taskId = input.taskId ?? this.deps.ids.taskId();
    const kind = input.kind ?? CHAT_TURN_TASK_KIND;
    const assistantMessageId = this.deps.ids.messageId();
    const payload: TurnRequest = {
      chatId: input.chatId,
      ...(input.model === undefined ? {} : { model: input.model }),
      ...(input.providerId === undefined
        ? {}
        : { providerId: input.providerId }),
      assistantMessageId,
      // The question this run answers. It is not a message this call created —
      // that is the difference between a regenerate and a submit — but it is
      // what `resubmitted` reads to answer a redelivery, and what the caller
      // renders the new branch under.
      userMessageId: parentMessageId,
    };

    try {
      await this.deps.store.transaction(async (tx) => {
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
          exclusiveScope: this.deps.allowConcurrentSubmit !== true,
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
      const existing = await this.resubmitted(
        err,
        { ...input, kind, expectUserMessageId: parentMessageId },
        taskId,
      );
      if (existing === null) throw err;
      await this.deps.taskRunner.enqueue({ taskId, scopeId: input.chatId });
      return existing;
    }

    await this.deps.taskRunner.enqueue({ taskId, scopeId: input.chatId });
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
  private async regenerateParentOf(
    input: RegenerateMessageInput,
  ): Promise<string> {
    const siblings = await this.deps.store.conversations.listSiblings(
      input.messageId,
    );
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

  /** Ask the queue to stop a turn; the worker's signal is what actually aborts. */
  async cancel(taskId: string): Promise<void> {
    await this.deps.taskRunner.requestCancel(taskId);
  }

  /**
   * The legacy direct-execute entry point, kept because a host (and this
   * package's own tests) may drive one attempt without standing up a registry.
   *
   * The load-and-guard is {@link loadExecutableTask}, the same call
   * `createDispatchingWorker` makes: on this path there is no dispatcher to have
   * done it, and two hand-written copies of that guard drifting apart is exactly
   * how one entry point ends up re-running a finished turn.
   */
  async execute(execution: TaskExecution): Promise<void> {
    const task = await loadExecutableTask(
      this.deps.store,
      execution.taskId,
      this.deps.clock,
    );

    await this.executeTask({
      task,
      attemptId: execution.attemptId,
      leaseToken: execution.leaseToken,
      signal: execution.signal,
    });
  }

  /**
   * Execute one attempt at one `chat.turn` task, on a record the dispatcher has
   * already loaded and guarded.
   *
   * Everything it can fail on is bookkept the same way: the original error
   * reaches the caller (the queue classifies it), and the task is landed
   * `failed` (or `cancelled`) on the way out so it is never left `running` with
   * nobody executing it — see {@link TurnRunner.landFailure} for the three
   * writes that bookkeeping is, and why an unexpected throw must produce all of
   * them and not just the task row.
   */
  async executeTask(ctx: TaskExecutionContext): Promise<void> {
    const { task } = ctx;
    const payload = task.payload as unknown as Partial<TurnRequest>;
    const chatId = payload.chatId;
    const assistantMessageId = payload.assistantMessageId;
    // A chat.turn task is unexecutable without a chat to answer in AND a
    // placeholder to answer into: with no `assistantMessageId` the turn has
    // nowhere to stream, and every `updateMessage` below would throw mid-run
    // after the model had already been paid for. Both are absent for the same
    // reason — something other than `submitMessage` created this row (a
    // hand-written one, a host that reused the kind for its own work) — and
    // both stay absent on every retry, so this is classified terminal and the
    // queue fails it instead of burning the attempt budget re-reading the same
    // payload.
    if (!isPresent(chatId) || !isPresent(assistantMessageId)) {
      const missing = [
        isPresent(chatId) ? null : "chatId",
        isPresent(assistantMessageId) ? null : "assistantMessageId",
      ].filter((field): field is string => field !== null);
      const error = new AgentKitHostError(
        "invalid_task_payload",
        `Task ${task.taskId} of kind ${CHAT_TURN_TASK_KIND} has no ${missing.join(
          " and no ",
        )} in its payload; only TurnRunner.submitMessage may create tasks of this kind.`,
        { taskId: task.taskId, kind: task.kind, missing },
      );
      // No placeholder is named: with `assistantMessageId` missing there is
      // nothing to finalize, and with `chatId` missing we could not prove a
      // named one belongs to this turn.
      await this.landFailure(ctx, error, error.message);
      throw error;
    }

    const request = payload as TurnRequest;
    try {
      await this.runTurn(ctx, chatId, request, assistantMessageId);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      await this.landFailure(ctx, err, message, assistantMessageId);
      throw err;
    }
  }

  /**
   * Bookkeep an unexpected throw out of a turn: the durable log, the task row,
   * the attempt, and the placeholder — in that order, all best-effort.
   *
   * THE THREE WRITES ARE NOT INTERCHANGEABLE, and before this existed only the
   * middle one happened. A throw from anywhere in `runTurn` — a lease lost, an
   * attachment resolver, the single-shot verifier, `providerFactory`, staging —
   * landed the TASK `failed` and left everything a client can actually see
   * untouched: no terminal event on the run's log, so an SSE consumer watched
   * the stream stop with no explanation; and `placeholder: true` on the
   * assistant record forever, so the UI kept a spinner on a message that will
   * never finish. Worse, a worker that lands its own task opts out of the
   * runner's retries, so nothing was ever coming back to repair it.
   *
   * ORDER, and why:
   *
   *  1. **The terminal event first**, because it is the only one of the three a
   *     live consumer is watching, and it is the write most likely to be
   *     refused (it is fenced, like every other write in an attempt) — sending
   *     it first means a fenced-out attempt does not spend its one chance on
   *     bookkeeping nobody reads.
   *  2. **The fenced task transition**, which is where ownership is actually
   *     proven. `LeaseLostError` here stops the rest: the owner that took this
   *     task over is the one whose verdict counts.
   *  3. **The placeholder**, last and only when the transition landed — the
   *     same ordering the successful terminal block uses, for the same reason
   *     (`ConversationStore` knows nothing about leases, so the only way to
   *     keep a zombie attempt off the live answer is to make it prove ownership
   *     on a write that CAN check, first).
   *
   * `cancelled` rather than `failed` when the run was aborted: a user pressing
   * stop is not a failure, and a task landed `failed` for it shows up as an
   * error in every UI and every retry decision downstream.
   */
  private async landFailure(
    ctx: TaskExecutionContext,
    err: unknown,
    message: string,
    assistantMessageId?: string,
  ): Promise<void> {
    const cancelled = ctx.signal.aborted;
    // `usage_denied` already wrote its own `run.failed`, with the specific code
    // a consumer acts on; a second terminal event here would only overwrite a
    // precise diagnosis with a generic one.
    if (!(err instanceof UsageDeniedError)) {
      await this.emitTerminalQuietly(ctx, err, message, cancelled);
    }
    await this.failQuietly(ctx, message, assistantMessageId);
  }

  /**
   * Put this run's terminal event on the durable log, best-effort.
   *
   * Swallows everything: the caller is already unwinding on a different error,
   * and replacing that diagnosis with "and then the log write failed" helps
   * nobody. The `errorCode` is the host error's own `code` when it has one —
   * `no_model`, `lease_lost`, `invalid_task_payload` — because that is what a
   * client switches on; anything else is `internal_error` rather than a message
   * dressed up as a code.
   */
  private async emitTerminalQuietly(
    ctx: TaskExecutionContext,
    err: unknown,
    message: string,
    cancelled: boolean,
  ): Promise<void> {
    const draft: AiRunEventDraft = cancelled
      ? {
          type: "run.cancelled",
          runId: ctx.task.taskId,
          timestamp: this.deps.clock.nowIso(),
          data: { reason: message },
        }
      : {
          type: "run.failed",
          runId: ctx.task.taskId,
          timestamp: this.deps.clock.nowIso(),
          data: {
            errorMessage: message,
            errorCode:
              err instanceof AgentKitHostError ? err.code : "internal_error",
          },
        };
    try {
      await this.appendHostEvent(ctx, draft);
    } catch (appendErr) {
      this.deps.logger?.warn("could not record run terminal after error", {
        taskId: ctx.task.taskId,
        attemptId: ctx.attemptId,
        error:
          appendErr instanceof Error ? appendErr.message : String(appendErr),
      });
    }
  }

  private async runTurn(
    ctx: TaskExecutionContext,
    chatId: string,
    request: TurnRequest,
    assistantMessageId: string,
  ): Promise<void> {
    const { store, clock } = this.deps;
    const { task } = ctx;
    const settings = await store.settings.getSettings();
    const provider = await this.resolveProvider(request, settings);
    const model =
      request.model ?? provider.defaultModel ?? settings.defaultModel;
    if (!model) {
      throw new AgentKitHostError(
        "no_model",
        `No model resolved for task ${task.taskId}.`,
        { taskId: task.taskId, providerId: provider.id },
      );
    }
    const capabilities = await store.providers.getCapabilities(provider.id);
    const limits =
      this.deps.limits ??
      resolveToolLimits({
        preference: settings.contextSizePreference,
        ...(capabilities?.maxContextTokens === undefined
          ? {}
          : { modelContextTokens: capabilities.maxContextTokens }),
      });

    const bindings = await this.resolveBindings(ctx, chatId);
    const hasPrimaryBinding = bindings.some(
      (binding) => binding.role === "primary" && binding.status === "active",
    );

    // A provider probed as chat-only must not be handed tools at all: doing so
    // is the exact request shape that fails, and the chat-only retry exists to
    // recover from discovering that the hard way. The setting overrides the
    // probe in BOTH directions — see `AssistantSettings.toolCalling`.
    const toolCalling = settings.toolCalling ?? "auto";
    const stageTools =
      toolCalling === "off"
        ? false
        : toolCalling === "on" || capabilities?.toolCalling !== false;
    const staged = stageTools
      ? await stageRegistry({
          contributors: this.deps.contributors,
          ctx: {
            chatId,
            runId: task.taskId,
            scopeId: task.scopeId,
            bindings,
            limits,
            signal: ctx.signal,
            ...(this.deps.logger === undefined
              ? {}
              : { logger: this.deps.logger }),
          },
          hasPrimaryBinding,
          contributeTimeoutMs: this.hookTimeouts.contribute,
          ...(this.deps.toolGuards === undefined
            ? {}
            : { guards: this.deps.toolGuards }),
        })
      : {
          registry: new AiToolRegistry(),
          namespaces: new Map<string, string>(),
          failed: [],
        };
    const registry = staged.registry;
    // A contributor that could not answer costs its tools and nothing else
    // (see `stageRegistry`), and `stageRegistry` has already logged why. A
    // TIMED-OUT one additionally goes on the durable log: `hook_timeout` is the
    // vocabulary for "the turn went on without what a hook would have
    // contributed", and a shrunken tool set looks, to the user reading the
    // conversation, exactly like a model that stopped bothering to use its
    // tools. A contributor that THREW stays a log line: the thrown text is host
    // code's, and this warning is read by clients.
    for (const contributor of staged.failed) {
      if (!contributor.timedOut) continue;
      await this.emitWarning(
        ctx,
        "hook_timeout",
        `Tools from "${contributor.namespace}" are not available for this turn: it did not finish contributing in time.`,
      );
    }
    const registryHadTools = registry.size() > 0;
    // The names actually staged — what the emulated-call detector checks a
    // printed call against, so a model quoting a `{"name", "parameters"}`
    // example for something that is not a tool here is not reported as one.
    const stagedToolNames = new Set(staged.namespaces.keys());

    const client = this.deps.providerFactory(await this.withSecret(provider));
    const systemPrompt = await this.resolveSystemPrompt(ctx, chatId);
    const assembled = await this.assembleMessages(
      chatId,
      assistantMessageId,
      systemPrompt,
    );
    // Snapshot before the first pass: the empty-response retry re-asks the
    // ORIGINAL question, not the question plus whatever the failed turn left
    // behind.
    const initialMessages = assembled.slice();

    const state: PassState = this.projector.createState({
      chatId,
      assistantMessageId,
      providerId: provider.id,
    });
    // RESUME THIS RUN'S OWN CHAIN. On attempt 1 the deepest record carrying
    // this run id IS the placeholder the projector already seeded, so nothing
    // changes. On attempt 2 of a crashed turn it is whatever attempt 1 wrote
    // last, and chaining there is what keeps attempt 2's records on the active
    // path — the placeholder by then has an active child, and a chain append
    // under a parent that already has one lands `active: false`, taking every
    // record after it off the path each later turn replays. See
    // `ConversationStore.lastMessageOfRun`.
    const resumed = await store.conversations.lastMessageOfRun(
      chatId,
      task.taskId,
    );
    if (resumed !== null) state.lastMessageId = resumed.id;
    const basePass = {
      task,
      chatId,
      ctx,
      client,
      providerId: provider.id,
      model,
      bindings,
      limits,
      assistantMessageId,
      state,
    };

    const first = await this.runPass({
      ...basePass,
      messages: assembled,
      registry,
      ...(this.deps.maxToolIterations === undefined
        ? {}
        : { maxToolIterations: this.deps.maxToolIterations }),
    });
    let terminal: PassTerminal = first.terminal;
    // How many provider passes this run has made. The pass just run is 1; every
    // recovery and correction pass announces itself as the next number, so a
    // log reader can tell which terminal event belongs to which attempt at the
    // answer. See `emitPassBoundary`.
    let passesRun = 1;

    if (shouldRetryChatOnly({ terminal, registryHadTools })) {
      passesRun += 1;
      await this.emitPassBoundary(
        ctx,
        passesRun,
        "chat_only",
        "The provider rejected the request with tools attached; re-asking without them.",
      );
      // Start the answer over: the failed pass may have streamed half a
      // sentence, and appending a second attempt to it would read as one
      // rambling reply.
      this.resetPass(state);
      await this.deps.store.conversations.updateMessage(assistantMessageId, {
        content: "",
      });
      const retryMessages = filterToolTurns([
        ...assembled,
        ...first.appendedMessages,
      ]);
      const retry = await this.runPass({
        ...basePass,
        messages: retryMessages,
        registry: new AiToolRegistry(),
        maxToolIterations: RETRY_MAX_TOOL_ITERATIONS,
      });
      terminal = retry.terminal;
    } else if (
      shouldRetryEmptyResponse({
        terminal,
        toolCallCount: state.toolCallIds.size,
        hadContent: hasContent(state),
      })
    ) {
      passesRun += 1;
      await this.emitPassBoundary(
        ctx,
        passesRun,
        "empty_response",
        "The model completed the turn without an answer; asking the original question once more.",
      );
      this.resetPass(state);
      const retry = await this.runPass({
        ...basePass,
        messages: filterToolTurns(initialMessages),
        registry: new AiToolRegistry(),
        maxToolIterations: RETRY_MAX_TOOL_ITERATIONS,
      });
      terminal = retry.terminal;
    }

    const toolCallCount = state.toolCallIds.size;
    const finalContent = state.content;

    // Still nothing, after every recovery pass. A cancelled turn is exempt: it
    // has no answer because it was stopped, which is not the same failure.
    if (terminal !== "cancelled" && !hasContent(state) && toolCallCount === 0) {
      await this.emitWarning(
        ctx,
        "empty_response",
        "The model returned no answer.",
      );
    }

    if (
      registryHadTools &&
      toolCallCount === 0 &&
      looksLikeEmulatedToolCall(finalContent, stagedToolNames)
    ) {
      await this.emitWarning(
        ctx,
        "emulated_tool_call",
        EMULATED_TOOL_CALL_MESSAGE,
      );
      state.lastMessageId = (
        await store.conversations.appendMessage({
          chatId,
          runId: task.taskId,
          role: "system",
          content: EMULATED_TOOL_CALL_MESSAGE,
          parentMessageId: state.lastMessageId,
          activate: false,
          metadata: { banner: "emulated_tool_call" },
        })
      ).id;
    }

    // Verification runs only when the turn actually did tool work — there is
    // nothing to verify about a chat answer.
    //
    // TWO SHAPES, and which one runs is the host's choice, not a heuristic.
    // Without `deps.correction` this is the SINGLE check it has always been:
    // one `verify()`, a banner if it did not pass, no events, no second
    // provider call — a `verify()` that throws still fails the turn, because
    // that is what it did before and a host relying on it has not asked for
    // anything else — it is now BOUNDED by `hookTimeoutsMs.verify`, so a
    // verifier that hangs fails the turn instead of holding the lease forever,
    // but a verifier that answers behaves exactly as before. With
    // `deps.correction` the harness takes over and the rules change
    // deliberately; see `runCorrectionHarness`.
    if (this.deps.verification && toolCallCount > 0) {
      if (this.deps.correction === undefined) {
        const verification = this.deps.verification;
        const report = await withHookDeadline({
          hook: "verify",
          timeoutMs: this.hookTimeouts.verify,
          run: () =>
            verification.verify({
              runId: task.taskId,
              chatId,
              scopeId: task.scopeId,
              attemptId: ctx.attemptId,
              toolCallCount,
              finalContent,
              signal: ctx.signal,
            }),
        });
        if (report && report.status !== "pass") {
          state.lastMessageId = (
            await store.conversations.appendMessage({
              chatId,
              runId: task.taskId,
              role: "system",
              content: describeDeficiencies(report.deficiencies),
              parentMessageId: state.lastMessageId,
              activate: false,
              metadata: { banner: "verification", status: report.status },
            })
          ).id;
        }
      } else {
        terminal = await this.runCorrectionHarness({
          basePass,
          registry,
          systemPrompt,
          // The request this run is answering, so a correction pass knows WHAT
          // was asked and not only that its answer fell short. Read off the
          // assembled history rather than the payload's `userMessageId`,
          // because that is the message the provider actually saw — a
          // regenerate has no user message of its own, and a history window
          // that moved past the question would leave nothing to replay.
          userRequest:
            this.deps.correction.includeUserRequest === false
              ? null
              : lastUserRequestOf(assembled),
          verification: this.deps.verification,
          maxPasses: resolveMaxCorrectionPasses(this.deps.correction),
          terminal,
          passesRun,
        });
      }
    }

    const finalStatus: TaskStatus =
      terminal === "completed"
        ? "completed"
        : terminal === "cancelled"
          ? "cancelled"
          : "failed";
    // THE FENCED TASK TRANSITION GOES FIRST, and the placeholder write last.
    // `ConversationStore` knows nothing about leases, so the only way to keep a
    // zombie attempt — one whose lease expired mid-tool-call, with recovery
    // already running attempt 2 — from overwriting the live attempt's answer is
    // to make it prove ownership on a write that CAN check, before it touches
    // the message. `LeaseLostError` from here therefore aborts the whole block:
    // the placeholder is left for the owner that actually holds the task, and
    // the error propagates so the queue classifies this attempt as lost rather
    // than finished.
    await store.tasks.transitionTask(
      task.taskId,
      ["running"],
      finalStatus,
      { finishedAt: clock.nowIso() },
      { leaseToken: ctx.leaseToken },
    );
    await store.tasks.endAttempt({
      attemptId: ctx.attemptId,
      status: finalStatus,
      leaseToken: ctx.leaseToken,
    });
    // `state.content` rather than the `finalContent` snapshot above: a
    // correction pass rewrites the visible answer, and the snapshot predates it.
    // With no harness the two are the same string — nothing between them touches
    // the state — so this is not a behaviour change for anyone not using it.
    await store.conversations.updateMessage(assistantMessageId, {
      content: state.content,
      metadata: { placeholder: false },
    });
  }

  /**
   * The multi-pass correction harness: verify, hand the deficiencies back, let
   * the model fix them with its tools, verify again — bounded three ways.
   *
   * THE LOOP. Pass 0 verifies the run's own answer; each `run.verification`
   * event names its pass number, so a log reader can tell "verified once and it
   * was fine" from "corrected twice and it still is not". A correction pass is a
   * full {@link runPass} on the SAME registry and the same task log: tools
   * staged exactly as the run had them, `seq` continuing unbroken,
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
  private async runCorrectionHarness(input: {
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
  }): Promise<PassTerminal> {
    const { basePass, registry, systemPrompt, verification, maxPasses } = input;
    const { ctx, state, chatId, task, assistantMessageId } = basePass;
    const { store } = this.deps;

    let terminal = input.terminal;
    let pass = 0;
    let previousDeficiencies: readonly string[] | undefined;
    let lastReport: DeficiencyReport | null = null;

    for (;;) {
      const report = await this.verifyQuietly(verification, {
        runId: task.taskId,
        chatId,
        scopeId: task.scopeId,
        attemptId: ctx.attemptId,
        toolCallCount: state.toolCallIds.size,
        finalContent: state.content,
        signal: ctx.signal,
      });
      if (report === null) {
        await this.emitVerification(ctx, pass, "unavailable", []);
        break;
      }
      lastReport = report;
      await this.emitVerification(
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
      await this.emitPassBoundary(
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
      this.resetPass(state);
      await store.conversations.updateMessage(assistantMessageId, {
        content: "",
      });
      const corrected = await this.runPass({
        ...basePass,
        messages,
        registry,
        ...(this.deps.maxToolIterations === undefined
          ? {}
          : { maxToolIterations: this.deps.maxToolIterations }),
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
  private async verifyQuietly(
    verification: VerificationHook,
    input: VerificationInput,
  ): Promise<DeficiencyReport | null> {
    try {
      return await withHookDeadline({
        hook: "verify",
        timeoutMs: this.hookTimeouts.verify,
        run: () => verification.verify(input),
      });
    } catch (err) {
      this.deps.logger?.warn("verification hook failed", {
        taskId: input.runId,
        chatId: input.chatId,
        error: err instanceof Error ? err.message : String(err),
      });
      return null;
    }
  }

  /** One `run.verification` event on the run's durable log. */
  private async emitVerification(
    ctx: TaskExecutionContext,
    pass: number,
    status: "pass" | "partial" | "unavailable",
    deficiencies: readonly string[],
  ): Promise<void> {
    await this.appendHostEvent(ctx, {
      type: "run.verification",
      runId: ctx.task.taskId,
      timestamp: this.deps.clock.nowIso(),
      data: { pass, status, deficiencies: [...deficiencies] },
    });
  }

  /**
   * One `runChat` invocation, with its events appended and projected.
   *
   * `firstSeq` comes from the store rather than from a counter in this class:
   * several passes share a task id, and only the log knows where the last one
   * stopped. That is what keeps `seq` unbroken across a retry — and what lets a
   * resumed process continue a stream it did not start.
   *
   * The spend check is the first thing in here, ahead of even reading the
   * sequence: this method is the ONE place a provider call is made, so gating it
   * here is what makes "every pass is authorized" true by construction rather
   * than by remembering to add a check to each of the three call sites.
   */
  private async runPass(input: PassInput): Promise<PassResult> {
    await this.authorizeUsage(input);
    // Before `firstSeq` is taken, because resolving may emit warnings of its
    // own and those must land BEFORE the pass's events — the sequence is one
    // unbroken run of numbers, and a warning stamped after `runChat` reserved
    // its start would collide with the pass's first event.
    const messages = await this.resolveAttachments(input);
    const firstSeq = await this.deps.store.tasks.nextSeq(input.task.taskId);
    const generator = runChat({
      client: input.client,
      registry: input.registry,
      model: input.model,
      messages,
      bindings: input.bindings,
      limits: input.limits,
      chatId: input.chatId,
      // The task's own scope, threaded to every tool: a write tool stages
      // against it, and re-deriving it downstream is how a host writing a
      // shared document ends up with two different answers.
      scopeId: input.task.scopeId,
      runId: input.task.taskId,
      attemptId: input.ctx.attemptId,
      firstSeq,
      signal: input.ctx.signal,
      ...(input.maxToolIterations === undefined
        ? {}
        : { maxToolIterations: input.maxToolIterations }),
    });

    for (;;) {
      const next = await generator.next();
      if (next.done) {
        return {
          terminal: next.value.terminal,
          appendedMessages: next.value.appendedMessages,
        };
      }
      await this.projectEvent(next.value, input);
    }
  }

  /**
   * Ask the {@link UsageAuthorizer} whether this pass may call the provider, and
   * end the run if the answer is no.
   *
   * A refusal is recorded before it is thrown: a `run.failed` event carrying
   * `errorCode: "usage_denied"` goes into the task's durable log, so a client
   * already following the SSE stream learns WHY its turn stopped instead of
   * watching the stream close on a status change it has to go and read. The
   * throw then lands the task through `executeTask`'s existing failure path —
   * one place decides what a failed turn looks like.
   *
   * The estimate is deliberately crude (assembled content chars / 4) and
   * deliberately optional on the port: a real token count needs the provider's
   * own tokenizer, which is not something this layer has, and a host with a
   * better one is free to ignore the number. What the port is actually being
   * asked is "is there budget left at all", and that question does not need the
   * estimate to be right to be worth asking.
   */
  private async authorizeUsage(input: PassInput): Promise<void> {
    const usage = this.deps.usage;
    if (usage === undefined) return;

    const decision = await usage.authorize({
      runId: input.task.taskId,
      chatId: input.chatId,
      providerId: input.providerId,
      model: input.model,
      estimatedPromptTokens: estimatePromptTokens(input.messages),
    });
    if (decision.allowed) return;

    const reason = decision.reason ?? "no reason given";
    const error = new UsageDeniedError(
      `Usage authorizer refused a provider call for task ${input.task.taskId}: ${reason}`,
      {
        taskId: input.task.taskId,
        chatId: input.chatId,
        providerId: input.providerId,
        model: input.model,
        ...(decision.retryAfterMs === undefined
          ? {}
          : { retryAfterMs: decision.retryAfterMs }),
      },
    );
    await this.appendHostEvent(input.ctx, {
      type: "run.failed",
      runId: input.task.taskId,
      timestamp: this.deps.clock.nowIso(),
      data: { errorMessage: error.message, errorCode: "usage_denied" },
    });
    throw error;
  }

  /**
   * Append an event to the durable log, then reflect it in conversation state.
   *
   * DELEGATED, whole, to [`projection.ts`](./projection.ts) — the seam a host
   * executor of its own kind drives to produce the identical conversation from
   * events this class never saw. There is no copy of the rules here, which is
   * what makes "a custom turn executor writes what `chat.turn` writes" true by
   * construction rather than by two implementations agreeing today.
   */
  private async projectEvent(
    event: AiRunEvent,
    input: PassInput,
  ): Promise<void> {
    await this.projector.project(
      {
        task: input.task,
        attemptId: input.ctx.attemptId,
        leaseToken: input.ctx.leaseToken,
      },
      input.state,
      event,
    );
  }

  /**
   * The chat's context bindings, under the context hook's deadline.
   *
   * A `ContextProvider` is host code reading host state — a document index, a
   * workspace, an editor — and before it was bounded, one blocked on a socket
   * parked the turn indefinitely with the lease renewing underneath it, which
   * also made the chat undeletable. The DEGRADED answer is no bindings, which
   * is a state the rest of this class already handles: it is what a host with
   * no `ContextProvider` wired gets, and `hasPrimaryBinding` false prunes the
   * tools that need a target. The warning is what stops that reading as "the
   * user had nothing open".
   *
   * `refresh` and `listBindings` share ONE deadline rather than getting one
   * each, because they are one logical read: refreshing and then listing is how
   * a provider is asked "what is open right now", and giving the pair 2× the
   * budget would make the configured number mean something other than what it
   * says.
   */
  private async resolveBindings(
    ctx: TaskExecutionContext,
    chatId: string,
  ): Promise<AiContextBinding[]> {
    const context = this.deps.context;
    if (context === undefined) return [];
    try {
      return await withHookDeadline({
        hook: "context.listBindings",
        timeoutMs: this.hookTimeouts.context,
        run: async () => {
          await context.refresh?.(chatId, ctx.signal);
          return (await context.listBindings(chatId, ctx.signal)) ?? [];
        },
      });
    } catch (err) {
      if (!isHookTimeout(err)) throw err;
      await this.emitWarning(
        ctx,
        "hook_timeout",
        "The context provider did not answer in time; this turn ran with no context bindings.",
      );
      return [];
    }
  }

  /**
   * The chat's system prompt, under the same deadline as the bindings.
   *
   * Degrades to NO system prompt, which is the same shape a host that never
   * wired `systemPrompt` produces — a turn without instructions is a worse turn
   * but still a turn, where a turn that never returns is neither.
   */
  private async resolveSystemPrompt(
    ctx: TaskExecutionContext,
    chatId: string,
  ): Promise<string | null> {
    const systemPrompt = this.deps.context?.systemPrompt;
    if (systemPrompt === undefined) return null;
    const context = this.deps.context as ContextProvider;
    try {
      return await withHookDeadline({
        hook: "context.systemPrompt",
        timeoutMs: this.hookTimeouts.context,
        run: async () =>
          (await systemPrompt.call(context, chatId, ctx.signal)) ?? null,
      });
    } catch (err) {
      if (!isHookTimeout(err)) throw err;
      await this.emitWarning(
        ctx,
        "hook_timeout",
        "The context provider did not produce a system prompt in time; this turn ran without one.",
      );
      return null;
    }
  }

  /**
   * Replace every `ref` image source in this pass's history with the bytes
   * behind it — in memory, for this pass only.
   *
   * WHAT IS NOT TOUCHED: the stored message. A ref is what the conversation
   * holds, and re-resolving it on the next pass is the whole point — an
   * attachment can be revoked, replaced, or become too large for a budget that
   * changed, and a record that had already been rewritten to base64 could not
   * notice any of it. Nothing here writes to `ConversationStore`.
   *
   * WHAT A DROP LOOKS LIKE. An image that cannot be sent is removed from the
   * message the provider sees, and the turn continues with the words around it —
   * the same "degrade, never fail a request over an attachment" rule the
   * provider client follows when it flattens parts on a `system` message. Each
   * dropped part gets exactly one durable `run.warning` naming its ref and why,
   * so a UI can say "this image was not sent" instead of quietly answering a
   * question about a picture the model never saw. A message whose parts are ALL
   * dropped becomes the empty STRING rather than an empty array: `content: []`
   * is a shape the contract rejects and providers reject.
   *
   * THE CACHE IS PER PASS, deliberately. The same ref mentioned twice in one
   * history costs one `resolve()`; the retry pass that follows a failed one asks
   * again, because "these bytes are still there" is not a fact that survives an
   * arbitrary amount of time and a provider round-trip.
   *
   * A history with no refs in it returns the caller's own array untouched — the
   * overwhelmingly common case allocates nothing and asks the port nothing.
   */
  private async resolveAttachments(
    input: PassInput,
  ): Promise<readonly AiChatMessage[]> {
    if (!input.messages.some((message) => hasRefImage(message.content))) {
      return input.messages;
    }
    const budgets = this.resolveBudgets();
    const resolver = this.deps.attachments;
    const cache = new Map<string, ResolvedAttachment | null>();
    let totalBytes = 0;
    let images = 0;

    const resolved: AiChatMessage[] = [];
    for (const message of input.messages) {
      if (!hasRefImage(message.content)) {
        resolved.push(message);
        continue;
      }
      const parts: AiContentPart[] = [];
      // `content` is narrowed to a parts array by `hasRefImage`.
      for (const part of message.content as AiContentPart[]) {
        if (part.type !== "image" || part.source.kind !== "ref") {
          parts.push(part);
          continue;
        }
        const ref = part.source.ref;
        if (resolver === undefined) {
          await this.emitWarning(
            input.ctx,
            "attachment_unresolved",
            `Attachment "${ref}" was dropped from this pass: no AttachmentResolver is wired.`,
          );
          continue;
        }
        if (!cache.has(ref)) {
          // The chat is passed so a multi-tenant host can SCOPE the lookup:
          // refs come from the client, so "may this chat see it" is the actual
          // question — see {@link AttachmentResolver.resolve}.
          //
          // UNDER A DEADLINE, and a timeout reads as "no bytes": a resolver
          // that has not answered is not a resolver that said yes, and the
          // drop path below is exactly the degradation this port already
          // documents. The `null` is cached with it, so a history mentioning
          // the same ref five times waits once rather than five times.
          cache.set(
            ref,
            await this.resolveAttachmentQuietly(resolver, ref, input.chatId),
          );
        }
        const attachment = cache.get(ref) ?? null;
        if (attachment === null) {
          await this.emitWarning(
            input.ctx,
            "attachment_unresolved",
            `Attachment "${ref}" was dropped from this pass: the resolver has no bytes for it.`,
          );
          continue;
        }
        const bytes = decodedByteLength(attachment.base64);
        const refusal = budgetRefusal({
          bytes,
          totalBytes,
          images,
          budgets,
        });
        if (refusal !== null) {
          await this.emitWarning(
            input.ctx,
            "attachment_budget_exceeded",
            `Attachment "${ref}" was dropped from this pass: ${refusal}`,
          );
          continue;
        }
        totalBytes += bytes;
        images += 1;
        parts.push({
          ...part,
          source: {
            kind: "data",
            base64: attachment.base64,
            mediaType: attachment.mediaType,
          },
        });
      }
      resolved.push({ ...message, content: parts.length === 0 ? "" : parts });
    }
    return resolved;
  }

  /**
   * One `resolve()` under the attachment deadline; a timeout answers `null`.
   *
   * Only the DEADLINE is folded — a resolver that THROWS still throws, exactly
   * as before, because a host that raised an authorization error from this port
   * meant the turn to stop. What is folded is the one outcome that used to have
   * no answer at all.
   */
  private async resolveAttachmentQuietly(
    resolver: AttachmentResolver,
    ref: string,
    chatId: string,
  ): Promise<ResolvedAttachment | null> {
    try {
      return await withHookDeadline({
        hook: "attachments.resolve",
        timeoutMs: this.hookTimeouts.attachments,
        run: () => resolver.resolve(ref, { chatId }),
      });
    } catch (err) {
      if (!isHookTimeout(err)) throw err;
      this.deps.logger?.warn("attachment resolver timed out", {
        chatId,
        ref,
        timeoutMs: this.hookTimeouts.attachments,
      });
      return null;
    }
  }

  private resolveBudgets(): ResolvedBudgets {
    const configured = this.deps.attachmentBudgets;
    return {
      maxBytesPerImage:
        configured?.maxBytesPerImage ?? DEFAULT_MAX_BYTES_PER_IMAGE,
      maxTotalBytes: configured?.maxTotalBytes ?? DEFAULT_MAX_TOTAL_BYTES,
      maxImages: configured?.maxImages ?? DEFAULT_MAX_IMAGES,
    };
  }

  /**
   * Stamp and append a host-originated warning, continuing the task's sequence.
   *
   * Stamped with core's `createEventStamper` rather than
   * `createTaskEventWriter`, because these warnings are emitted BETWEEN passes
   * and must speak the same `AiRunEvent` vocabulary the passes around them do.
   */
  private async emitWarning(
    ctx: TaskExecutionContext,
    code: string,
    message: string,
  ): Promise<void> {
    await this.appendHostEvent(ctx, {
      type: "run.warning",
      runId: ctx.task.taskId,
      timestamp: this.deps.clock.nowIso(),
      data: { code, message },
    });
  }

  /**
   * Stamp one host-originated event onto the task's log, continuing its `seq`.
   *
   * Shared by the between-pass warnings and the usage refusal so there is one
   * answer to "where does the next seq come from" — two copies of this that
   * disagreed would put a gap or a repeat in the very sequence a consumer uses
   * to detect gaps and repeats.
   */
  private async appendHostEvent(
    ctx: TaskExecutionContext,
    draft: AiRunEventDraft,
  ): Promise<void> {
    const taskId = ctx.task.taskId;
    const firstSeq = await this.deps.store.tasks.nextSeq(taskId);
    const stamp = createEventStamper({
      firstSeq,
      attemptId: ctx.attemptId,
    });
    await this.deps.store.tasks.appendEvents(taskId, [stamp(draft)], {
      leaseToken: ctx.leaseToken,
    });
  }

  /**
   * Announce the pass about to run, on the durable log, BEFORE it runs.
   *
   * A run is not one pass, and every pass writes its own `run.started` …
   * terminal pair onto the same log. Without a marker between them a consumer
   * following the stream reads the FIRST terminal as the run's answer: a turn
   * whose pass 1 failed and whose pass 2 completed is reported failed, and a
   * UI that has been concatenating deltas shows pass 1's half-sentence glued to
   * pass 2's answer. This event is the boundary — `data.pass` is the 1-based
   * number of the pass about to run, `data.reason` says why it is running — and
   * a consumer treats it as "the run is live again, drop the text so far",
   * mirroring the reset this class performs on the stored placeholder
   * immediately afterwards.
   *
   * Emitted BEFORE `resetPass` and before the pass's own events, so the order
   * on the log is the order a consumer has to apply them in.
   */
  private async emitPassBoundary(
    ctx: TaskExecutionContext,
    pass: number,
    reason: "chat_only" | "empty_response" | "correction",
    message: string,
  ): Promise<void> {
    await this.appendHostEvent(ctx, {
      type: "run.warning",
      runId: ctx.task.taskId,
      timestamp: this.deps.clock.nowIso(),
      data: { code: "retry_pass", message, pass, reason },
    });
  }

  /**
   * Clear the answer-so-far before a recovery or correction pass re-answers
   * from scratch.
   *
   * THE TOOL-CALL IDS GO TOO, because every reader of them asks about the pass
   * that produced the answer, not about the run's history: the `empty_response`
   * warning ("this pass returned nothing and called nothing"), the
   * emulated-call detector ("it described a tool call instead of making one"),
   * and `VerificationInput.toolCallCount` ("was there tool work to verify?").
   * The chat-only and empty-response retries run with an EMPTY registry and
   * cannot call a tool at all, so carrying pass 1's ids across suppressed both
   * signals for a pass that made no calls, and ran verification on a pass that
   * did no tool work — over results that pass had already filtered out of its
   * own history.
   */
  private resetPass(state: PassState): void {
    state.content = "";
    state.streamed = false;
    state.toolCallIds.clear();
    delete state.pendingAssistantMessageId;
    state.pendingToolCalls = [];
    state.announcedToolCalls = [];
    // DISCARD the coalesced placeholder write rather than letting it land: the
    // caller blanks the record right after this, and a pending flush applied
    // afterwards would put the abandoned pass's text straight back. See
    // `RunProjectionState.unflushedDeltas`.
    state.unflushedDeltas = 0;
    // Re-stamped, so the next pass coalesces from NOW. Left alone, the
    // elapsed-since-last-flush check reads the moment the previous pass last
    // wrote — long past the interval by the time a retry starts — and every
    // recovery pass flushed its very first delta straight to the store.
    state.lastFlushAtMs = this.deps.clock.now().getTime();
  }

  /**
   * Assemble the provider-facing conversation from stored records.
   *
   * The placeholder is skipped — it is where this turn's answer is being
   * written, and feeding a model its own empty (or half-written) reply is how a
   * turn ends up completing someone else's sentence. `role: "system"` records
   * are skipped too: those are UI banners the host wrote about the turn, not
   * prompt material — as are the correction harness's write-backs, which were
   * instructions to one pass of one run and not standing orders (below).
   *
   * The records come from `listMessages`, which reports the chat's ACTIVE PATH —
   * so a branch submit replays the branch and not the answer it replaced, with
   * no branch filtering needed here. `orderMessagesForProvider` runs over them
   * unchanged: `orderKey` increases with depth along any path, so ordering the
   * active path by `orderKey` and ordering it by depth are the same order, and
   * the run-scoped tool-call linkage it restores is untouched by branching.
   *
   * The result is then balanced in BOTH directions before it leaves: a tool
   * result whose requesting turn fell outside the window is dropped (below), and
   * a tool call whose result never got written is answered with a synthetic
   * failure (see {@link reconcileOrphanToolCalls}). Either imbalance is rejected
   * outright by every provider, and both are reachable — the first from the
   * history limit, the second from a turn that died between two writes.
   */
  private async assembleMessages(
    chatId: string,
    assistantMessageId: string,
    systemPrompt: string | null,
  ): Promise<AiChatMessage[]> {
    const records = await this.deps.store.conversations.listMessages(chatId, {
      limit: this.deps.historyLimit ?? DEFAULT_HISTORY_LIMIT,
    });
    const ordered = orderMessagesForProvider(records);
    const messages: AiChatMessage[] = [];
    if (systemPrompt) messages.push({ role: "system", content: systemPrompt });
    /**
     * Tool call ids an assistant turn in THIS window actually declared. A tool
     * result whose requesting turn fell outside the history limit is an orphan
     * `tool_call_id`, which providers reject outright — so the window silently
     * moving past an assistant turn must not take the whole conversation down
     * with it.
     */
    const declaredToolCallIds = new Set<string>();
    for (const record of ordered) {
      if (record.id === assistantMessageId) continue;
      // A correction write-back is an instruction the harness aimed at ONE pass
      // of ONE run ("fix these three items now, by calling your tools"). It is
      // persisted for the audit trail — the stored history has to say why the
      // model changed its answer — but replaying it here would hand every later
      // turn a dangling order about deficiencies that were already addressed,
      // with nothing left in view to address. The harness's own passes are
      // unaffected: they build their messages directly, not from this history.
      if (record.metadata["correctionPass"] !== undefined) continue;
      if (record.role === "user") {
        messages.push({ role: "user", content: record.content });
      } else if (record.role === "assistant") {
        for (const call of record.toolCalls ?? []) {
          declaredToolCallIds.add(call.id);
        }
        messages.push({
          role: "assistant",
          content: record.content,
          ...(record.toolCalls && record.toolCalls.length > 0
            ? { toolCalls: record.toolCalls }
            : {}),
        });
      } else if (
        record.role === "tool" &&
        record.toolCallId !== undefined &&
        declaredToolCallIds.has(record.toolCallId)
      ) {
        const toolName = record.metadata["toolName"];
        messages.push({
          role: "tool",
          content: record.content,
          toolCallId: record.toolCallId,
          ...(typeof toolName === "string" ? { name: toolName } : {}),
        });
      }
    }
    return reconcileOrphanToolCalls(messages);
  }

  private async resolveProvider(
    request: TurnRequest,
    settings: AssistantSettings,
  ): Promise<AiProviderConfig> {
    const providerId = request.providerId ?? settings.defaultProviderId;
    if (providerId !== undefined) {
      const provider = await this.deps.store.providers.getProvider(providerId);
      if (!provider) {
        throw new RecordNotFoundError(`Provider not found: ${providerId}`, {
          providerId,
        });
      }
      return provider;
    }
    const enabled = (await this.deps.store.providers.listProviders()).find(
      (candidate) => candidate.enabled,
    );
    if (!enabled) {
      throw new AgentKitHostError(
        "no_provider",
        "No enabled provider is configured.",
      );
    }
    return enabled;
  }

  /**
   * Inject the API key at the last possible moment, from the secret store. The
   * config that travels through the rest of the system carries only a ref.
   */
  private async withSecret(
    provider: AiProviderConfig,
  ): Promise<AiProviderConfig> {
    const ref = provider.metadata?.[PROVIDER_SECRET_REF_KEY];
    if (!this.deps.secrets || typeof ref !== "string") return provider;
    const apiKey = await this.deps.secrets.get(ref);
    return apiKey === null ? provider : { ...provider, apiKey };
  }

  /**
   * Best-effort failure bookkeeping on an unexpected throw. Every step is
   * guarded: the original error is what the caller must see, and a secondary
   * failure while recording it would replace the diagnosis with noise.
   *
   * SAME ORDER AS THE TERMINAL BLOCK, for the same reason: the fenced task
   * transition is the one write that can prove this attempt still owns the
   * task, so it goes first, and a `LeaseLostError` from it stops the rest. An
   * attempt that has been fenced out must record nothing — not even a failure —
   * because the owner that took the task over is the one whose verdict counts,
   * and `abandoned` (which recovery already wrote for this attempt) is the
   * honest description of what happened here.
   *
   * THE PLACEHOLDER IS FINALIZED ON EITHER PROOF OF OWNERSHIP: this attempt
   * landed the task, or the fenced `endAttempt` succeeded on a task somebody
   * landed OUT OF BAND (a host transition, an operator cancel). Requiring the
   * first alone left the second case with `placeholder: true` forever — the
   * task cancelled, the run over, and a UI still spinning on a message nothing
   * was coming back to finish.
   */
  private async failQuietly(
    ctx: TaskExecutionContext,
    message: string,
    /**
     * The placeholder to finalize once the transition proves this attempt still
     * owns the task. Omitted when there is none to name.
     */
    assistantMessageId?: string,
  ): Promise<void> {
    const { store, clock, logger } = this.deps;
    const taskId = ctx.task.taskId;
    // A cancelled turn is not a failed one. The signal is the same one the
    // provider call was watching, so "aborted" here means the user (or the
    // queue) stopped this run, and landing it `failed` reports a user action as
    // an error to every consumer downstream.
    const status: TaskStatus = ctx.signal.aborted ? "cancelled" : "failed";
    // Whether THIS attempt actually landed the task — one of two proofs that it
    // still owns it, and therefore that it may touch the placeholder.
    let landed = false;
    // The other proof, for the task that was landed OUT OF BAND: a host
    // transition, an operator cancel, a `waiting_approval` host that settled it
    // while this turn was breaking. `endAttempt` below is fenced, so its
    // SUCCESS says the lease is still this attempt's — nobody else has taken
    // the task over, and nobody else will finalize the placeholder.
    let leaseCurrent = false;
    try {
      const task = await store.tasks.getTask(taskId);
      if (task?.status === "running") {
        await store.tasks.transitionTask(
          taskId,
          ["running"],
          status,
          { finishedAt: clock.nowIso(), error: message },
          { leaseToken: ctx.leaseToken },
        );
        landed = true;
      }
    } catch (err) {
      logger?.warn("could not fail task after error", {
        taskId,
        error: err instanceof Error ? err.message : String(err),
      });
      if (err instanceof LeaseLostError) return;
    }
    try {
      await store.tasks.endAttempt({
        attemptId: ctx.attemptId,
        status,
        error: message,
        leaseToken: ctx.leaseToken,
      });
      leaseCurrent = true;
    } catch (err) {
      logger?.warn("could not end attempt after failure", {
        taskId,
        attemptId: ctx.attemptId,
        error: err instanceof Error ? err.message : String(err),
      });
    }
    // LAST, and only with ownership proved. A task somebody ELSE took over is a
    // task whose placeholder somebody else is responsible for — a
    // `LeaseLostError` from the transition has already returned, and one from
    // the fenced `endAttempt` leaves `leaseCurrent` false.
    //
    // `landed` alone was not enough. A task landed out of band — a host
    // transition, an operator cancel — is terminal before this block runs, so
    // the transition above is skipped and nothing else was ever going to take
    // the `placeholder: true` flag off: the run's answer stayed a spinner
    // forever, with the task long since cancelled. Nobody else can write it,
    // because the lease proves the task is still this attempt's.
    if (!(landed || leaseCurrent) || assistantMessageId === undefined) return;
    try {
      // Whatever the run streamed before it broke is KEPT — a half-written
      // answer plus a terminal event explaining the stop is more use to a
      // reader than a blank bubble — but `placeholder` has to come off, or the
      // UI spins forever on a message nothing is coming back to finish.
      await store.conversations.updateMessage(assistantMessageId, {
        metadata: { placeholder: false },
      });
    } catch (err) {
      logger?.warn("could not finalize placeholder after failure", {
        taskId,
        assistantMessageId,
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }
}

/**
 * The `chat.turn` entry in an {@link ExecutorRegistry}.
 *
 * Deliberately thin: everything about executing a turn lives on
 * {@link TurnRunner}, and this class exists only so the dispatcher has one
 * uniform shape to call — the same shape a host's own executors implement.
 */
export class ChatTurnExecutor implements TaskExecutor {
  readonly kind = CHAT_TURN_TASK_KIND;

  constructor(private readonly runner: TurnRunner) {}

  async execute(ctx: TaskExecutionContext): Promise<void> {
    await this.runner.executeTask(ctx);
  }
}

/** A required payload field that is actually there: a string, and non-empty. */
function isPresent(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function hasContent(state: PassState): boolean {
  return state.content.trim().length > 0;
}

/**
 * The request an assembled history is asking about: its LAST `role: "user"`
 * message, as text. Null when there is none, or when it carries no text.
 *
 * The last rather than the first, because a chat is a sequence of questions and
 * the one being answered is the most recent — every earlier user turn already
 * has its answer in the history. Text only, via `messageContentToText`: the one
 * caller is the correction harness, whose messages are built rather than
 * assembled, so an image part here would reach the provider as an unresolved
 * `ref`. See {@link CorrectionMessagesInput.userRequest}.
 */
function lastUserRequestOf(messages: readonly AiChatMessage[]): string | null {
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    const message = messages[index];
    if (message?.role !== "user") continue;
    const text = messageContentToText(message.content);
    return text.trim().length > 0 ? text : null;
  }
  return null;
}

/**
 * A prompt-size estimate for {@link UsageAuthorizer.authorize}: the assembled
 * conversation's characters, divided by four.
 *
 * Four characters per token is the rule of thumb, not a measurement — a real
 * count needs the provider's tokenizer, which this layer deliberately does not
 * carry (it would mean shipping a tokenizer per provider, kept in step with each
 * one's releases, to compute a number the port documents as best-effort). Image
 * and other non-text parts contribute nothing, via `messageContentToText`: their
 * token cost is provider-specific and guessing it would be a worse number than
 * admitting it is missing. `estimatedPromptTokens` is optional on the port
 * precisely so a host with a better estimate can ignore this one.
 */
function estimatePromptTokens(messages: readonly AiChatMessage[]): number {
  let chars = 0;
  for (const message of messages) {
    chars += messageContentToText(message.content).length;
  }
  return Math.ceil(chars / 4);
}

function describeDeficiencies(deficiencies: string[]): string {
  if (deficiencies.length === 0) {
    return "Verification did not pass, but reported no specific deficiency.";
  }
  return [
    `Verification found ${deficiencies.length} unresolved item(s):`,
    ...deficiencies.map((line) => `- ${line}`),
  ].join("\n");
}
