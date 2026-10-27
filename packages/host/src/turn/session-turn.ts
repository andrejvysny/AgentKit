/**
 * The session-mode half of a chat turn (ADR 0016 §2-§5): steering, parking for
 * human input, the resume guard and settlement conversion. Kept apart from
 * `turn-runner.ts` so the non-session path stays exactly as it was; every
 * function here is only reached when the chat carries a session descriptor.
 */
import type { AgentSteeringSource } from "@agentkit/core";
import { ChatBusyError } from "../errors.js";
import type {
  AgentInputRecord,
  AgentInputStore,
} from "../ports/agent-input-store.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { IdGenerator, Logger } from "../ports/system.js";
import type { TaskStatus } from "../ports/task-store.js";
import { CHAT_TURN_TASK_KIND } from "../tasks/kinds.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";
import type { AgentSessionDescriptor } from "../sessions/descriptor.js";
import { readInputContent } from "../sessions/input-content.js";
import { fencedConversations } from "./fenced-conversations.js";
import { stageTurnInTransaction, type TurnRequest } from "./submit.js";
import type { TurnRunnerDeps } from "./turn-runner.js";

/** Steers for this turn: untargeted, or aimed at this task. */
export function sessionSteering(
  inputs: AgentInputStore,
  deps: Pick<TurnRunnerDeps, "logger">,
  chatId: string,
  taskId: string,
): AgentSteeringSource {
  return {
    async pending() {
      const records = await inputs.listInputs({
        sessionId: chatId,
        kinds: ["steer"],
        statuses: ["pending"],
      });
      const steers = [];
      for (const record of records) {
        if (record.targetTaskId !== undefined && record.targetTaskId !== taskId)
          continue;
        const content = readInputContent(record.content);
        if (content === undefined) {
          deps.logger?.warn("skipping a steer with malformed content", {
            inputId: record.inputId,
            taskId,
          });
          continue;
        }
        steers.push({
          inputId: record.inputId,
          message: { role: "user" as const, content },
        });
      }
      return steers;
    },
  };
}

/** True while this task has a human request nobody has answered. */
export async function hasUnansweredRequest(
  inputs: AgentInputStore,
  chatId: string,
  taskId: string,
): Promise<boolean> {
  const pending = await inputs.listInputs({
    sessionId: chatId,
    kinds: ["human_request"],
    statuses: ["pending"],
    targetTaskId: taskId,
  });
  return pending.length > 0;
}

/**
 * Park the task on a human request. The fenced task write goes first, so a
 * lost lease aborts before any conversation write; the attempt ends
 * `completed` because it did its work cleanly (ADR 0016 §5).
 */
export async function parkTurn(
  store: AssistantStore,
  ctx: TaskExecutionContext,
  assistantMessageId: string,
  content: string,
): Promise<void> {
  await store.transaction(async (tx) => {
    await tx.tasks.transitionTask(
      ctx.task.taskId,
      ["running"],
      "waiting_approval",
      {},
      { leaseToken: ctx.leaseToken },
    );
    await tx.tasks.endAttempt({
      attemptId: ctx.attemptId,
      status: "completed",
      leaseToken: ctx.leaseToken,
    });
    await fencedConversations(tx, ctx).updateMessage(assistantMessageId, {
      content,
    });
  });
}

export interface SettleSessionInput {
  ctx: TaskExecutionContext;
  chatId: string;
  session: AgentSessionDescriptor;
  status: TaskStatus;
  assistantMessageId: string;
}

/**
 * Inside the terminal transaction, AFTER the fenced task write: cancel the
 * leftover steers whose policy says so and, when the turn completed, turn the
 * oldest queued follow-up (or leftover steer) into the next turn. Returns the
 * staged task id; the caller enqueues it after commit.
 */
