import type {
  AiChatMessage,
  AiContextBinding,
  AiProviderConfig,
  AiRunEvent,
  AiToolLimits,
} from "@agentkit/contracts";
import {
  AiToolRegistry,
  createEventStamper,
  resolveToolLimits,
  runChat,
  type AiProviderClient,
  type AiRunEventDraft,
} from "@agentkit/core";
import {
  AgentKitHostError,
  LeaseLostError,
  RecordNotFoundError,
  UsageDeniedError,
} from "../errors.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { AttachmentResolver } from "../ports/attachment-resolver.js";
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
import type { VerificationHook } from "../ports/verification.js";
import { CHAT_TURN_TASK_KIND } from "../tasks/kinds.js";
import { loadExecutableTask } from "../tasks/load-executable-task.js";
import type {
  TaskExecutionContext,
  TaskExecutor,
} from "../tasks/task-executor.js";
import {
  resolveMaxCorrectionPasses,
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
import {
  assembleMessages,
  estimatePromptTokens,
  lastUserRequestOf,
  resolveAttachments,
  type AttachmentBudgets,
} from "./history-assembly.js";
import {
  describeDeficiencies,
  runCorrectionHarness,
} from "./harness-driver.js";
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
import {
  regenerate as regenerateTurn,
  submitMessage as submitMessageTurn,
  type RegenerateMessageInput,
  type SubmitMessageInput,
  type SubmitMessageResult,
  type TurnRequest,
} from "./submit.js";

// Re-exported so `AttachmentBudgets`, `SubmitMessageInput`/`Result` and
// `RegenerateMessageInput` stay importable from this module exactly as before
// the split — they now live in `history-assembly.ts` and `submit.ts`.
export type { AttachmentBudgets } from "./history-assembly.js";
export type {
  RegenerateMessageInput,
  SubmitMessageInput,
  SubmitMessageResult,
} from "./submit.js";

/** Provider metadata key holding the {@link SecretStore} ref for the API key. */
export const PROVIDER_SECRET_REF_KEY = "apiKeySecretRef";

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

/**
 * Mutable state accumulated across one provider pass.
 *
 * It is the projection seam's own state — see {@link RunProjectionState} — not
 * a second model beside it: every field this class reads between passes
 * (`content` and `toolCallIds` for the retry decisions, `lastMessageId` for the
 * banners it chains) is one the projector maintains, and two structures
 * tracking the same run would be two answers to "what has this turn written".
 */
export type PassState = RunProjectionState;

export interface PassInput {
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

export interface PassResult {
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
   * Delegates to [`submit.ts`](./submit.ts)`#submitMessage`; kept as a thin
   * method because it is this class's public API.
   */
  async submitMessage(input: SubmitMessageInput): Promise<SubmitMessageResult> {
    return submitMessageTurn(this.deps, input);
  }

  /**
   * Re-answer a question that already has an answer, as a NEW BRANCH beside it.
   *
   * Delegates to [`submit.ts`](./submit.ts)`#regenerate`; kept as a thin method
   * because it is this class's public API.
   */
  async regenerate(
    input: RegenerateMessageInput,
  ): Promise<SubmitMessageResult> {
    return regenerateTurn(this.deps, input);
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
    const assembled = await assembleMessages(
      this.deps,
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
        terminal = await runCorrectionHarness(
          {
            deps: this.deps,
            hookTimeouts: this.hookTimeouts,
            runPass: this.runPass.bind(this),
            resetPass: this.resetPass.bind(this),
            appendHostEvent: this.appendHostEvent.bind(this),
            emitPassBoundary: this.emitPassBoundary.bind(this),
          },
          {
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
          },
        );
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
    const messages = await resolveAttachments(
      {
        deps: this.deps,
        hookTimeouts: this.hookTimeouts,
        emitWarning: this.emitWarning.bind(this),
      },
      input.ctx,
      input.chatId,
      input.messages,
    );
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
