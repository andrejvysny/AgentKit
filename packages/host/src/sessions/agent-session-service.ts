/**
 * `AgentSessionService` (ADR 0016 §6): a facade over the turn runner, the task
 * service and the durable input store. It adds no scheduler, queue runner or
 * store of its own: a session is a chat carrying a descriptor, a turn is a
 * `chat.turn` task, and everything that cannot start a turn right now is an
 * `AgentInputStore` record.
 */
import {
  AgentInputTransitionError,
  AgentKitHostError,
  ChatBusyError,
  RecordNotFoundError,
  SessionNotFoundError,
  SessionsUnavailableError,
} from "../errors.js";
import type { AgentInputRecord } from "../ports/agent-input-store.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { ChatRecord } from "../ports/conversation-store.js";
import type { Clock, IdGenerator, Logger } from "../ports/system.js";
import { assertScopeIdle, isTerminalTaskStatus } from "../ports/task-store.js";
import type { TaskRecord } from "../ports/task-store.js";
import type {
  TaskInterruptionReason,
  TaskRunner,
} from "../ports/task-runner.js";
import { isValidSessionTurnKind } from "../tasks/kinds.js";
import type { TaskService } from "../tasks/task-service.js";
import { stageNextQueuedTurn } from "../turn/session-turn.js";
import type { TurnRunner } from "../turn/turn-runner.js";
import {
  COMPACT_REQUESTED_METADATA_KEY,
  REASONING_EFFORTS,
  SESSION_METADATA_KEY,
  readSessionDescriptor,
  sessionContentSelection,
  sessionTurnKind,
  type AgentSessionDescriptor,
  type SessionContextOptions,
  type SessionReasoningEffort,
} from "./descriptor.js";
import { sha256Hex } from "./hash.js";
import {
  executionStateForMode,
  planRequestOf,
  readSessionExecution,
  withSessionExecution,
  type PlanApprovalPolicy,
  type SessionExecutionState,
} from "./plan-mode.js";
import { answerHumanRequest } from "./human-answer.js";
import {
  exportSession,
  listTurnTasks,
  sessionUsage,
  summarizeTurn,
} from "./session-report.js";
import type {
  AgentAnswerReceipt,
  AgentSendResult,
  AgentSessionExport,
  AgentSessionState,
  AgentSessionUsage,
  AgentTurnSummary,
  AnswerInput,
  CreateSessionInput,
  FollowUpInput,
  SendInput,
  SessionContent,
  SteerInput,
} from "./session-types.js";

export interface AgentSessionServiceDeps {
  /** Must carry `.inputs`; the constructor refuses a store without it. */
  store: AssistantStore;
  turns: TurnRunner;
  tasks: TaskService;
  taskRunner: TaskRunner;
  clock: Clock;
  ids: IdGenerator;
  logger?: Logger;
  /**
   * Plan-mode escalation policy (ADR 0019 part 2). When wired, an approval
   * answer for one of `toolNames` writes the session's execution grant in the
   * same transaction that re-admits the parked turn. Absent, plan approvals
   * are never honored and a session in plan mode stays read-only.
   */
  planApproval?: PlanApprovalPolicy;
}

type QueueKind = "follow_up" | "steer";

interface QueueRequest {
  inputId?: string | undefined;
  content: SessionContent;
  turnId?: string | undefined;
  onSettled?: "follow_up" | "cancel" | undefined;
}

/** Re-checks of the idle/busy race before giving up with `chat_busy`. */
const MAX_SUBMIT_ATTEMPTS = 3;

function invalid(message: string, details?: Record<string, unknown>): never {
  throw new AgentKitHostError("invalid_input", message, details);
}

function turnIdFor(inputId: string): string {
  return `turn:${inputId}`;
}

export class AgentSessionService {
  constructor(private readonly deps: AgentSessionServiceDeps) {
    if (!deps.store.inputs) {
      throw new SessionsUnavailableError(
        "AgentSessionService needs a store with `inputs` (AgentInputStore).",
      );
    }
  }

