/**
 * Persistent teammates (AGENTKIT-90): agent lifecycle, the configuration
 * ledger, the ONE admission path, task status/history, event replay and
 * run-profile explanation.
 *
 * **All fifteen routes answer 501 without `deps.teammates`** — the same
 * standing as `listTools` without a catalogue: the routes exist in the
 * contract, another deployment of the same version serves them, and a host
 * that runs the teammate services nowhere cannot serve them better by
 * pretending. The port is host-wired; the Rust backend
 * (`rust/crates/host`) is the execution owner behind it.
 *
 * Two conventions make the handlers thin, and both are the port's to keep
 * (see {@link TeammateApiPort}): `null` from a method that names one record
 * is the not-found answer (a 404 here), and a state violation — a stale
 * `expectedRevision`, an archived agent asked to admit — throws the
 * `AgentKitHostError` whose code the dispatcher already maps to 409. What
 * this file owns is the boundary: bounded queries, closed-body validation,
 * and the existence check that must happen BEFORE an SSE response exists.
 *
 * LIST routes answer as the port answers — a scoped host returns the
 * caller's rows, and an agent that is not in the caller's scope is
 * indistinguishable from one that does not exist, which is the contract's
 * own hiding rule. A 404 on a LIST of an unknown-but-forbidden agent would
 * leak which ids exist.
 */
import { jsonResponse, readJsonObject, readPositiveInt } from "../http.js";
import { badRequest, notFound, notImplemented } from "../problem.js";
import { resolveStreamOptions } from "../deps.js";
import {
  createRunEventStream,
  resolveStreamStart,
  SSE_HEADERS,
  STREAM_RESYNC_HEADER,
  type TaskEventSource,
} from "../sse.js";
import {
  validateCreateAgentRequest,
  validateSetAgentModelRequest,
  validateSubmitTeammateTaskRequest,
  validateUpdateAgentRequest,
} from "../validate.js";
import { pathParam, type RouteContext } from "./context.js";

/** The 501 all fifteen share — the same sentence, because it is one fact. */
function noPort(ctx: RouteContext): Response {
  return notImplemented(
    "This deployment does not expose persistent teammates; no TeammateApiPort is wired.",
    ctx.instance,
  );
}

/**
 * A non-negative integer query parameter — `?after=` is a `seq`, and seq 0
 * is a legal cursor, which {@link readPositiveInt} would refuse. Absent is
 * `undefined`; present and unparseable is a 400, for the same reason.
 */
function readSeqParam(
  url: URL,
  name: string,
): { ok: true; value?: number } | { ok: false; message: string } {
  const raw = url.searchParams.get(name);
  if (raw === null) return { ok: true };
  const value = Number(raw);
  if (!Number.isInteger(value) || value < 0) {
    return {
      ok: false,
      message: `Query parameter \`${name}\` must be an integer of 0 or more.`,
    };
  }
  return { ok: true, value };
}

/** The bounded `?limit=` read every listing route shares. */
function readLimit(
  ctx: RouteContext,
): { ok: true; limit?: number } | { ok: false; response: Response } {
  const limit = readPositiveInt(ctx.url, "limit");
  if (!limit.ok) {
    return {
      ok: false,
      response: badRequest("invalid_request", limit.message, ctx.instance),
    };
  }
  return {
    ok: true,
    ...(limit.value === undefined ? {} : { limit: limit.value }),
  };
}

// ---------------------------------------------------------------------------
// Agent lifecycle and the configuration ledger
// ---------------------------------------------------------------------------

/**
 * Mint a persistent agent identity. 201: the identity is new and the
 * revision ledger opens with its `create` row — both inside the host's one
 * transaction, so no caller ever reads an agent without its definition.
 */
export async function createAgent(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateCreateAgentRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const agent = await port.createAgent(validated.value);
  return jsonResponse(agent, 201);
}

export async function listAgents(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const limit = readLimit(ctx);
  if (!limit.ok) return limit.response;
  return jsonResponse(
    await port.listAgents({
      ...(limit.limit === undefined ? {} : { limit: limit.limit }),
    }),
  );
}

