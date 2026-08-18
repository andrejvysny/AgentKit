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
import { AgentKitHostError, RecordNotFoundError } from "../errors.js";
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
import type { RunRecord, RunStatus } from "../ports/run-store.js";
import type { VerificationHook } from "../ports/verification.js";
import type { WritePolicy } from "../ports/write-policy.js";
import {
  EMULATED_TOOL_CALL_MESSAGE,
  looksLikeEmulatedToolCall,
} from "./emulated-tool-call.js";
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

export interface TurnRunnerDeps {
  store: AssistantStore;
  taskRunner: TaskRunner;
  /** Builds a client for a resolved provider config (key already injected). */
  providerFactory(config: AiProviderConfig): AiProviderClient;
  secrets?: SecretStore;
  contributors: ToolSetContributor[];
  context?: ContextProvider;
  verification?: VerificationHook;
  policy?: WritePolicy;
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
}

export interface SubmitMessageResult {
  chatId: string;
  runId: string;
  userMessageId: string;
  assistantMessageId: string;
}

/** What a run's `request` carries for this worker. */
interface TurnRequest {
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
  run: RunRecord;
  execution: TaskExecution;
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
 * Turns a submitted message into a durable run, and executes that run.
 *
 * Two halves, deliberately far apart in time:
 *
 * `submitMessage` writes the user message, an empty assistant placeholder, and a
 * queued run — in ONE transaction — and returns immediately. The caller gets ids
 * it can render against before a single token exists, and a crash one
 * millisecond later loses nothing, because the work is already recorded.
 *
 * `execute` is the {@link TaskWorker} the queue calls back. It drives
 * `runChat`, and every event it yields goes two places: appended to the run's
 * durable log (the replayable truth) and projected onto conversation state (what
 * the next turn replays to the provider). Retries stay inside ONE run id, each
 * pass continuing the same `seq` sequence via `RunStore.nextSeq`, so a consumer
 * that reconnects mid-retry still sees one unbroken, gap-detectable stream.
 */
export class TurnRunner implements TaskWorker {
  constructor(private readonly deps: TurnRunnerDeps) {}

  /**
   * Record a turn and hand it to the queue. Never waits on the model: the run is
   * durable the moment this returns, and the answer arrives through the event
   * log.
   */
  async submitMessage(
    input: SubmitMessageInput,
  ): Promise<SubmitMessageResult> {
    const runId = this.deps.ids.runId();
    const { userMessageId, assistantMessageId } =
      await this.deps.store.transaction(async (tx) => {
        const user = await tx.conversations.appendMessage({
          chatId: input.chatId,
          role: "user",
          content: input.content,
          metadata: input.metadata ?? {},
        });
        // The placeholder exists so the UI has a message to stream into, and so
        // the run has one durable target to write the answer to no matter how
        // many provider passes it takes.
        const placeholder = await tx.conversations.appendMessage({
          chatId: input.chatId,
          runId,
          role: "assistant",
          content: "",
          metadata: { placeholder: true },
        });
        const request: TurnRequest = {
          ...(input.model === undefined ? {} : { model: input.model }),
          ...(input.providerId === undefined
            ? {}
            : { providerId: input.providerId }),
          assistantMessageId: placeholder.id,
          userMessageId: user.id,
        };
        await tx.runs.createRun({
          runId,
          chatId: input.chatId,
          // Scope on the chat: two turns in one conversation must not run at
          // once, or they would interleave into the same message history.
          scopeId: input.chatId,
          request: request as unknown as Record<string, unknown>,
          ...(input.priority === undefined ? {} : { priority: input.priority }),
        });
        return { userMessageId: user.id, assistantMessageId: placeholder.id };
      });

    await this.deps.taskRunner.enqueue({ runId, scopeId: input.chatId });
    return {
      chatId: input.chatId,
      runId,
      userMessageId,
      assistantMessageId,
    };
  }

  /** Ask the queue to stop a run; the worker's signal is what actually aborts. */
  async cancel(runId: string): Promise<void> {
    await this.deps.taskRunner.requestCancel(runId);
  }

  async execute(execution: TaskExecution): Promise<void> {
    const { runId, attemptId } = execution;
    const runs = this.deps.store.runs;
    const run = await runs.getRun(runId);
    if (!run) throw new RecordNotFoundError(`Run not found: ${runId}`, { runId });
    if (run.status !== "queued" && run.status !== "running") {
      throw new AgentKitHostError(
        "run_not_executable",
        `Run ${runId} is ${run.status}; only queued or running runs execute.`,
        { runId, status: run.status },
      );
    }
    if (run.status === "queued") {
      await runs.transitionRun(runId, ["queued"], "running", {
        startedAt: this.deps.clock.nowIso(),
      });
    }

    const request = run.request as unknown as TurnRequest;
    const assistantMessageId = request.assistantMessageId;
    try {
      await this.runTurn(run, request, assistantMessageId, execution);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      await this.failQuietly(runId, attemptId, message);
      throw err;
    }
  }