  async createSession(input: CreateSessionInput): Promise<AgentSessionState> {
    if (input.metadata && SESSION_METADATA_KEY in input.metadata)
      invalid(`metadata must not contain ${SESSION_METADATA_KEY}.`);
    if (input.turnKind !== undefined && !isValidSessionTurnKind(input.turnKind))
      invalid(
        "turnKind must be a dotted lowercase task kind outside the reserved chat.* / agentkit.* prefixes.",
      );
    const raw = {
      version: 1,
      profile: input.profile,
      ...(input.providerId === undefined
        ? {}
        : { providerId: input.providerId }),
      ...(input.model === undefined ? {} : { model: input.model }),
      createdAt: this.deps.clock.nowIso(),
      ...(input.workspace === undefined ? {} : { workspace: input.workspace }),
      ...(input.context === undefined ? {} : { context: input.context }),
      ...(input.skills === undefined ? {} : { skills: input.skills }),
      ...(input.turnKind === undefined ? {} : { turnKind: input.turnKind }),
    };
    const metadata = { ...input.metadata, [SESSION_METADATA_KEY]: raw };
    if (readSessionDescriptor(metadata) === undefined)
      invalid("profile.id must be a non-empty string.");
    if (input.executionMode !== undefined && SESSION_METADATA_KEY in metadata)
      // `withSessionExecution` keeps the descriptor; validate the mode first so
      // a bad one cannot half-create the session.
      executionStateForMode(input.executionMode, this.deps.clock.nowIso());
    const chat = await this.deps.store.conversations.createChat({
      ...(input.sessionId === undefined ? {} : { id: input.sessionId }),
      ...(input.title === undefined ? {} : { title: input.title }),
      metadata:
        input.executionMode === undefined
          ? metadata
          : withSessionExecution(
              metadata,
              executionStateForMode(
                input.executionMode,
                this.deps.clock.nowIso(),
              ),
            ),
    });
    return this.getState(chat.id);
  }

  /** The session's durable permission state, when one was ever set. */
  async getExecution(
    sessionId: string,
  ): Promise<SessionExecutionState | undefined> {
    const { chat } = await this.requireSession(sessionId);
    return readSessionExecution(chat.metadata);
  }

  /**
   * An explicit, persisted mode switch by the HOST or the user — never by the
   * model. Entering `plan` drops any grant, so a session that was cleared to
   * edit cannot read its way back into write access without a new approval.
   */
  async setExecutionMode(
    sessionId: string,
    mode: string,
  ): Promise<SessionExecutionState> {
    const now = this.deps.clock.nowIso();
    return this.deps.store.transaction(async (tx) => {
      const chat = await tx.conversations.getChat(sessionId);
      if (chat === null)
        throw new SessionNotFoundError(`Session not found: ${sessionId}`, {
          sessionId,
        });
      if (readSessionDescriptor(chat.metadata) === undefined)
        throw new SessionNotFoundError(
          `Session ${sessionId} is not an agent session.`,
          { sessionId },
        );
      const state = executionStateForMode(
        mode,
        now,
        readSessionExecution(chat.metadata),
      );
      await tx.conversations.updateChat(sessionId, {
        metadata: withSessionExecution(chat.metadata, state),
      });
      return state;
    });
  }

  async getState(sessionId: string): Promise<AgentSessionState> {
    const { descriptor, chat } = await this.requireSession(sessionId);
    const active = await this.activeTurn(
      sessionId,
      sessionTurnKind(descriptor),
    );
    const records = await this.inputs().listInputs({
      sessionId,
      statuses: ["pending"],
    });
    const waiting = records.find((record) => record.kind === "human_request");
    const execution = readSessionExecution(chat.metadata);
    const waitingPlan =
      waiting !== undefined
        ? planRequestOf(waiting, this.deps.planApproval)
        : undefined;
    return {
      sessionId,
      descriptor,
      ...(active
        ? { activeTurn: { turnId: active.taskId, status: active.status } }
        : {}),
      pending: records.filter((record) => record.kind !== "human_request"),
      ...(waiting ? { waiting } : {}),
      ...(waiting && approvalOf(waiting)
        ? { waitingApproval: approvalOf(waiting) }
        : {}),
      ...(waiting && waitingPlan
        ? {
            waitingPlan: {
              requestId: waiting.inputId,
              toolName:
                (waiting.content as { toolName?: string }).toolName ?? "",
              ...waitingPlan,
            },
          }
        : {}),
      ...(execution ? { execution } : {}),
    };
  }

