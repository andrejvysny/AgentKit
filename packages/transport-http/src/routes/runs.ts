/**
 * Runs: read one, stream one, cancel one.
 *
 * A "run" is a `chat.turn` task seen from the outside. A task of any other kind
 * is not addressable here — see {@link chatIdOfTask} for why the absence of a
 * chat is a 404 rather than a partial projection.
 */
import type { RunDto } from "@agentkit/contracts";
import { jsonResponse } from "../http.js";
import { notFound } from "../problem.js";
import { chatIdOfTask, runDto } from "../projections.js";
import { resolveStreamOptions } from "../deps.js";
import { createRunEventStream, resolveStartSeq, SSE_HEADERS } from "../sse.js";
import { pathParam, type RouteContext } from "./context.js";

export async function getRun(ctx: RouteContext): Promise<Response> {
  const runId = pathParam(ctx, "runId");
  const projected = await readRun(ctx, runId);
  if (projected === null)
    return notFound(`Run not found: ${runId}`, ctx.instance);
  return jsonResponse(projected);
}

/**
 * Cancellation is a REQUEST, hence 202: a queued run is settled in the store
 * immediately, but a running one is asked to stop through the queue and only
 * the worker can land it. The `RunDto` returned is re-read after the call, so it
 * reports what actually happened — `cancelled` for the queued case, still
 * `running` for the cooperative one — rather than the optimistic answer.
 */
export async function cancelRun(ctx: RouteContext): Promise<Response> {
  const runId = pathParam(ctx, "runId");
  const before = await readRun(ctx, runId);
  if (before === null) return notFound(`Run not found: ${runId}`, ctx.instance);
  await ctx.deps.tasks.cancelTask(runId);
  const after = await readRun(ctx, runId);
  return jsonResponse(after ?? before, 202);
}

/**
 * Open the run's event stream.
 *
 * The 404 is decided BEFORE the stream is created: once a `text/event-stream`
 * response has been returned there is no status code left to say "no such run",
 * and a client would sit on an empty stream waiting for a run that does not
 * exist. Everything after that point is in {@link createRunEventStream}.
 */
export async function streamRun(ctx: RouteContext): Promise<Response> {
  const runId = pathParam(ctx, "runId");
  const projected = await readRun(ctx, runId);
  if (projected === null)
    return notFound(`Run not found: ${runId}`, ctx.instance);

  const tasks = ctx.deps.store.tasks;
  const startSeq = await resolveStartSeq(
    tasks,
    runId,
    ctx.req.headers.get("last-event-id"),
  );
  const stream = createRunEventStream({
    tasks,
    taskId: runId,
    startSeq,
    options: resolveStreamOptions(ctx.deps.streaming),
    ...(ctx.req.signal === undefined ? {} : { signal: ctx.req.signal }),
    ...(ctx.deps.logger === undefined ? {} : { logger: ctx.deps.logger }),
  });
  return new Response(stream, { status: 200, headers: { ...SSE_HEADERS } });
}

/** The run projection, or null when the id names no run of this contract. */
async function readRun(
  ctx: RouteContext,
  runId: string,
): Promise<RunDto | null> {
  const task = await ctx.deps.store.tasks.getTask(runId);
  if (task === null) return null;
  const chatId = chatIdOfTask(task);
  if (chatId === null) return null;
  return runDto(task, chatId);
}