  private async runTurn(
    run: RunRecord,
    request: TurnRequest,
    assistantMessageId: string,
    execution: TaskExecution,
  ): Promise<void> {
    const { store, clock } = this.deps;
    const chatId = run.chatId;
    const settings = await store.settings.getSettings();
    const provider = await this.resolveProvider(request, settings);
    const model = request.model ?? provider.defaultModel ?? settings.defaultModel;
    if (!model) {
      throw new AgentKitHostError(
        "no_model",
        `No model resolved for run ${run.runId}.`,
        { runId: run.runId, providerId: provider.id },
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

    await this.deps.context?.refresh?.(chatId, execution.signal);
    const bindings =
      (await this.deps.context?.listBindings(chatId, execution.signal)) ?? [];
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
            runId: run.runId,
            scopeId: run.scopeId,
            bindings,
            limits,
            signal: execution.signal,
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
      (await this.deps.context?.systemPrompt?.(chatId, execution.signal)) ??
      null;
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
      run,
      execution,
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

    // Still nothing, after every recovery pass. A cancelled run is exempt: it
    // has no answer because it was stopped, which is not the same failure.
    if (
      terminal !== "cancelled" &&
      !hasContent(state) &&
      toolCallCount === 0
    ) {
      await this.emitWarning(
        execution,
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
        execution,
        "emulated_tool_call",
        EMULATED_TOOL_CALL_MESSAGE,
      );
      await store.conversations.appendMessage({
        chatId,
        runId: run.runId,
        role: "system",
        content: EMULATED_TOOL_CALL_MESSAGE,
        metadata: { banner: "emulated_tool_call" },
      });
    }

    // Verification runs once, and only when the run actually did tool work —
    // there is nothing to verify about a chat answer. A single pass is
    // deliberate: feeding the deficiencies back for the model to correct is a
    // multi-pass harness, and that belongs in a later phase where its cost and
    // its stopping condition can be designed properly rather than bolted on.
    if (this.deps.verification && toolCallCount > 0) {
      const report = await this.deps.verification.verify({
        runId: run.runId,
        chatId,
        scopeId: run.scopeId,
        attemptId: execution.attemptId,
        toolCallCount,
        finalContent,
        signal: execution.signal,
      });
      if (report && report.status !== "pass") {
        await store.conversations.appendMessage({
          chatId,
          runId: run.runId,
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

    const finalStatus: RunStatus =
      terminal === "completed"
        ? "completed"
        : terminal === "cancelled"
          ? "cancelled"
          : "failed";
    await store.runs.transitionRun(run.runId, ["running"], finalStatus, {
      finishedAt: clock.nowIso(),
    });
    await store.runs.endAttempt({
      attemptId: execution.attemptId,
      status: finalStatus,
    });
  }

  /**
   * One `runChat` invocation, with its events appended and projected.
   *
   * `firstSeq` comes from the store rather than from a counter in this class:
   * several passes share a run id, and only the log knows where the last one
   * stopped. That is what keeps `seq` unbroken across a retry — and what lets a
   * resumed process continue a stream it did not start.
   */
  private async runPass(input: PassInput): Promise<PassResult> {
    const firstSeq = await this.deps.store.runs.nextSeq(input.run.runId);
    const generator = runChat({
      client: input.client,
      registry: input.registry,
      model: input.model,
      messages: input.messages,
      bindings: input.bindings,
      limits: input.limits,
      chatId: input.run.chatId,
      runId: input.run.runId,
      attemptId: input.execution.attemptId,
      firstSeq,
      signal: input.execution.signal,
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
    const { state, run } = input;
    await store.runs.appendEvents(run.runId, [event], {
      leaseToken: input.execution.leaseToken,
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
            chatId: run.chatId,
            runId: run.runId,
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
          chatId: run.chatId,
          runId: run.runId,
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
          chatId: run.chatId,
          runId: run.runId,
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

  /** Stamp and append a host-originated warning, continuing the run's sequence. */
  private async emitWarning(
    execution: TaskExecution,
    code: string,
    message: string,
  ): Promise<void> {
    const firstSeq = await this.deps.store.runs.nextSeq(execution.runId);
    const stamp = createEventStamper({
      firstSeq,
      attemptId: execution.attemptId,
    });
    const event = stamp({
      type: "run.warning",
      runId: execution.runId,
      timestamp: this.deps.clock.nowIso(),
      data: { code, message },
    });
    await this.deps.store.runs.appendEvents(execution.runId, [event], {
      leaseToken: execution.leaseToken,
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
   * The placeholder is skipped — it is where this run's answer is being written,
   * and feeding a model its own empty (or half-written) reply is how a turn ends
   * up completing someone else's sentence. `role: "system"` records are skipped
   * too: those are UI banners the host wrote about the run, not prompt material.
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
    return messages;
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
    runId: string,
    attemptId: string,
    message: string,
  ): Promise<void> {
    const { store, clock, logger } = this.deps;
    try {
      await store.runs.endAttempt({
        attemptId,
        status: "failed",
        error: message,
      });
    } catch (err) {
      logger?.warn("could not end attempt after failure", {
        runId,
        attemptId,
        error: err instanceof Error ? err.message : String(err),
      });
    }
    try {
      const run = await store.runs.getRun(runId);
      if (run?.status === "running") {
        await store.runs.transitionRun(runId, ["running"], "failed", {
          finishedAt: clock.nowIso(),
          error: message,
        });
      }
    } catch (err) {
      logger?.warn("could not fail run after error", {
        runId,
        error: err instanceof Error ? err.message : String(err),
      });
    }
  }
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