  async send(sessionId: string, input: SendInput): Promise<AgentSendResult> {
    const whenBusy = input.whenBusy ?? "reject";
    if (whenBusy !== "reject" && input.inputId === undefined)
      invalid(`inputId is required when whenBusy is "${whenBusy}".`);
    const session = await this.requireSession(sessionId);
    if (whenBusy === "reject")
      return this.submitTurn(session, input.content, input.inputId);
    return this.submitOrQueue(session, whenBusy, input);
  }

  async followUp(
    sessionId: string,
    input: FollowUpInput,
  ): Promise<AgentSendResult> {
    return this.submitOrQueue(
      await this.requireSession(sessionId),
      "follow_up",
      input,
    );
  }

  /** On an idle session a steer is just a new turn. */
  async steer(sessionId: string, input: SteerInput): Promise<AgentSendResult> {
    return this.submitOrQueue(
      await this.requireSession(sessionId),
      "steer",
      input,
    );
  }

  /**
   * Convert the oldest queued input into a turn. Refused with `chat_busy` while
   * the session has unfinished work; undefined when nothing is queued.
   */
  async flushQueue(sessionId: string): Promise<AgentSendResult | undefined> {
    const { descriptor } = await this.requireSession(sessionId);
    const staged = await this.deps.store.transaction(async (tx) => {
      const inputs = tx.inputs;
      if (!inputs) return undefined;
      const pending = await inputs.listInputs({
        sessionId,
        kinds: ["follow_up", "steer"],
        statuses: ["pending"],
      });
      if (pending.length === 0) return undefined;
      // Keeps a refused flush from cancelling steers meant for the live turn;
      // the exclusive-scope create below stays the authoritative guard.
      assertScopeIdle(
        sessionId,
        (await tx.tasks.listByScope(sessionId)).filter(
          isLiveTurn(sessionTurnKind(descriptor)),
        ),
      );
      const queue: AgentInputRecord[] = [];
      for (const record of pending) {
        if (record.kind === "steer" && record.targetTaskId !== undefined) {
          const target = await tx.tasks.getTask(record.targetTaskId);
          if (target && !isTerminalTaskStatus(target.status)) continue;
        }
        if (record.kind === "steer" && record.onSettled === "cancel") {
          await inputs.transitionInput(
            record.inputId,
            ["pending"],
            "cancelled",
          );
          continue;
        }
        queue.push(record);
      }
      return stageNextQueuedTurn(tx, this.stageDeps(), {
        chatId: sessionId,
        session: descriptor,
        queue,
      });
    });
    if (!staged) return undefined;
    await this.deps.taskRunner.enqueue({
      taskId: staged.taskId,
      scopeId: sessionId,
    });
    return {
      kind: "turn",
      turnId: staged.taskId,
      userMessageId: staged.userMessageId,
      assistantMessageId: staged.assistantMessageId,
    };
  }

  async cancel(sessionId: string, turnId: string): Promise<void> {
    const task = await this.requireTurn(sessionId, turnId);
    const wasParked = task.status === "waiting_approval";
    await this.deps.tasks.cancelTask(turnId);
    if (wasParked) await this.cancelRequests(sessionId, turnId);
  }

  /**
   * Stop a turn RESUMABLY (distinct from {@link cancel}): the turn parks
   * `interrupted` with its id, events and records intact, and
   * {@link resume} continues it with a new attempt.
   */
  async interrupt(
    sessionId: string,
    turnId: string,
    reason?: TaskInterruptionReason,
  ): Promise<void> {
    await this.requireTurn(sessionId, turnId);
    await this.deps.tasks.interruptTask(turnId, reason);
  }

