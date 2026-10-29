/**
 * Resuming an approved tool call (ADR 0019 part 1). An approve answer writes no
 * tool record; the next attempt finds the answered approval requests whose call
 * still has no result, checks the call against the hash the human approved, and
 * hands the survivors to the core loop to execute before its first model
 * request.
 */
import type { AiToolCall } from "@agentkit/contracts";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { MessageRecord } from "../ports/conversation-store.js";
import { argsSha256Of } from "../sessions/hash.js";
import type { TaskExecutionContext } from "../tasks/task-executor.js";
import { fencedConversations } from "./fenced-conversations.js";
import { uncertainOperationOf, type UncertainCall } from "./side-effects.js";

/** Tool failure code for an approval whose call no longer matches its hash. */
export const APPROVAL_MISMATCH_CODE = "approval_mismatch";

export interface ApprovalResume {
  /** In the assistant's declared order. */
  resumeCalls: AiToolCall[];
  /** toolCallId to the approved argsSha256. */
  approvedCalls: Map<string, string>;
  approvedRequests: Map<string, string>;
  approvedGenerations: Map<string, string>;
  /** Last run record after any mismatch records were appended. */
  lastMessageId?: string;
}

/** The marker `answerHumanRequest` leaves in `appliedMessageId` on approve. */
const APPROVE_MARKER = "approval:";

export async function resolveApprovedCalls(
  store: AssistantStore,
  ctx: TaskExecutionContext,
  input: {
    chatId: string;
    assistantMessageId: string;
    lastMessageId?: string;
    uncertain?: ReadonlyMap<string, UncertainCall>;
  },
): Promise<ApprovalResume> {
  const none: ApprovalResume = {
    resumeCalls: [],
    approvedCalls: new Map(),
    approvedRequests: new Map(),
    approvedGenerations: new Map(),
    ...(input.lastMessageId === undefined
      ? {}
      : { lastMessageId: input.lastMessageId }),
  };
  const inputs = store.inputs;
  if (inputs === undefined) return none;
  const taskId = ctx.task.taskId;
  const approved = (
    await inputs.listInputs({
      sessionId: input.chatId,
      kinds: ["human_request"],
      statuses: ["answered"],
      targetTaskId: taskId,
    })
  ).filter(
    (record) =>
      (record.content as { kind?: unknown } | null)?.kind === "approval" &&
      record.appliedMessageId?.startsWith(APPROVE_MARKER) === true &&
      record.toolCallId !== undefined,
  );
  const receipts = (await store.toolExecutions?.listByRun(taskId)) ?? [];
  if (approved.length === 0 && receipts.length === 0) return none;

  const declared = new Map<string, AiToolCall>();
  const answered = new Set<string>();
  let record: MessageRecord | null =
    input.lastMessageId === undefined
      ? null
      : await store.conversations.getMessage(input.lastMessageId);
  while (record && record.id !== input.assistantMessageId) {
    if (record.runId !== taskId) break;
    if (record.role === "tool" && record.toolCallId !== undefined)
      answered.add(record.toolCallId);
    // Walking backwards: keep the earliest declaration of an id.
    for (const call of [...(record.toolCalls ?? [])].reverse())
      declared.set(call.id, call);
    record =
      record.parentMessageId === undefined
        ? null
        : await store.conversations.getMessage(record.parentMessageId);
  }
  // `declared` was filled newest-first; assistant order is its reverse.
  const order = [...declared.keys()].reverse();

  const resumeCalls: AiToolCall[] = [];
  const approvedCalls = new Map<string, string>();
  const approvedRequests = new Map<string, string>();
  const approvedGenerations = new Map<string, string>();
  let lastMessageId = input.lastMessageId;
  for (const request of approved) {
    const toolCallId = request.toolCallId as string;
    if (answered.has(toolCallId)) continue;
    const call = declared.get(toolCallId);
    if (call === undefined) continue;
    const expected = (request.content as { argsSha256?: unknown }).argsSha256;
    if ((await hashOfCall(call)) !== expected || typeof expected !== "string") {
      lastMessageId = await appendMismatch(store, ctx, {
        chatId: input.chatId,
        call,
        parentMessageId: lastMessageId,
      });
      continue;
    }
    const generation = (request.content as { approvalGeneration?: string })
      .approvalGeneration;
    const uncertainty =
      input.uncertain === undefined
        ? undefined
        : uncertainOperationOf(input.uncertain, call.name, expected, taskId);
    const consumed = receipts.some(
      (receipt) => receipt.approvalRequestId === request.inputId,
    );
    if (
      !consumed &&
      (uncertainty === undefined ||
        (generation !== undefined &&
          generation === uncertainty.approvalGeneration))
    ) {
      approvedCalls.set(toolCallId, expected);
      approvedRequests.set(toolCallId, request.inputId);
      if (generation !== undefined)
        approvedGenerations.set(toolCallId, generation);
    }
    if (!resumeCalls.some((resumed) => resumed.id === call.id))
      resumeCalls.push(call);
  }
  // A committed result that never reached projection is replayed through the
  // wrapper, which returns the receipt without invoking the tool again.
  for (const receipt of receipts) {
    const call = declared.get(receipt.toolCallId);
    if (
      call &&
      !answered.has(call.id) &&
      !resumeCalls.some((resumed) => resumed.id === call.id)
    )
      resumeCalls.push(call);
  }
  resumeCalls.sort((a, b) => order.indexOf(a.id) - order.indexOf(b.id));
  return {
    resumeCalls,
    approvedCalls,
    approvedRequests,
    approvedGenerations,
    ...(lastMessageId === undefined ? {} : { lastMessageId }),
  };
}

async function hashOfCall(call: AiToolCall): Promise<string | undefined> {
  try {
    return await argsSha256Of(JSON.parse(call.argumentsJson) as unknown);
  } catch {
    return undefined;
  }
}

async function appendMismatch(
  store: AssistantStore,
  ctx: TaskExecutionContext,
  input: {
    chatId: string;
    call: AiToolCall;
    parentMessageId: string | undefined;
  },
): Promise<string> {
  const id = `approval-mismatch:${ctx.task.taskId}:${input.call.id}`;
  const message =
    "The arguments of this call differ from what the user approved; it was not run.";
  const envelope = JSON.stringify({
    ok: false,
    status: "error",
    summary: message,
    warnings: [],
    truncated: false,
    data: {
      errorCode: APPROVAL_MISMATCH_CODE,
      errorMessage: message,
      phase: "guard",
      retryable: false,
    },
  });
  const existing = await store.conversations.getMessage(id);
  if (existing) return existing.id;
  const record = await store.transaction((tx) =>
    fencedConversations(tx, ctx).appendMessage({
      id,
      chatId: input.chatId,
      runId: ctx.task.taskId,
      role: "tool",
      content: envelope,
      modelResultJson: envelope,
      toolCallId: input.call.id,
      ...(input.parentMessageId === undefined
        ? {}
        : { parentMessageId: input.parentMessageId }),
      activate: false,
      metadata: { internal: true, toolName: input.call.name },
    }),
  );
  return record.id;
}