export async function getAgent(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const agentId = pathParam(ctx, "agentId");
  const agent = await port.getAgent(agentId);
  if (agent === null)
    return notFound(`Agent not found: ${agentId}`, ctx.instance);
  return jsonResponse(agent);
}

/**
 * Replace the effective definition. A stale `expectedRevision` is the
 * port's `revision_conflict` throw — a 409 that applied nothing — so the
 * caller can re-read and rebase rather than wondering what half of a patch
 * landed.
 */
export async function updateAgent(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const agentId = pathParam(ctx, "agentId");
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateUpdateAgentRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const agent = await port.updateAgent(agentId, validated.value);
  if (agent === null)
    return notFound(`Agent not found: ${agentId}`, ctx.instance);
  return jsonResponse(agent);
}

/** Move the model preference under the SAME identity — a `model_change` revision, never a new agent. */
export async function setAgentModel(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const agentId = pathParam(ctx, "agentId");
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateSetAgentModelRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const agent = await port.setAgentModel(agentId, validated.value);
  if (agent === null)
    return notFound(`Agent not found: ${agentId}`, ctx.instance);
  return jsonResponse(agent);
}

/** Archive: the agent refuses NEW admissions; history stays readable. */
export async function archiveAgent(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const agentId = pathParam(ctx, "agentId");
  const agent = await port.archiveAgent(agentId);
  if (agent === null)
    return notFound(`Agent not found: ${agentId}`, ctx.instance);
  return jsonResponse(agent);
}

/** Restore an archived agent: admissions open again. */
export async function restoreAgent(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const agentId = pathParam(ctx, "agentId");
  const agent = await port.restoreAgent(agentId);
  if (agent === null)
    return notFound(`Agent not found: ${agentId}`, ctx.instance);
  return jsonResponse(agent);
}

/** The append-only configuration ledger, oldest first. */
export async function listAgentRevisions(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const agentId = pathParam(ctx, "agentId");
  const limit = readLimit(ctx);
  if (!limit.ok) return limit.response;
  return jsonResponse(
    await port.listAgentRevisions(agentId, {
      ...(limit.limit === undefined ? {} : { limit: limit.limit }),
    }),
  );
}

// ---------------------------------------------------------------------------
// Admission and task history
// ---------------------------------------------------------------------------

/**
 * The ONE admission path. 201 when this call created the logical task, 200
 * when the body's `admissionKey` replayed one that already exists — the
 * same task either way, and the `replayed` flag in the body is the wire's
 * honest answer to "did I just create anything?".
 */
export async function submitTeammateTask(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const agentId = pathParam(ctx, "agentId");
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateSubmitTeammateTaskRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const answer = await port.submitTeammateTask(agentId, validated.value);
  if (answer === null) {
    return notFound(`Agent not found: ${agentId}`, ctx.instance);
  }
  return jsonResponse(answer, answer.replayed ? 200 : 201);
}

/** The agent's logical tasks, insertion-ordered (history load). */
export async function listAgentTasks(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const agentId = pathParam(ctx, "agentId");
  const limit = readLimit(ctx);
  if (!limit.ok) return limit.response;
  return jsonResponse(
    await port.listAgentTasks(agentId, {
      ...(limit.limit === undefined ? {} : { limit: limit.limit }),
    }),
  );
}

/** One logical task by id — the reconnect/status read. */
export async function getAgentTask(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const taskId = pathParam(ctx, "taskId");
  const task = await port.getAgentTask(taskId);
  if (task === null)
    return notFound(`Agent task not found: ${taskId}`, ctx.instance);
  return jsonResponse(task);
}

/**
 * Request cancellation. 202 like `cancelRun`: cancellation is a REQUEST —
 * a queued task may settle at once, but a running one lands it through the
 * lease heartbeat, and only the worker knows when. The returned task is
 * what the host recorded, `cancelRequested` true.
 */