  /** Continue an interrupted turn; any other status is `invalid_state`. */
  async resume(sessionId: string, turnId: string): Promise<void> {
    const task = await this.requireTurn(sessionId, turnId);
    if (task.status !== "interrupted")
      throw new AgentKitHostError(
        "invalid_state",
        `Turn ${turnId} is ${task.status}; only an interrupted turn can resume.`,
        { sessionId, turnId, status: task.status },
      );
    await this.deps.tasks.resumeTask(turnId);
  }

  /** The session's turns, oldest first. */
  async listTurns(sessionId: string): Promise<AgentTurnSummary[]> {
    const { descriptor } = await this.requireSession(sessionId);
    return (
      await listTurnTasks(
        this.deps.store,
        sessionId,
        sessionTurnKind(descriptor),
      )
    ).map(summarizeTurn);
  }

  /** Provider-reported token usage per turn, plus the context window it fills. */
  async getUsage(sessionId: string): Promise<AgentSessionUsage> {
    const { descriptor } = await this.requireSession(sessionId);
    return sessionUsage(this.deps.store, sessionId, descriptor);
  }

  /** A JSON-serializable history: active-path messages, turns, inputs. No secrets. */
  async exportSession(sessionId: string): Promise<AgentSessionExport> {
    const { descriptor, chat } = await this.requireSession(sessionId);
    return exportSession(this.deps.store, {
      sessionId,
      descriptor,
      chatMetadata: chat.metadata,
      exportedAt: this.deps.clock.nowIso(),
    });
  }

  async answer(input: AnswerInput): Promise<AgentAnswerReceipt> {
    const outcome = await answerHumanRequest(this.deps, input);
    if (outcome.enqueue) await this.deps.taskRunner.enqueue(outcome.enqueue);
    return outcome.receipt;
  }

  /** Approve exactly the call the request names (ADR 0019). */
  approve(
    requestId: string,
    answerId: string,
    note?: string,
  ): Promise<AgentAnswerReceipt> {
    return this.answer({
      requestId,
      answerId,
      response: {
        decision: "approve",
        ...(note === undefined ? {} : { note }),
      },
    });
  }

  /** Answer the call with a denial result; it never runs. */
  deny(
    requestId: string,
    answerId: string,
    note?: string,
  ): Promise<AgentAnswerReceipt> {
    return this.answer({
      requestId,
      answerId,
      response: { decision: "deny", ...(note === undefined ? {} : { note }) },
    });
  }

  /**
   * Ask for the session's context to be compacted (ADR 0017 part 2).
   *
   * DEFERRED, not immediate: this service holds no provider client, so it only
   * records `compactRequestedAt` on the chat. The session's NEXT turn, which
   * has the pinned client, compacts even below the threshold when anything is
   * older than the kept tail, then clears the flag in the same transaction that
   * saves the summary. Refused with `chat_busy` while a turn is live, because
   * that turn is already assembling its history.
   */
  async compact(sessionId: string): Promise<void> {
    const { descriptor } = await this.requireSession(sessionId);
    const turnKind = sessionTurnKind(descriptor);
    if (await this.activeTurn(sessionId, turnKind))
      throw new ChatBusyError(`Chat ${sessionId} is busy.`, {
        chatId: sessionId,
      });
    await this.deps.store.transaction(async (tx) => {
      const chat = await tx.conversations.getChat(sessionId);
      if (!chat || !readSessionDescriptor(chat.metadata))
        throw this.notASession(sessionId);
      const tasks = await tx.tasks.listByScope(sessionId);
      if (tasks.some(isLiveTurn(turnKind)))
        throw new ChatBusyError(`Chat ${sessionId} is busy.`, {
          chatId: sessionId,
        });
      await tx.conversations.updateChat(sessionId, {
        metadata: {
          ...chat.metadata,
          [COMPACT_REQUESTED_METADATA_KEY]: this.deps.clock.nowIso(),
        },
      });
    });
  }

