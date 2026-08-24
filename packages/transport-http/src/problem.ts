/**
 * RFC 7807 `application/problem+json` — the ONE error body this adapter writes.
 *
 * Every non-2xx response on every route goes through here, because a client
 * that has to branch on the shape of an error before it can read the error is a
 * client that will get it wrong on the route nobody tested. `code` is the
 * member it actually branches on: the host's own stable error code
 * (`not_found`, `invalid_task_transition`, …) passed through untouched, with
 * `type`/`title` left for humans.
 *
 * Status is derived from the code rather than chosen per call site. Two call
 * sites mapping `revision_conflict` to two different statuses is exactly the
 * drift this table exists to prevent.
 */
import type { ProblemDetailsDto } from "@agentkit/contracts";
import { AgentKitHostError, type Logger } from "@agentkit/host";

/** `type` URIs are `<prefix><code>`; the code is the documented identity. */
export const PROBLEM_TYPE_PREFIX = "https://agentkit.dev/problems/";

export const PROBLEM_CONTENT_TYPE = "application/problem+json";

/**
 * Host error code → HTTP status.
 *
 * The conflicts are all the same shape — "the record moved since you read it"
 * — and 409 is the only status that says so without implying the client can fix
 * its request and retry verbatim. `executor_not_found` is a deployment fault,
 * not a client one, so it is a 500 despite arriving from a named code.
 */
const STATUS_BY_CODE: Readonly<Record<string, number>> = Object.freeze({
  not_found: 404,
  invalid_task_transition: 409,
  invalid_proposal_transition: 409,
  duplicate_task: 409,
  duplicate_action_id: 409,
  revision_conflict: 409,
  lease_lost: 409,
  seq_conflict: 409,
  unknown_dependency: 409,
  invalid_decision: 400,
  executor_not_found: 500,
});

const TITLE_BY_STATUS: Readonly<Record<number, string>> = Object.freeze({
  400: "Bad Request",
  401: "Unauthorized",
  403: "Forbidden",
  404: "Not Found",
  405: "Method Not Allowed",
  409: "Conflict",
  500: "Internal Server Error",
  501: "Not Implemented",
});

export interface ProblemInit {
  status: number;
  /** The machine-readable code a client branches on. */
  code: string;
  detail?: string;
  /** Overrides the status-derived title. */
  title?: string;
  /** The request path, so a log line pairs the problem with the call. */
  instance?: string;
  /** Extra response headers — `Allow` on a 405, and nothing else so far. */
  headers?: Record<string, string>;
}

/** Build the problem body without serializing it (tests read it directly). */
export function problemDetails(init: ProblemInit): ProblemDetailsDto {
  return {
    type: `${PROBLEM_TYPE_PREFIX}${init.code}`,
    title: init.title ?? TITLE_BY_STATUS[init.status] ?? "Error",
    status: init.status,
    ...(init.detail === undefined ? {} : { detail: init.detail }),
    ...(init.instance === undefined ? {} : { instance: init.instance }),
    code: init.code,
  };
}

export function problemResponse(init: ProblemInit): Response {
  return new Response(JSON.stringify(problemDetails(init)), {
    status: init.status,
    headers: {
      "content-type": PROBLEM_CONTENT_TYPE,
      ...(init.headers ?? {}),
    },
  });
}

/** The 404 every "the id names nothing" path shares. */
export function notFound(detail: string, instance: string): Response {
  return problemResponse({
    status: 404,
    code: "not_found",
    detail,
    instance,
  });
}

/** The 400 body-parse and structural-validation failures share. */
export function badRequest(
  code: string,
  detail: string,
  instance: string,
): Response {
  return problemResponse({ status: 400, code, detail, instance });
}

/**
 * A route the wired host cannot answer — the proposal routes without a
 * `ProposalService`, `listTools` without a tool catalog.
 *
 * 501 rather than 404: the route EXISTS in the contract and another deployment
 * of the same version serves it. Telling a client "no such route" would send it
 * looking for a spelling mistake.
 */
export function notImplemented(detail: string, instance: string): Response {
  return problemResponse({
    status: 501,
    code: "not_implemented",
    detail,
    instance,
  });
}

/**
 * Map a thrown error onto a problem response.
 *
 * A host error is reported with its own `code` and message: those strings are
 * written by this framework, not by a user, so they are safe to publish and are
 * the only thing that makes a 409 actionable. Anything else is a bug in this
 * process — logged in full, answered with a generic detail, because an
 * unexpected exception's message is the one string most likely to carry
 * internals.
 */
export function problemForError(
  err: unknown,
  instance: string,
  logger?: Logger,
): Response {
  if (err instanceof AgentKitHostError) {
    const status = STATUS_BY_CODE[err.code] ?? 500;
    if (status >= 500) {
      logger?.error("rest handler host error", {
        code: err.code,
        instance,
        message: err.message,
      });
    }
    return problemResponse({
      status,
      code: err.code,
      detail: err.message,
      instance,
    });
  }
  logger?.error("rest handler unhandled error", {
    instance,
    message: err instanceof Error ? err.message : String(err),
  });
  return problemResponse({
    status: 500,
    code: "internal_error",
    detail: "The server failed to handle the request.",
    instance,
  });
}