export async function settleSessionInputs(
  tx: AssistantStore,
  deps: TurnRunnerDeps,
  input: SettleSessionInput,
): Promise<string | undefined> {
  const inputs = tx.inputs;
  if (!inputs) return undefined;
  const { chatId, ctx } = input;
  const taskId = ctx.task.taskId;
  const leftovers = (
    await inputs.listInputs({
      sessionId: chatId,
      kinds: ["steer"],
      statuses: ["pending"],
    })
  ).filter(
    (record) =>
      record.targetTaskId === undefined || record.targetTaskId === taskId,
  );
  for (const record of leftovers) {
    if (record.onSettled === "cancel")
      await inputs.transitionInput(record.inputId, ["pending"], "cancelled");
  }
  if (input.status !== "completed") return undefined;

  const followUps = await inputs.listInputs({
    sessionId: chatId,
    kinds: ["follow_up"],
    statuses: ["pending"],
  });
  const queue = [
    ...followUps,
    ...leftovers.filter((record) => record.onSettled !== "cancel"),
  ].sort((a, b) => a.order - b.order);
  try {
    const staged = await stageNextQueuedTurn(tx, deps, {
      chatId,
      session: input.session,
      parentMessageId: input.assistantMessageId,
      queue,
    });
    return staged?.taskId;
  } catch (error) {
    // The scope already holds unfinished work; the input stays queued and the
    // session's own flush releases it later. Nothing was written (the task
    // row is the first write), so the settlement is not poisoned.
    if (!(error instanceof ChatBusyError)) throw error;
    deps.logger?.warn("could not stage the next turn; chat is busy", {
      taskId,
    });
    return undefined;
  }
}

export interface StageQueuedTurnDeps {
  ids: IdGenerator;
  logger?: Logger | undefined;
  allowConcurrentSubmit?: boolean | undefined;
}

export interface StageQueuedTurnInput {
  chatId: string;
  session: AgentSessionDescriptor;
  /** The new user message's parent; absent appends to the chat's active leaf. */
  parentMessageId?: string;
  /** Candidates in FIFO order; the first with readable content is converted. */
  queue: readonly AgentInputRecord[];
}

export interface StagedQueuedTurn {
  taskId: string;
  inputId: string;
  userMessageId: string;
  assistantMessageId: string;
}

/**
 * The conversion core shared by settlement and `AgentSessionService.flushQueue`:
 * inside the caller's transaction, stage the oldest convertible queued input as
 * a `chat.turn` task and CAS it `converted`. The caller enqueues the returned
 * task after commit. Throws {@link ChatBusyError} when the scope still holds
 * unfinished work (nothing is written then); returns undefined when no
 * candidate has readable content.
 */
export async function stageNextQueuedTurn(
  tx: AssistantStore,
  deps: StageQueuedTurnDeps,
  input: StageQueuedTurnInput,
): Promise<StagedQueuedTurn | undefined> {
  const inputs = tx.inputs;
  if (!inputs) return undefined;
  const { chatId, session } = input;
  for (const next of input.queue) {
    const content = readInputContent(next.content);
    if (content === undefined) {
      deps.logger?.warn("skipping a queued input with malformed content", {
        inputId: next.inputId,
      });
      continue;
    }
    const taskId = deps.ids.taskId();
    const userMessageId = deps.ids.messageId();
    const assistantMessageId = deps.ids.messageId();
    const payload: TurnRequest = {
      chatId,
      ...(input.parentMessageId === undefined
        ? {}
        : { parentMessageId: input.parentMessageId }),
      ...(session.model === undefined ? {} : { model: session.model }),
      ...(session.providerId === undefined
        ? {}
        : { providerId: session.providerId }),
      assistantMessageId,
      userMessageId,
    };
    await stageTurnInTransaction(tx, deps, {
      taskId,
      kind: CHAT_TURN_TASK_KIND,
      chatId,
      content,
      userMessageId,
      assistantMessageId,
      payload,
      ...(input.parentMessageId === undefined
        ? {}
        : { parentMessageId: input.parentMessageId }),
    });
    await inputs.transitionInput(next.inputId, ["pending"], "converted", {
      appliedTaskId: taskId,
    });
    return { taskId, inputId: next.inputId, userMessageId, assistantMessageId };
  }
  return undefined;
}