  /** Pending inputs and the provider continuation are NOT copied. */
  async fork(
    sessionId: string,
    fromMessageId: string,
  ): Promise<AgentSessionState> {
    const { descriptor } = await this.requireSession(sessionId);
    const forkedId = await this.deps.store.transaction(async (tx) => {
      const { chat } = await tx.conversations.forkChat(
        sessionId,
        fromMessageId,
      );
      await tx.conversations.updateChat(chat.id, {
        metadata: {
          ...chat.metadata,
          [SESSION_METADATA_KEY]: {
            ...descriptor,
            createdAt: this.deps.clock.nowIso(),
            forkedFrom: { sessionId, messageId: fromMessageId },
          },
        },
      });
      return chat.id;
    });
    return this.getState(forkedId);
  }

  /** Future turns only; an active task keeps the model it was submitted with. */
  async setModel(
    sessionId: string,
    input: {
      providerId?: string;
      model: string;
      /** Omit to keep the current effort; `null` falls back to the profile's. */
      reasoningEffort?: SessionReasoningEffort | null;
    },
  ): Promise<AgentSessionState> {
    if (typeof input.model !== "string" || input.model === "")
      invalid("model must be a non-empty string.");
    if (
      input.reasoningEffort !== undefined &&
      input.reasoningEffort !== null &&
      !REASONING_EFFORTS.includes(input.reasoningEffort)
    )
      invalid(
        `reasoningEffort must be one of ${REASONING_EFFORTS.join(", ")}.`,
      );
    await this.deps.store.transaction(async (tx) => {
      const chat = await tx.conversations.getChat(sessionId);
      const descriptor = readSessionDescriptor(chat?.metadata);
      if (!chat || !descriptor) throw this.notASession(sessionId);
      const { reasoningEffort: current, ...rest } = descriptor;
      const reasoningEffort =
        input.reasoningEffort === undefined ? current : input.reasoningEffort;
      await tx.conversations.updateChat(sessionId, {
        metadata: {
          ...chat.metadata,
          [SESSION_METADATA_KEY]: {
            ...rest,
            model: input.model,
            ...(reasoningEffort === null || reasoningEffort === undefined
              ? {}
              : { reasoningEffort }),
            ...(input.providerId === undefined
              ? {}
              : { providerId: input.providerId }),
          },
        },
      });
    });
    return this.getState(sessionId);
  }

  /**
   * Cancel every pending queued follow-up and steer of the session. Human
   * requests are untouched. Returns how many inputs were cancelled.
   */
  async clearQueue(sessionId: string): Promise<number> {
    await this.requireSession(sessionId);
    return this.deps.store.transaction(async (tx) => {
      const inputs = tx.inputs;
      if (!inputs) return 0;
      const pending = await inputs.listInputs({
        sessionId,
        kinds: ["follow_up", "steer"],
        statuses: ["pending"],
      });
      let cleared = 0;
      for (const record of pending) {
        try {
          await inputs.transitionInput(
            record.inputId,
            ["pending"],
            "cancelled",
          );
          cleared += 1;
        } catch (error) {
          // A concurrent flush or cancel won this input; it is no longer queued.
          if (!(error instanceof AgentInputTransitionError)) throw error;
        }
      }
      return cleared;
    });
  }

  /**
   * Replace the session's Skills and/or context options. Future turns only; an
   * active task keeps what it was submitted with.
   */
  async configure(
    sessionId: string,
    patch: {
      skills?: { id: string; revision: string }[];
      context?: SessionContextOptions;
    },
  ): Promise<AgentSessionState> {
    await this.deps.store.transaction(async (tx) => {
      const chat = await tx.conversations.getChat(sessionId);
      const descriptor = readSessionDescriptor(chat?.metadata);
      if (!chat || !descriptor) throw this.notASession(sessionId);
      const next = {
        ...descriptor,
        ...(patch.skills === undefined ? {} : { skills: patch.skills }),
        ...(patch.context === undefined ? {} : { context: patch.context }),
      };
      const metadata = { ...chat.metadata, [SESSION_METADATA_KEY]: next };
      const checked = readSessionDescriptor(metadata);
      if (checked === undefined)
        invalid("skills must be {id, revision} strings and context valid.");
      await tx.conversations.updateChat(sessionId, { metadata });
    });
    return this.getState(sessionId);
  }

