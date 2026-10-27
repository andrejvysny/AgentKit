import {
  AgentInputTransitionError,
  AgentKitHostError,
  AnswerConflictError,
  AnswerStaleError,
  InvalidTaskTransitionError,
  RecordNotFoundError,
  SessionsUnavailableError,
} from "../errors.js";
import type { AgentInputRecord } from "../ports/agent-input-store.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { Clock, IdGenerator } from "../ports/system.js";
import { sha256Hex } from "./hash.js";
import type { AgentAnswerReceipt, AnswerInput } from "./session-types.js";

export interface AnswerDeps {
  store: AssistantStore;
  clock: Clock;
  ids: IdGenerator;
}

/** The task to enqueue after the answer's transaction commits. */
export interface AnswerOutcome {
  receipt: AgentAnswerReceipt;
  enqueue?: { taskId: string; scopeId: string };
}

function toolName(content: unknown): string {
  const name = (content as { toolName?: unknown } | null)?.toolName;
  return typeof name === "string" ? name : "request_user_input";
}

/**
 * Record a human's answer and re-admit the parked task, all in ONE transaction
 * (ADR 0016 §5). The task transition goes first: it is the write that rejects
 * when the turn was cancelled or already resumed, and the memory adapter cannot
 * roll back what was written before it.
 */
export async function answerHumanRequest(
  deps: AnswerDeps,
  input: AnswerInput,
): Promise<AnswerOutcome> {
  const { requestId, answerId } = input;
  const serialized = JSON.stringify(input.response);
  if (serialized === undefined)
    throw new AgentKitHostError(
      "invalid_input",
      "response must be JSON-serializable.",
      { requestId },
    );
  const responseHash = await sha256Hex(serialized);
  const now = deps.clock.nowIso();
  const messageId = deps.ids.messageId();
  const envelopeJson = JSON.stringify({
    ok: true,
    status: "ok",
    summary: "The user answered.",
    warnings: [],
    truncated: false,
    data: { answer: input.response },
  });

  return deps.store.transaction(async (tx) => {
    const inputs = tx.inputs;
    if (!inputs)
      throw new SessionsUnavailableError("The store has no agent inputs.");
    const request = await inputs.getInput(requestId);
    if (!request || request.kind !== "human_request")
      throw new RecordNotFoundError(`Human request not found: ${requestId}`, {
        requestId,
      });
    const taskId = request.targetTaskId;
    const stale = (reason: string): AnswerStaleError =>
      new AnswerStaleError(
        `Request ${requestId} can no longer be answered: ${reason}.`,
        {
          requestId,
          reason,
        },
      );
    if (taskId === undefined) throw stale("it names no turn");
    const task = await tx.tasks.getTask(taskId);

    if (request.status === "answered") {
      const recorded = request.appliedMessageId
        ? await tx.conversations.getMessage(request.appliedMessageId)
        : null;
      // REPAIR, not conflict: the receipt names a tool record that is not
      // there. Only a store without rollback (the memory adapter) can leave
      // that state, by failing between the CAS and the append. The same
      // answerId is the caller retrying that very answer, so write the record
      // it was owed under the id the receipt already promised; refusing would
      // let the resumed turn run on a synthetic "missing result" instead.
      if (
        recorded === null &&
        request.appliedMessageId !== undefined &&
        request.answerId === answerId
      ) {
        const last = await tx.conversations.lastMessageOfRun(
          request.sessionId,
          taskId,
        );
        if (!last)
          throw new RecordNotFoundError(`Turn ${taskId} has no run records.`, {
            taskId,
          });
        await appendAnswer(tx, {
          id: request.appliedMessageId,
          request,
          taskId,
          parentMessageId: last.id,
          envelopeJson,
          answerId,
          responseHash,
        });
        return {
          receipt: {
            requestId,
            answerId,
            turnId: taskId,
            messageId: request.appliedMessageId,
            duplicate: false,
          },
          ...(task?.status === "queued"
            ? { enqueue: { taskId, scopeId: request.sessionId } }
            : {}),
        };
      }
      if (
        request.answerId !== answerId ||
        recorded?.metadata["responseHash"] !== responseHash
      ) {
        throw new AnswerConflictError(
          `Request ${requestId} was already answered differently.`,
          { requestId },
        );
      }
      return {
        receipt: {
          requestId,
          answerId,
          turnId: taskId,
          messageId: request.appliedMessageId ?? "",
          duplicate: true,
        },
        // The first answer may have committed and died before it could poke.
        ...(task?.status === "queued"
          ? { enqueue: { taskId, scopeId: request.sessionId } }
          : {}),
      };
    }
    if (request.status === "cancelled") throw stale("it was cancelled");
    if (!task || task.status !== "waiting_approval")
      throw stale("its turn is no longer waiting");
    if (
      request.expiresAt !== undefined &&
      Date.parse(request.expiresAt) <= Date.parse(now)
    )
      throw stale("it expired");

    const last = await tx.conversations.lastMessageOfRun(
      request.sessionId,
      taskId,
    );
    if (!last)
      throw new RecordNotFoundError(`Turn ${taskId} has no run records.`, {
        taskId,
      });

    try {
      await tx.tasks.transitionTask(taskId, ["waiting_approval"], "queued", {
        availableAt: now,
      });
      await inputs.transitionInput(requestId, ["pending"], "answered", {
        answerId,
        appliedMessageId: messageId,
      });
    } catch (error) {
      if (
        error instanceof InvalidTaskTransitionError ||
        error instanceof AgentInputTransitionError
      )
        throw stale("a concurrent answer or cancel won");
      throw error;
    }
    await appendAnswer(tx, {
      id: messageId,
      request,
      taskId,
      parentMessageId: last.id,
      envelopeJson,
      answerId,
      responseHash,
    });
    return {
      receipt: {
        requestId,
        answerId,
        turnId: taskId,
        messageId,
        duplicate: false,
      },
      enqueue: { taskId, scopeId: request.sessionId },
    };
  });
}

/** The human's answer, as the parked call's tool result in the run chain. */
async function appendAnswer(
  tx: AssistantStore,
  input: {
    id: string;
    request: AgentInputRecord;
    taskId: string;
    parentMessageId: string;
    envelopeJson: string;
    answerId: string;
    responseHash: string;
  },
): Promise<void> {
  const { request } = input;
  await tx.conversations.appendMessage({
    id: input.id,
    chatId: request.sessionId,
    runId: input.taskId,
    role: "tool",
    content: input.envelopeJson,
    modelResultJson: input.envelopeJson,
    ...(request.toolCallId === undefined
      ? {}
      : { toolCallId: request.toolCallId }),
    parentMessageId: input.parentMessageId,
    activate: false,
    metadata: {
      internal: true,
      toolName: toolName(request.content),
      humanAnswer: true,
      answerId: input.answerId,
      requestId: request.inputId,
      responseHash: input.responseHash,
    },
  });
}
