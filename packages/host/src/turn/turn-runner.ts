import type {
  AiChatMessage,
  AiContentPart,
  AiContextBinding,
  AiMessageContent,
  AiProviderConfig,
  AiRunEvent,
  AiToolCall,
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
import type { UsageAuthorizer } from "../ports/usage-authorizer.js";
import type { AssistantSettings } from "../ports/settings-store.js";
import type { TaskRecord, TaskStatus } from "../ports/task-store.js";
import type { VerificationHook } from "../ports/verification.js";
import { CHAT_TURN_TASK_KIND } from "../tasks/kinds.js";
import { loadExecutableTask } from "../tasks/load-executable-task.js";
import type {
  TaskExecutionContext,
  TaskExecutor,
} from "../tasks/task-executor.js";
import {
  EMULATED_TOOL_CALL_MESSAGE,
  looksLikeEmulatedToolCall,
} from "./emulated-tool-call.js";
import { reconcileOrphanToolCalls } from "./history-reconcile.js";
import { orderMessagesForProvider } from "./message-order.js";
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
  context?: ContextProvider;
  verification?: VerificationHook;
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
}

export interface SubmitMessageResult {
  chatId: string;
  runId: string;
  userMessageId: string;
  assistantMessageId: string;
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

/** Mutable state accumulated across one provider pass. */
interface PassState {
  /** Visible answer so far. */
  content: string;
  /** Whether any delta arrived (a completed-only provider sends none). */
  streamed: boolean;
  /** Distinct tool call ids seen this run — the "did it use tools?" signal. */
  toolCallIds: Set<string>;
  /** Internal assistant record awaiting its tool calls (see `projectEvent`). */
  pendingAssistantMessageId?: string;
  pendingToolCalls: AiToolCall[];
  /**
   * The last message THIS RUN wrote — the link every further append chains off.
   *
   * Seeded with the placeholder, so the run's records descend from the answer
   * they belong to, and carried across passes because a retry continues the
   * same conversation branch rather than starting a second one.
   *
   * It exists because "the chat's active leaf" is not a stable answer for the
   * duration of a turn: a user may switch branches between two of these
   * writes, and an append that took the leaf would put the second half of this
   * run's records on a conversation that never ran them — while leaving this
   * run's own branch with tool calls nobody answered. Naming the link removes
   * the race rather than narrowing it. See `AppendMessageInput.activate`.
   */
  lastMessageId: string;
}

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
  constructor(private readonly deps: TurnRunnerDeps) {}

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
          kind: CHAT_TURN_TASK_KIND,
          // Scope on the chat: two turns in one conversation must not run at
          // once, or they would interleave into the same message history.
          scopeId: input.chatId,
          payload: payload as unknown as Record<string, unknown>,
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
      const existing = await this.resubmitted(err, input, taskId);
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
   * - a task under this key that is not a `chat.turn`, or is a turn in another
   *   chat, is an id collision between two unrelated callers — same reasoning,
   *   louder failure mode;
   * - a payload missing either message id cannot answer the caller at all, and
   *   a `chat.turn` row without them did not come from this method.
   */
  private async resubmitted(
    err: unknown,
    input: SubmitMessageInput,
    taskId: string,
  ): Promise<SubmitMessageResult | null> {
    if (!(err instanceof DuplicateTaskError) || input.taskId === undefined) {
      return null;
    }
    const existing = await this.deps.store.tasks.getTask(taskId);
    if (!existing || existing.kind !== CHAT_TURN_TASK_KIND) return null;
    const payload = existing.payload as unknown as Partial<TurnRequest>;
    if (
      payload.chatId !== input.chatId ||
      typeof payload.userMessageId !== "string" ||
      typeof payload.assistantMessageId !== "string"
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
   * `failed` on the way out so it is never left `running` with nobody executing
   * it.
   */
  async executeTask(ctx: TaskExecutionContext): Promise<void> {
    const { task, attemptId } = ctx;
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
      await this.failQuietly(task.taskId, attemptId, error.message);
      throw error;
    }

    const request = payload as TurnRequest;
    try {
      await this.runTurn(ctx, chatId, request, assistantMessageId);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      await this.failQuietly(task.taskId, attemptId, message);
      throw err;
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

    await this.deps.context?.refresh?.(chatId, ctx.signal);
    const bindings =
      (await this.deps.context?.listBindings(chatId, ctx.signal)) ?? [];
    const hasPrimaryBinding = bindings.some(
      (binding) => binding.role === "primary" && binding.status === "active",
    );

    // A provider probed as chat-only must not be handed tools at all: doing so
    // is the exact request shape that fails, and the chat-only retry exists to
    // recover from discovering that the hard way.
    const providerAllowsTools = capabilities?.toolCalling !== false;
    const registry = providerAllowsTools
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
        })
      : new AiToolRegistry();
    const registryHadTools = registry.size() > 0;

    const client = this.deps.providerFactory(await this.withSecret(provider));
    const systemPrompt =
      (await this.deps.context?.systemPrompt?.(chatId, ctx.signal)) ?? null;
    const assembled = await this.assembleMessages(
      chatId,
      assistantMessageId,
      systemPrompt,
    );
    // Snapshot before the first pass: the empty-response retry re-asks the
    // ORIGINAL question, not the question plus whatever the failed turn left
    // behind.
    const initialMessages = assembled.slice();

    const state: PassState = {
      content: "",
      streamed: false,
      toolCallIds: new Set<string>(),
      pendingToolCalls: [],
      lastMessageId: assistantMessageId,
    };
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

    if (shouldRetryChatOnly({ terminal, registryHadTools })) {
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
      looksLikeEmulatedToolCall(finalContent)
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

    // Verification runs once, and only when the turn actually did tool work —
    // there is nothing to verify about a chat answer. A single pass is
    // deliberate: feeding the deficiencies back for the model to correct is a
    // multi-pass harness, and that belongs in a later phase where its cost and
    // its stopping condition can be designed properly rather than bolted on.
    if (this.deps.verification && toolCallCount > 0) {
      const report = await this.deps.verification.verify({
        runId: task.taskId,
        chatId,
        scopeId: task.scopeId,
        attemptId: ctx.attemptId,
        toolCallCount,
        finalContent,
        signal: ctx.signal,
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
    }

    await store.conversations.updateMessage(assistantMessageId, {
      content: finalContent,
      metadata: { placeholder: false },
    });

    const finalStatus: TaskStatus =
      terminal === "completed"
        ? "completed"
        : terminal === "cancelled"
          ? "cancelled"
          : "failed";
    await store.tasks.transitionTask(task.taskId, ["running"], finalStatus, {
      finishedAt: clock.nowIso(),
    });
    await store.tasks.endAttempt({
      attemptId: ctx.attemptId,
      status: finalStatus,
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
   * The log is written FIRST and unconditionally: it is the record of what
   * happened, and a projection failure must not be able to erase it.
   *
   * Every message this writes is a CHAIN append — `parentMessageId` is the id
   * this run wrote last, `activate: false` — so the records land on the run's
   * own branch whatever the user has been doing to the active path meanwhile.
   * With no branch switch that is byte-identically what an unparented append
   * produced: the run's last write IS the active leaf, so the chain and the
   * path are the same messages. See {@link PassState.lastMessageId}.
   */
  private async projectEvent(
    event: AiRunEvent,
    input: PassInput,
  ): Promise<void> {
    const { store } = this.deps;
    const { state, task, chatId } = input;
    await store.tasks.appendEvents(task.taskId, [event], {
      leaseToken: input.ctx.leaseToken,
    });

    switch (event.type) {
      case "run.message.delta": {
        state.content += event.data.delta;
        state.streamed = true;
        await store.conversations.updateMessage(input.assistantMessageId, {
          content: state.content,
        });
        break;
      }
      case "run.message.completed": {
        // A new assistant turn supersedes any turn still waiting for its calls:
        // late `run.tool.requested` events belong to THIS turn, not the last one.
        delete state.pendingAssistantMessageId;
        state.pendingToolCalls = [];
        if (event.data.toolCallCount > 0) {
          const toolCalls = event.data.toolCalls ?? [];
          const record = await store.conversations.appendMessage({
            chatId,
            runId: task.taskId,
            role: "assistant",
            content: event.data.content,
            toolCalls,
            parentMessageId: state.lastMessageId,
            activate: false,
            metadata: { internal: true },
          });
          state.lastMessageId = record.id;
          for (const call of toolCalls) state.toolCallIds.add(call.id);
          if (toolCalls.length === 0) {
            // A streaming provider reports the COUNT here and the calls
            // themselves in the `run.tool.requested` events that follow. Hold
            // the record open and fill them in as they arrive: an assistant
            // turn persisted without its tool_calls leaves every tool result
            // after it an orphan on the next replay.
            state.pendingAssistantMessageId = record.id;
            state.pendingToolCalls = [];
          }
        } else if (!state.streamed && event.data.content.length > 0) {
          // Non-streaming provider: the visible answer exists only here.
          state.content = event.data.content;
          await store.conversations.updateMessage(input.assistantMessageId, {
            content: state.content,
          });
        }
        break;
      }
      case "run.usage": {
        // Reported to the spend port AFTER the event is durable, and with the
        // provider's own numbers rather than the estimate the pass was
        // authorized on — the gap between the two is the whole reason
        // `UsageAuthorizer` has a second method. Every usage event is reported,
        // including the non-final ones a streaming provider emits mid-call: a
        // recorder that only saw `finalForCall` would lose the accounting for a
        // call that died before it settled, which is exactly the call a budget
        // most needs to know about. `finalForCall`/`source`/`step` ride along
        // so the recorder can tell those two kinds apart — reporting the
        // interim numbers without them is just double counting.
        await this.deps.usage?.record({
          runId: task.taskId,
          callId: event.data.callId,
          attempt: event.data.attempt,
          providerId: input.providerId,
          model: event.data.model,
          finalForCall: event.data.finalForCall,
          source: event.data.source,
          step: event.data.step,
          ...(event.data.promptTokens === undefined
            ? {}
            : { promptTokens: event.data.promptTokens }),
          ...(event.data.completionTokens === undefined
            ? {}
            : { completionTokens: event.data.completionTokens }),
          ...(event.data.totalTokens === undefined
            ? {}
            : { totalTokens: event.data.totalTokens }),
          at: event.timestamp,
        });
        break;
      }
      case "run.tool.requested": {
        state.toolCallIds.add(event.data.toolCallId);
        if (state.pendingAssistantMessageId !== undefined) {
          state.pendingToolCalls.push({
            id: event.data.toolCallId,
            name: event.data.toolName,
            argumentsJson: event.data.argumentsJson,
          });
          await store.conversations.updateMessage(
            state.pendingAssistantMessageId,
            { toolCalls: [...state.pendingToolCalls] },
          );
        }
        break;
      }
      case "run.tool.succeeded": {
        // The SLIM envelope is what gets persisted as the tool message, because
        // this record is replayed into the model's context on every later turn.
        // The full payload stays on the event, where the UI can read it once.
        const slim = event.data.modelResultJson ?? event.data.resultJson;
        state.lastMessageId = (
          await store.conversations.appendMessage({
            chatId,
            runId: task.taskId,
            role: "tool",
            content: slim,
            toolCallId: event.data.toolCallId,
            modelResultJson: slim,
            parentMessageId: state.lastMessageId,
            activate: false,
            metadata: { internal: true, toolName: event.data.toolName },
          })
        ).id;
        break;
      }
      case "run.tool.failed": {
        const slim =
          event.data.modelResultJson ??
          JSON.stringify({
            ok: false,
            status: "error",
            summary: event.data.errorMessage,
            warnings: [],
            truncated: false,
            data: {
              errorCode: event.data.errorCode,
              errorMessage: event.data.errorMessage,
            },
          });
        state.lastMessageId = (
          await store.conversations.appendMessage({
            chatId,
            runId: task.taskId,
            role: "tool",
            content: slim,
            toolCallId: event.data.toolCallId,
            modelResultJson: slim,
            parentMessageId: state.lastMessageId,
            activate: false,
            metadata: { internal: true, toolName: event.data.toolName },
          })
        ).id;
        break;
      }
      default:
        break;
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
        if (!cache.has(ref)) cache.set(ref, await resolver.resolve(ref));
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

  /** Clear the answer-so-far before a recovery pass re-answers from scratch. */
  private resetPass(state: PassState): void {
    state.content = "";
    state.streamed = false;
    delete state.pendingAssistantMessageId;
    state.pendingToolCalls = [];
  }

  /**
   * Assemble the provider-facing conversation from stored records.
   *
   * The placeholder is skipped — it is where this turn's answer is being
   * written, and feeding a model its own empty (or half-written) reply is how a
   * turn ends up completing someone else's sentence. `role: "system"` records
   * are skipped too: those are UI banners the host wrote about the turn, not
   * prompt material.
   *
   * The records come from `listMessages`, which reports the chat's ACTIVE PATH —
   * so a branch submit replays the branch and not the answer it replaced, with
   * no filtering here. `orderMessagesForProvider` runs over them unchanged:
   * `orderKey` increases with depth along any path, so ordering the active path
   * by `orderKey` and ordering it by depth are the same order, and the
   * run-scoped tool-call repair it performs is untouched by branching.
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
   */
  private async failQuietly(
    taskId: string,
    attemptId: string,
    message: string,
  ): Promise<void> {
    const { store, clock, logger } = this.deps;
    try {
      await store.tasks.endAttempt({
        attemptId,
        status: "failed",
        error: message,
      });
    } catch (err) {
      logger?.warn("could not end attempt after failure", {
        taskId,
        attemptId,
        error: err instanceof Error ? err.message : String(err),
      });
    }
    try {
      const task = await store.tasks.getTask(taskId);
      if (task?.status === "running") {
        await store.tasks.transitionTask(taskId, ["running"], "failed", {
          finishedAt: clock.nowIso(),
          error: message,
        });
      }
    } catch (err) {
      logger?.warn("could not fail task after error", {
        taskId,
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