  private inputs() {
    const inputs = this.deps.store.inputs;
    if (!inputs)
      throw new SessionsUnavailableError("The store has no agent inputs.");
    return inputs;
  }

  private stageDeps() {
    return {
      ids: this.deps.ids,
      ...(this.deps.logger ? { logger: this.deps.logger } : {}),
    };
  }

  private notASession(sessionId: string): SessionNotFoundError {
    return new SessionNotFoundError(`Session not found: ${sessionId}`, {
      sessionId,
    });
  }

  private async requireSession(sessionId: string): Promise<{
    chat: ChatRecord;
    descriptor: AgentSessionDescriptor;
    sessionId: string;
  }> {
    const chat = await this.deps.store.conversations.getChat(sessionId);
    const descriptor = readSessionDescriptor(chat?.metadata);
    if (!chat || !descriptor) throw this.notASession(sessionId);
    return { chat, descriptor, sessionId };
  }

  private async requireTurn(
    sessionId: string,
    turnId: string,
  ): Promise<TaskRecord> {
    await this.requireSession(sessionId);
    const task = await this.deps.store.tasks.getTask(turnId);
    if (!task || task.scopeId !== sessionId)
      throw new RecordNotFoundError(
        `Turn ${turnId} not found in ${sessionId}.`,
        { sessionId, turnId },
      );
    return task;
  }

  private async activeTurn(
    sessionId: string,
    turnKind: string,
  ): Promise<TaskRecord | undefined> {
    const tasks = await this.deps.store.tasks.listByScope(sessionId);
    return tasks.find(isLiveTurn(turnKind));
  }

  private async cancelRequests(
    sessionId: string,
    turnId: string,
  ): Promise<void> {
    await this.deps.store.transaction(async (tx) => {
      const requests = await tx.inputs?.listInputs({
        sessionId,
        kinds: ["human_request"],
        statuses: ["pending"],
        targetTaskId: turnId,
      });
      for (const request of requests ?? []) {
        await tx.inputs?.transitionInput(
          request.inputId,
          ["pending"],
          "cancelled",
        );
      }
    });
  }

  private async submitTurn(
    session: { descriptor: AgentSessionDescriptor; sessionId: string },
    content: SessionContent,
    inputId: string | undefined,
  ): Promise<AgentSendResult> {
    const { descriptor } = session;
    const contentSelection = sessionContentSelection(descriptor);
    const result = await this.deps.turns.submitMessage({
      chatId: session.sessionId,
      content,
      kind: sessionTurnKind(descriptor),
      ...(inputId === undefined ? {} : { taskId: turnIdFor(inputId) }),
      ...(descriptor.model === undefined ? {} : { model: descriptor.model }),
      ...(descriptor.reasoningEffort === undefined
        ? {}
        : { reasoningEffort: descriptor.reasoningEffort }),
      ...(descriptor.providerId === undefined
        ? {}
        : { providerId: descriptor.providerId }),
      ...(contentSelection === undefined ? {} : { contentSelection }),
    });
    return {
      kind: "turn",
      turnId: result.runId,
      userMessageId: result.userMessageId,
      assistantMessageId: result.assistantMessageId,
    };
  }

  /**
   * Try to start a turn; if the session is busy, store the input instead. The
   * attempt IS the idle check (the store refuses a concurrent turn), so there
   * is no check-then-write window to lose a race in.
   */
  private async submitOrQueue(
    session: { descriptor: AgentSessionDescriptor; sessionId: string },
    kind: QueueKind,
    request: QueueRequest,
  ): Promise<AgentSendResult> {
    const { sessionId } = session;
    let busy: ChatBusyError | undefined;
    for (let attempt = 0; attempt < MAX_SUBMIT_ATTEMPTS; attempt += 1) {
      if (request.inputId !== undefined) {
        const prior = await this.priorReceipt(sessionId, kind, request);
        if (prior) return prior;
      }
      try {
        return await this.submitTurn(session, request.content, request.inputId);
      } catch (error) {
        if (!(error instanceof ChatBusyError)) throw error;
        busy = error;
      }
      const active = await this.activeTurn(
        sessionId,
        sessionTurnKind(session.descriptor),
      );
      // The turn settled between the refusal and now: start over as idle.
      if (active === undefined) continue;
      return this.queueInput(sessionId, kind, request, active);
    }
    throw (
      busy ??
      new ChatBusyError(`Chat ${sessionId} is busy.`, { chatId: sessionId })
    );
  }