export async function cancelAgentTask(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const taskId = pathParam(ctx, "taskId");
  const task = await port.cancelAgentTask(taskId);
  if (task === null)
    return notFound(`Agent task not found: ${taskId}`, ctx.instance);
  return jsonResponse(task, 202);
}

/**
 * One bounded page of the task's durable event log. The existence check
 * happens FIRST: a page of an unknown task would be an empty page, and an
 * empty page cannot 404 — the caller would poll a task that never existed.
 */
export async function listAgentTaskEvents(
  ctx: RouteContext,
): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const taskId = pathParam(ctx, "taskId");
  if ((await port.taskStatus(taskId)) === null) {
    return notFound(`Agent task not found: ${taskId}`, ctx.instance);
  }
  const after = readSeqParam(ctx.url, "after");
  if (!after.ok) {
    return badRequest("invalid_request", after.message, ctx.instance);
  }
  const limit = readLimit(ctx);
  if (!limit.ok) return limit.response;
  const page = await port.listAgentTaskEvents(taskId, {
    ...(after.value === undefined ? {} : { after: after.value }),
    ...(limit.limit === undefined ? {} : { limit: limit.limit }),
  });
  return jsonResponse(page);
}

/**
 * Open the task's SSE stream — the same durable log {@link
 * listAgentTaskEvents} pages, replayed and then followed by the one
 * seq-cursor pump `streamRun` uses.
 *
 * The 404 is decided BEFORE the stream exists, for the run-stream reason
 * verbatim: once a `text/event-stream` has been returned there is no
 * status left to say "no such task". The pump closes on the port's
 * terminal status, so a task that will never speak again ends its stream
 * instead of polling one forever.
 */
export async function streamAgentTask(ctx: RouteContext): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const taskId = pathParam(ctx, "taskId");
  if ((await port.taskStatus(taskId)) === null) {
    return notFound(`Agent task not found: ${taskId}`, ctx.instance);
  }
  // Resolved ONCE and shared with the stream, the `streamRun` rule: the
  // resume scan pages at the size the stream reads at.
  const options = resolveStreamOptions(ctx.deps.streaming);
  const source: TaskEventSource = {
    getTask: async (id: string) => {
      const status = await port.taskStatus(id);
      return status === null ? null : { status };
    },
    listEvents: (id: string, opts?: { afterSeq?: number; limit?: number }) =>
      port.events(id, opts ?? {}),
  };
  const start = await resolveStreamStart(
    source,
    taskId,
    ctx.req.headers.get("last-event-id"),
    options.readBatchSize,
  );
  const stream = createRunEventStream({
    tasks: source,
    taskId,
    startSeq: start.startSeq,
    ...(start.resyncReason === null
      ? {}
      : { resyncReason: start.resyncReason }),
    options,
    ...(ctx.req.signal === undefined ? {} : { signal: ctx.req.signal }),
    ...(ctx.deps.logger === undefined ? {} : { logger: ctx.deps.logger }),
  });
  return new Response(stream, {
    status: 200,
    headers: {
      ...SSE_HEADERS,
      // The replay is announced in the header AND the first frame, so a
      // client can tell a fresh stream from a full replay either way.
      ...(start.resyncReason === null
        ? {}
        : { [STREAM_RESYNC_HEADER]: start.resyncReason }),
    },
  });
}

/**
 * Explain the sealed effective run profile the admission pinned
 * (AGENTKIT-89). Absent `digest`/`profile` is the honest answer for an
 * admission that pinned none — unknown, not an empty profile.
 */
export async function getAgentTaskRunProfile(
  ctx: RouteContext,
): Promise<Response> {
  const port = ctx.deps.teammates;
  if (port === undefined) return noPort(ctx);
  const taskId = pathParam(ctx, "taskId");
  const explanation = await port.getAgentTaskRunProfile(taskId);
  if (explanation === null) {
    return notFound(`Agent task not found: ${taskId}`, ctx.instance);
  }
  return jsonResponse(explanation);
}
