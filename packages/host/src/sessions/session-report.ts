/**
 * Read-only reports over a session: its turns, usage and a portable export.
 * Pure queries over the store; nothing here writes.
 */
import type { AssistantStore } from "../ports/assistant-store.js";
import type { TaskRecord } from "../ports/task-store.js";
import { resolveContextWindow } from "../context/budget.js";
import { sessionWithAdmittedContext } from "./admitted-context.js";
import { type AgentSessionDescriptor, sessionTurnKind } from "./descriptor.js";
import { readSessionExecution } from "./plan-mode.js";
import type {
  AgentExportMessage,
  AgentSessionExport,
  AgentSessionUsage,
  AgentTurnSummary,
  AgentTurnUsage,
} from "./session-types.js";

/**
 * The ONLY message metadata keys an export carries. An allow-list, because the
 * rest of that bag is host bookkeeping (`placeholder`, `canonicalProvider*`) or
 * caller-supplied, and provider continuation / encrypted reasoning state must
 * never leave the store. `reasoningContent` is the model's public reasoning text.
 */
const EXPORT_METADATA_KEYS = [
  "internal",
  "toolName",
  "steering",
  "inputId",
  "reasoningContent",
] as const;

function str(value: unknown): string | undefined {
  return typeof value === "string" && value !== "" ? value : undefined;
}

export async function listTurnTasks(
  store: AssistantStore,
  sessionId: string,
  kind: string,
): Promise<TaskRecord[]> {
  const tasks = await store.tasks.listByScope(sessionId);
  return tasks
    .filter((task) => task.kind === kind)
    .sort(
      (a, b) =>
        a.enqueuedAt.localeCompare(b.enqueuedAt) ||
        a.taskId.localeCompare(b.taskId),
    );
}

export function summarizeTurn(task: TaskRecord): AgentTurnSummary {
  const model = str(task.payload["model"]);
  const providerId = str(task.payload["providerId"]);
  const error = str(task.error);
  return {
    turnId: task.taskId,
    status: task.status,
    attempts: task.attemptCount,
    createdAt: task.enqueuedAt,
    ...(task.finishedAt === undefined ? {} : { finishedAt: task.finishedAt }),
    ...(model === undefined ? {} : { model }),
    ...(providerId === undefined ? {} : { providerId }),
    ...(error === undefined ? {} : { error }),
  };
}