  private async queueInput(
    sessionId: string,
    kind: QueueKind,
    request: QueueRequest,
    active: TaskRecord,
  ): Promise<AgentSendResult> {
    const inputId = request.inputId ?? invalid("inputId is required to queue.");
    let targetTaskId: string | undefined;
    if (kind === "steer") {
      targetTaskId = request.turnId ?? active.taskId;
      const target = await this.deps.store.tasks.getTask(targetTaskId);
      if (!target || target.scopeId !== sessionId)
        throw new RecordNotFoundError(
          `Turn ${targetTaskId} not found in ${sessionId}.`,
          { sessionId, turnId: targetTaskId },
        );
    }
    const { record } = await this.inputs().createInput({
      inputId,
      sessionId,
      kind,
      content: { content: request.content },
      contentHash: await this.hashOf(kind, request.content),
      ...(targetTaskId === undefined ? {} : { targetTaskId }),
      ...(kind === "steer" && request.onSettled
        ? { onSettled: request.onSettled }
        : {}),
    });
    return this.receiptOf(record);
  }

  /** The earlier outcome of this `inputId`, or undefined when it is new. */
  private async priorReceipt(
    sessionId: string,
    kind: QueueKind,
    request: QueueRequest,
  ): Promise<AgentSendResult | undefined> {
    const inputId = request.inputId as string;
    const existing = await this.inputs().getInput(inputId);
    if (existing) {
      // createInput is the idempotency check: same input replays, a different
      // one is `input_conflict`.
      const { record } = await this.inputs().createInput({
        inputId,
        sessionId,
        kind,
        content: { content: request.content },
        contentHash: await this.hashOf(kind, request.content),
        ...(existing.targetTaskId === undefined
          ? {}
          : { targetTaskId: existing.targetTaskId }),
        ...(existing.onSettled === undefined
          ? {}
          : { onSettled: existing.onSettled }),
      });
      return this.receiptOf(record);
    }
    const turn = await this.deps.store.tasks.getTask(turnIdFor(inputId));
    return turn ? this.turnResult(turn) : undefined;
  }

  private async receiptOf(record: AgentInputRecord): Promise<AgentSendResult> {
    if (record.status === "converted" && record.appliedTaskId !== undefined) {
      const task = await this.deps.store.tasks.getTask(record.appliedTaskId);
      if (task) return this.turnResult(task);
    }
    return { kind: "queued", input: record };
  }

  private turnResult(task: TaskRecord): AgentSendResult {
    const payload = task.payload as {
      userMessageId?: string;
      assistantMessageId?: string;
    };
    return {
      kind: "turn",
      turnId: task.taskId,
      userMessageId: payload.userMessageId ?? "",
      assistantMessageId: payload.assistantMessageId ?? "",
    };
  }

  private hashOf(kind: QueueKind, content: SessionContent): Promise<string> {
    return sha256Hex(JSON.stringify({ kind, content }));
  }
}

function approvalOf(
  record: AgentInputRecord,
): AgentSessionState["waitingApproval"] {
  const content = record.content as {
    kind?: unknown;
    prompt?: unknown;
    toolName?: unknown;
    argsSha256?: unknown;
  } | null;
  if (
    content?.kind !== "approval" ||
    record.toolCallId === undefined ||
    typeof content.argsSha256 !== "string"
  )
    return undefined;
  return {
    requestId: record.inputId,
    toolCallId: record.toolCallId,
    toolName: typeof content.toolName === "string" ? content.toolName : "",
    argsSha256: content.argsSha256,
    prompt: typeof content.prompt === "string" ? content.prompt : "",
  };
}

function isLiveTurn(turnKind: string): (task: TaskRecord) => boolean {
  return (task) => task.kind === turnKind && !isTerminalTaskStatus(task.status);
}
