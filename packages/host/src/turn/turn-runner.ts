import type {
  AiChatMessage,
  AiContextBinding,
  AiProviderConfig,
  AiRunEvent,
  AiToolCall,
  AiToolLimits,
} from "@agentkit/contracts";
import {
  AiToolRegistry,
  createEventStamper,
  resolveToolLimits,
  runChat,
  type AiProviderClient,
} from "@agentkit/core";
import {
  AgentKitHostError,
  DuplicateTaskError,
  RecordNotFoundError,
} from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { ContextProvider } from "../ports/context-provider.js";
import type { SecretStore } from "../ports/secret-store.js";
import type { Clock, IdGenerator, Logger } from "../ports/system.js";
import type {
  TaskExecution,
  TaskRunner,
  TaskWorker,
} from "../ports/task-runner.js";
import type { ToolSetContributor } from "../ports/tool-contributor.js";
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
  content: string;
  model?: string;
  providerId?: string;
  metadata?: Record<string, unknown>;
  priority?: number;
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
}

interface PassInput {
  task: TaskRecord;
  /** The conversation this turn belongs to, read once from the payload. */
  chatId: string;
  ctx: TaskExecutionContext;
  client: AiProviderClient;
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
  async submitMessage(
    input: SubmitMessageInput,
  ): Promise<SubmitMessageResult> {
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
    const model = request.model ?? provider.defaultModel ?? settings.defaultModel;
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
    };
    const basePass = {
      task,
      chatId,
      ctx,
      client,
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
    if (
      terminal !== "cancelled" &&
      !hasContent(state) &&
      toolCallCount === 0
    ) {
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
      await store.conversations.appendMessage({
        chatId,
        runId: task.taskId,
        role: "system",
        content: EMULATED_TOOL_CALL_MESSAGE,
        metadata: { banner: "emulated_tool_call" },
      });
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
        await store.conversations.appendMessage({
          chatId,
          runId: task.taskId,
          role: "system",
          content: describeDeficiencies(report.deficiencies),
          metadata: { banner: "verification", status: report.status },
        });
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
   */
  private async runPass(input: PassInput): Promise<PassResult> {
    const firstSeq = await this.deps.store.tasks.nextSeq(input.task.taskId);
    const generator = runChat({
      client: input.client,
      registry: input.registry,
      model: input.model,
      messages: input.messages,
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
   * Append an event to the durable log, then reflect it in conversation state.
   *
   * The log is written FIRST and unconditionally: it is the record of what
   * happened, and a projection failure must not be able to erase it.
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
            metadata: { internal: true },
          });
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
        await store.conversations.appendMessage({
          chatId,
          runId: task.taskId,
          role: "tool",
          content: slim,
          toolCallId: event.data.toolCallId,
          modelResultJson: slim,
          metadata: { internal: true, toolName: event.data.toolName },
        });
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
        await store.conversations.appendMessage({
          chatId,
          runId: task.taskId,
          role: "tool",
          content: slim,
          toolCallId: event.data.toolCallId,
          modelResultJson: slim,
          metadata: { internal: true, toolName: event.data.toolName },
        });
        break;
      }
      default:
        break;
    }
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
    const taskId = ctx.task.taskId;
    const firstSeq = await this.deps.store.tasks.nextSeq(taskId);
    const stamp = createEventStamper({
      firstSeq,
      attemptId: ctx.attemptId,
    });
    const event = stamp({
      type: "run.warning",
      runId: taskId,
      timestamp: this.deps.clock.nowIso(),
      data: { code, message },
    });
    await this.deps.store.tasks.appendEvents(taskId, [event], {
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

function describeDeficiencies(deficiencies: string[]): string {
  if (deficiencies.length === 0) {
    return "Verification did not pass, but reported no specific deficiency.";
  }
  return [
    `Verification found ${deficiencies.length} unresolved item(s):`,
    ...deficiencies.map((line) => `- ${line}`),
  ].join("\n");
}
