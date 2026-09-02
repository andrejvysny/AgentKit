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
 *
 * BOUNDED AT EVERY STEP, which is why the walk below runs backwards. The
 * answer is "the most recent N", so the newest run is where the answer starts:
 * messages are paged newest-first through `beforeOrderKey`, each run's log is
 * read in `EVENT_BATCH` pages through `afterSeq`, and the walk stops as soon as
 * `limit` rows are in hand. The obvious implementation — list every message,
 * read every run's whole log, then slice the tail — asks a chat with a thousand
 * turns to materialize a thousand logs in order to return ten rows, and does it
 * on a request whose `?limit=1` says the caller wanted almost nothing.
 */
import type {
  AiSourceRef,
  AiToolStatus,
  TaskEventEnvelope,
  ToolEventDto,
} from "@agentkit/contracts";
import type { TaskStore } from "@agentkit/host";
import { jsonResponse, readPositiveInt } from "../http.js";
import { badRequest, notFound } from "../problem.js";
import { pathParam, type RouteContext } from "./context.js";

/** Rows returned when the caller names no `limit`. `readPositiveInt` caps it. */
const DEFAULT_TOOL_EVENT_LIMIT = 200;

/** Messages read per backwards page while collecting run ids. */
const MESSAGE_BATCH = 200;

/** Events read per `listEvents` page while walking one run's log. */
const EVENT_BATCH = 200;

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

  const budget = limit.value ?? DEFAULT_TOOL_EVENT_LIMIT;
  const newestFirst: ToolEventDto[][] = [];
  let collected = 0;
  for await (const runId of runIdsNewestFirst(ctx, chatId)) {
    if (collected >= budget) break;
    const rows = await tailOfRun(
      ctx.deps.store.tasks,
      runId,
      chatId,
      budget - collected,
    );
    if (rows.length === 0) continue;
    newestFirst.push(rows);
    collected += rows.length;
  }

  // Collected newest run first; answered oldest run first, which is the order
  // the route has always returned and the only one a client can read as a
  // timeline.
  return jsonResponse(newestFirst.reverse().flat());
}

/**
 * The chat's run ids, newest first, read one backwards page of messages at a
 * time.
 *
 * A generator rather than a list because the caller usually wants the first
 * one or two: `?limit=10` on a chat with a thousand turns must not page a
 * thousand messages to answer.
 */
async function* runIdsNewestFirst(
  ctx: RouteContext,
  chatId: string,
): AsyncGenerator<string> {
  const seen = new Set<string>();
  let beforeOrderKey: number | undefined;
  for (;;) {
    const page = await ctx.deps.store.conversations.listMessages(chatId, {
      limit: MESSAGE_BATCH,
      ...(beforeOrderKey === undefined ? {} : { beforeOrderKey }),
    });
    if (page.length === 0) return;
    for (let i = page.length - 1; i >= 0; i--) {
      const runId = page[i]?.runId;
      if (runId === undefined || seen.has(runId)) continue;
      seen.add(runId);
      yield runId;
    }
    // A short page is the top of the path; anything else keeps walking up from
    // the oldest key this page carried.
    if (page.length < MESSAGE_BATCH) return;
    const oldest = page[0];
    if (oldest === undefined || oldest.orderKey === beforeOrderKey) return;
    beforeOrderKey = oldest.orderKey;
  }
}

/**
 * The LAST `budget` tool-event rows of one run's log, oldest first.
 *
 * The log is read forward in pages because that is the only direction
 * `ListEventsOptions` offers, but the result is trimmed to `budget` as it goes,
 * so the memory this costs is the page size plus the answer — not the run.
 */
async function tailOfRun(
  tasks: TaskStore,
  runId: string,
  chatId: string,
  budget: number,
): Promise<ToolEventDto[]> {
  const rows: ToolEventDto[] = [];
  let afterSeq = -1;
  for (;;) {
    const batch = await tasks.listEvents(runId, {
      limit: EVENT_BATCH,
      ...(afterSeq < 0 ? {} : { afterSeq }),
    });
    if (batch.length === 0) return rows;
    for (const event of batch) {
      afterSeq = Math.max(afterSeq, event.seq);
      const dto = toolEventDto(event, runId, chatId);
      if (dto !== null) rows.push(dto);
    }
    if (rows.length > budget) rows.splice(0, rows.length - budget);
    if (batch.length < EVENT_BATCH) return rows;
  }
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
