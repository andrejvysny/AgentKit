/**
 * `AgentSessionService` (ADR 0016 §6): a facade over the turn runner, the task
 * service and the durable input store. It adds no scheduler, queue runner or
 * store of its own: a session is a chat carrying a descriptor, a turn is a
 * `chat.turn` task, and everything that cannot start a turn right now is an
 * `AgentInputStore` record.
 */
import {
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
import type { TaskRunner } from "../ports/task-runner.js";
import { CHAT_TURN_TASK_KIND } from "../tasks/kinds.js";
import type { TaskService } from "../tasks/task-service.js";
import { stageNextQueuedTurn } from "../turn/session-turn.js";
import type { TurnRunner } from "../turn/turn-runner.js";
import {
  SESSION_METADATA_KEY,
  readSessionDescriptor,
  type AgentSessionDescriptor,
} from "./descriptor.js";
import { sha256Hex } from "./hash.js";
import { answerHumanRequest } from "./human-answer.js";
import type {
  AgentAnswerReceipt,
  AgentSendResult,
  AgentSessionState,
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
    const raw = {
      version: 1,
      profile: input.profile,
      ...(input.providerId === undefined
        ? {}
        : { providerId: input.providerId }),
      ...(input.model === undefined ? {} : { model: input.model }),
      createdAt: this.deps.clock.nowIso(),
    };
    const metadata = { ...input.metadata, [SESSION_METADATA_KEY]: raw };
    if (readSessionDescriptor(metadata) === undefined)
      invalid("profile.id must be a non-empty string.");
    const chat = await this.deps.store.conversations.createChat({
      ...(input.sessionId === undefined ? {} : { id: input.sessionId }),
      ...(input.title === undefined ? {} : { title: input.title }),
      metadata,
    });
    return this.getState(chat.id);
  }

  async getState(sessionId: string): Promise<AgentSessionState> {
    const { descriptor } = await this.requireSession(sessionId);
    const active = await this.activeTurn(sessionId);
    const records = await this.inputs().listInputs({
      sessionId,
      statuses: ["pending"],
    });
    const waiting = records.find((record) => record.kind === "human_request");
    return {
      sessionId,
      descriptor,
      ...(active
        ? { activeTurn: { turnId: active.taskId, status: active.status } }
        : {}),
      pending: records.filter((record) => record.kind !== "human_request"),
      ...(waiting ? { waiting } : {}),
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
        (await tx.tasks.listByScope(sessionId)).filter(isLiveTurn),
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
    await this.requireSession(sessionId);
    const task = await this.deps.store.tasks.getTask(turnId);
    if (!task || task.scopeId !== sessionId)
      throw new RecordNotFoundError(
        `Turn ${turnId} not found in ${sessionId}.`,
        {
          sessionId,
          turnId,
        },
      );
    const wasParked = task.status === "waiting_approval";
    await this.deps.tasks.cancelTask(turnId);
    if (wasParked) await this.cancelRequests(sessionId, turnId);
  }

  async answer(input: AnswerInput): Promise<AgentAnswerReceipt> {
    const outcome = await answerHumanRequest(this.deps, input);
    if (outcome.enqueue) await this.deps.taskRunner.enqueue(outcome.enqueue);
    return outcome.receipt;
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
    input: { providerId?: string; model: string },
  ): Promise<AgentSessionState> {
    if (typeof input.model !== "string" || input.model === "")
      invalid("model must be a non-empty string.");
    await this.deps.store.transaction(async (tx) => {
      const chat = await tx.conversations.getChat(sessionId);
      const descriptor = readSessionDescriptor(chat?.metadata);
      if (!chat || !descriptor) throw this.notASession(sessionId);
      await tx.conversations.updateChat(sessionId, {
        metadata: {
          ...chat.metadata,
          [SESSION_METADATA_KEY]: {
            ...descriptor,
            model: input.model,
            ...(input.providerId === undefined
              ? {}
              : { providerId: input.providerId }),
          },
        },
      });
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

  private async activeTurn(sessionId: string): Promise<TaskRecord | undefined> {
    const tasks = await this.deps.store.tasks.listByScope(sessionId);
    return tasks.find(isLiveTurn);
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
    const result = await this.deps.turns.submitMessage({
      chatId: session.sessionId,
      content,
      ...(inputId === undefined ? {} : { taskId: turnIdFor(inputId) }),
      ...(descriptor.model === undefined ? {} : { model: descriptor.model }),
      ...(descriptor.providerId === undefined
        ? {}
        : { providerId: descriptor.providerId }),
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
      const active = await this.activeTurn(sessionId);
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

function isLiveTurn(task: TaskRecord): boolean {
  return (
    task.kind === CHAT_TURN_TASK_KIND && !isTerminalTaskStatus(task.status)
  );
}
