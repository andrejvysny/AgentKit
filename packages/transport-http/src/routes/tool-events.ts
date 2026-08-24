/**
 * `listToolEvents` — a chat's tool history without replaying every run's stream.
 *
 * There is no tool-event table in the host layer, so this route DERIVES the
 * list: the chat's messages name the runs (`MessageRecord.runId`), each run's
 * durable log holds its `run.tool.*` events, and each of those events projects
 * to one {@link ToolEventDto}. One row per EVENT, not one per tool call: the
 * DTO's `id` is documented as "the eventId of the originating run event" and
 * `status` as "its stage", so a call that was requested, ran and succeeded
 * appears three times and a client folds them by `toolCallId` if it wants the
 * latest state. Folding here would throw away the timeline and could not be
 * reconstructed from what was left.
 *
 * Ordering is by run (first appearance in the chat's messages) then by `seq`
 * within the run — the only ordering that is stable across calls, since event
 * timestamps are wall-clock and two runs' logs share no sequence.
 */
import type {
  AiSourceRef,
  AiToolStatus,
  TaskEventEnvelope,
  ToolEventDto,
} from "@agentkit/contracts";
import { jsonResponse, readPositiveInt } from "../http.js";
import { badRequest, notFound } from "../problem.js";
import { pathParam, type RouteContext } from "./context.js";

/** `run.tool.*` event type → the stage it puts the call in. */
const STATUS_BY_EVENT_TYPE: Readonly<Record<string, AiToolStatus>> =
  Object.freeze({
    "run.tool.requested": "requested",
    "run.tool.running": "running",
    "run.tool.succeeded": "succeeded",
    "run.tool.failed": "failed",
  });

export async function listToolEvents(ctx: RouteContext): Promise<Response> {
  const chatId = pathParam(ctx, "chatId");
  const chat = await ctx.deps.store.conversations.getChat(chatId);
  if (chat === null) return notFound(`Chat not found: ${chatId}`, ctx.instance);

  const limit = readPositiveInt(ctx.url, "limit");
  if (!limit.ok) {
    return badRequest("invalid_request", limit.message, ctx.instance);
  }

  const messages = await ctx.deps.store.conversations.listMessages(chatId);
  const runIds: string[] = [];
  const seen = new Set<string>();
  for (const message of messages) {
    const runId = message.runId;
    if (runId === undefined || seen.has(runId)) continue;
    seen.add(runId);
    runIds.push(runId);
  }

  const items: ToolEventDto[] = [];
  for (const runId of runIds) {
    const log = await ctx.deps.store.tasks.listEvents(runId);
    for (const event of [...log].sort((a, b) => a.seq - b.seq)) {
      const dto = toolEventDto(event, runId, chatId);
      if (dto !== null) items.push(dto);
    }
  }

  // The most recent N, still oldest-first: a client asking for a bounded slice
  // of a tool history wants the end of it.
  const page = limit.value === undefined ? items : items.slice(-limit.value);
  return jsonResponse(page);
}

/**
 * Project one event, or null when it is not a tool event.
 *
 * Everything is read defensively: the log is `TaskEventEnvelope`, whose `data`
 * is open by design, and a host that appended a malformed tool event should
 * cost this route one row rather than the whole response.
 */
function toolEventDto(
  event: TaskEventEnvelope,
  runId: string,
  chatId: string,
): ToolEventDto | null {
  const status = STATUS_BY_EVENT_TYPE[event.type];
  if (status === undefined) return null;
  const data = (event as { data?: unknown }).data;
  if (data === null || typeof data !== "object") return null;
  const fields = data as Record<string, unknown>;
  const toolCallId = str(fields["toolCallId"]);
  const toolName = str(fields["toolName"]);
  if (toolCallId === undefined || toolName === undefined) return null;

  const sources = fields["sources"];
  const warnings = fields["warnings"];
  const truncated = fields["truncated"];

  return {
    id: event.eventId,
    runId,
    chatId,
    toolCallId,
    toolName,
    status,
    ...optional("argumentsJson", str(fields["argumentsJson"])),
    ...optional("resultJson", str(fields["resultJson"])),
    ...optional("modelResultJson", str(fields["modelResultJson"])),
    ...optional("summary", str(fields["summary"])),
    ...optional("errorCode", str(fields["errorCode"])),
    ...optional("errorMessage", str(fields["errorMessage"])),
    ...(Array.isArray(sources) ? { sources: sources as AiSourceRef[] } : {}),
    ...(Array.isArray(warnings)
      ? { warnings: warnings.filter((w): w is string => typeof w === "string") }
      : {}),
    ...(typeof truncated === "boolean" ? { truncated } : {}),
    createdAt: event.timestamp,
  };
}

function str(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

function optional<K extends string>(
  key: K,
  value: string | undefined,
): Record<K, string> | Record<string, never> {
  return value === undefined ? {} : ({ [key]: value } as Record<K, string>);
}
