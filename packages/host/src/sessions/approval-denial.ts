import type { AiMessageContent } from "@agentkit/contracts";
import type { AgentInputRecord } from "../ports/agent-input-store.js";
import type { AssistantStore } from "../ports/assistant-store.js";
import type { MessageRecord } from "../ports/conversation-store.js";
import { AgentKitHostError } from "../errors.js";

const ORDINARY_DENIAL = "The user denied this tool call.";
const RETRY_DENIAL =
  "The user denied this retry. This retry was not executed. The earlier execution may already have produced effects; its outcome remains unknown. Inspect current state before reporting what changed.";

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function generationOf(request: AgentInputRecord): string | undefined {
  const content = request.content;
  if (!object(content) || content.kind !== "approval") return undefined;
  return typeof content.approvalGeneration === "string" &&
    content.approvalGeneration !== ""
    ? content.approvalGeneration
    : undefined;
}

function retryDenial(
  envelope: Record<string, unknown>,
  data: Record<string, unknown>,
  generation: string,
  note?: string,
): string {
  return JSON.stringify({
    ...envelope,
    summary: RETRY_DENIAL,
    data: {
      ...data,
      errorMessage:
        note === undefined
          ? RETRY_DENIAL
          : `${RETRY_DENIAL} User note: ${note}`,
      deniedScope: "uncertainty_retry",
      priorExecutionOutcome: "unknown",
      approvalGeneration: generation,
      ...(note === undefined ? {} : { note }),
    },
  });
}

export function approvalDenialEnvelope(
  request: AgentInputRecord,
  note?: string,
): string {
  const data = {
    errorCode: "approval_denied",
    errorMessage: note ?? ORDINARY_DENIAL,
    phase: "guard",
    retryable: false,
  };
  const envelope = {
    ok: false,
    status: "error",
    summary: ORDINARY_DENIAL,
    warnings: [],
    truncated: false,
    data,
  };
  const generation = generationOf(request);
  return generation === undefined
    ? JSON.stringify(envelope)
    : retryDenial(envelope, data, generation, note);
}

function canonicalAnswer(
  request: AgentInputRecord | undefined,
  record: MessageRecord,
): request is AgentInputRecord {
  return (
    request !== undefined &&
    request.kind === "human_request" &&
    request.status === "answered" &&
    request.inputId === record.metadata["requestId"] &&
    request.sessionId === record.chatId &&
    typeof record.runId === "string" &&
    request.targetTaskId === record.runId &&
    typeof record.toolCallId === "string" &&
    request.toolCallId === record.toolCallId &&
    request.appliedMessageId === record.id &&
    typeof request.answerId === "string" &&
    request.answerId === record.metadata["answerId"]
  );
}

function denialFrom(
  record: MessageRecord,
):
  | { envelope: Record<string, unknown>; data: Record<string, unknown> }
  | undefined {
  if (
    record.role !== "tool" ||
    record.metadata["humanAnswer"] !== true ||
    typeof record.metadata["requestId"] !== "string" ||
    typeof record.content !== "string"
  )
    return undefined;
  let envelope: unknown;
  try {
    envelope = JSON.parse(record.content) as unknown;
  } catch {
    return undefined;
  }
  if (
    !object(envelope) ||
    envelope.ok !== false ||
    envelope.status !== "error" ||
    !object(envelope.data) ||
    envelope.data.errorCode !== "approval_denied"
  )
    return undefined;
  return { envelope, data: envelope.data };
}

async function denialRequest(
  store: AssistantStore,
  requestId: string,
): Promise<AgentInputRecord | undefined> {
  if (store.inputs === undefined)
    throw new AgentKitHostError(
      "recovery_history_unreadable",
      "Cannot read the approval context for a recovered denial.",
    );
  return store.inputs.getInput(requestId).catch(() => {
    throw new AgentKitHostError(
      "recovery_history_unreadable",
      "Cannot read the approval context for a recovered denial.",
    );
  });
}

function denialNote(data: Record<string, unknown>): string | undefined {
  if (typeof data.note === "string") return data.note;
  if (
    data.deniedScope === "uncertainty_retry" ||
    data.errorMessage === ORDINARY_DENIAL
  )
    return undefined;
  return typeof data.errorMessage === "string" ? data.errorMessage : undefined;
}

/** Enrich old denial replay without rewriting its durable answer or response hash. */
export async function providerHumanAnswerContent(
  store: AssistantStore,
  record: MessageRecord,
): Promise<AiMessageContent> {
  const denial = denialFrom(record);
  if (denial === undefined) return record.content;
  const request = await denialRequest(
    store,
    record.metadata["requestId"] as string,
  );
  if (!canonicalAnswer(request, record)) return record.content;
  const generation = generationOf(request);
  return generation === undefined
    ? record.content
    : retryDenial(
        denial.envelope,
        denial.data,
        generation,
        denialNote(denial.data),
      );
}