function num(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

async function turnUsage(
  store: AssistantStore,
  task: TaskRecord,
): Promise<AgentTurnUsage> {
  const usage: AgentTurnUsage = {
    turnId: task.taskId,
    requests: 0,
    promptTokens: 0,
    completionTokens: 0,
    totalTokens: 0,
  };
  for (const event of await store.tasks.listEvents(task.taskId)) {
    if (event.type === "run.context.admitted") {
      usage.lastContextAdmission = (
        event as import("@agentkit/contracts").AiRunContextAdmittedEvent
      ).data;
    }
    if (event.type !== "run.usage") continue;
    const data = (event as { data?: Record<string, unknown> }).data;
    // Only settled numbers: a non-final event is superseded by a later one for
    // the same call, so summing it would double count.
    if (!data || data["finalForCall"] !== true) continue;
    usage.requests += 1;
    usage.promptTokens += num(data["promptTokens"]);
    usage.completionTokens += num(data["completionTokens"]);
    usage.totalTokens += num(data["totalTokens"]);
    if (typeof data["promptTokens"] === "number")
      usage.lastPromptTokens = data["promptTokens"];
  }
  return usage;
}

export async function sessionUsage(
  store: AssistantStore,
  sessionId: string,
  descriptor: AgentSessionDescriptor,
): Promise<AgentSessionUsage> {
  const tasks = await listTurnTasks(
    store,
    sessionId,
    sessionTurnKind(descriptor),
  );
  const turns: AgentTurnUsage[] = [];
  for (const task of tasks) turns.push(await turnUsage(store, task));
  const totals = {
    requests: 0,
    promptTokens: 0,
    completionTokens: 0,
    totalTokens: 0,
  };
  for (const turn of turns) {
    totals.requests += turn.requests;
    totals.promptTokens += turn.promptTokens;
    totals.completionTokens += turn.completionTokens;
    totals.totalTokens += turn.totalTokens;
  }
  // An executing/parked turn wins over later queued work admitted under future settings.
  const latest =
    [...tasks]
      .reverse()
      .find((task) =>
        ["running", "waiting_approval", "interrupted"].includes(task.status),
      ) ?? tasks[tasks.length - 1];
  const lastPromptTokens = turns.find(
    (turn) => turn.turnId === latest?.taskId,
  )?.lastPromptTokens;
  const admission = turns.find(
    (turn) => turn.turnId === latest?.taskId,
  )?.lastContextAdmission;
  const modelId = str(latest?.payload["model"]) ?? descriptor.model;
  const providerId =
    str(latest?.payload["providerId"]) ?? descriptor.providerId;
  const models =
    providerId === undefined
      ? []
      : await store.providers.listModels(providerId);
  const window = admission
    ? { windowTokens: admission.windowTokens, source: admission.windowSource }
    : latest && "sessionContext" in latest.payload
      ? resolveContextWindow({
          descriptor: sessionWithAdmittedContext(descriptor, latest.payload),
          model: models.find((candidate) => candidate.modelId === modelId),
          capabilities: null,
        })
      : { windowTokens: null, source: "unknown" as const };
  return {
    sessionId,
    turns,
    totals,
    contextWindowTokens: window.windowTokens,
    contextWindowSource: window.source,
    ...(latest === undefined ? {} : { contextTurnId: latest.taskId }),
    ...(admission === undefined ? {} : { contextAdmission: admission }),
    ...(lastPromptTokens === undefined ? {} : { lastPromptTokens }),
  };
}

export async function exportSession(
  store: AssistantStore,
  input: {
    sessionId: string;
    descriptor: AgentSessionDescriptor;
    chatMetadata: Record<string, unknown>;
    exportedAt: string;
  },
): Promise<AgentSessionExport> {
  const { sessionId } = input;
  const records = await store.conversations.listMessages(sessionId);
  const messages: AgentExportMessage[] = records.map((record) => {
    const metadata: Record<string, unknown> = {};
    for (const key of EXPORT_METADATA_KEYS)
      if (record.metadata[key] !== undefined)
        metadata[key] = record.metadata[key];
    return {
      id: record.id,
      role: record.role,
      content: record.content,
      ...(record.toolCalls === undefined
        ? {}
        : { toolCalls: record.toolCalls }),
      ...(record.toolCallId === undefined
        ? {}
        : { toolCallId: record.toolCallId }),
      ...(record.runId === undefined ? {} : { runId: record.runId }),
      metadata,
      createdAt: record.createdAt,
    };
  });
  const execution = readSessionExecution(input.chatMetadata);
  const summaries = store.summaries
    ? (await store.summaries.listSummaries(sessionId))
        .reverse()
        .map((summary) => ({
          summaryId: summary.summaryId,
          coversThroughMessageId: summary.coversThroughMessageId,
          firstKeptMessageId: summary.firstKeptMessageId,
          summary: summary.summary,
          model: summary.model,
          ...(summary.providerId === undefined
            ? {}
            : { providerId: summary.providerId }),
          createdAt: summary.createdAt,
        }))
    : undefined;
  return {
    format: "agentkit.session.export",
    version: 1,
    exportedAt: input.exportedAt,
    sessionId,
    descriptor: input.descriptor,
    ...(execution === undefined ? {} : { execution }),
    messages,
    turns: (
      await listTurnTasks(store, sessionId, sessionTurnKind(input.descriptor))
    ).map(summarizeTurn),
    inputs: store.inputs ? await store.inputs.listInputs({ sessionId }) : [],
    ...(summaries === undefined ? {} : { summaries }),
  };
}
