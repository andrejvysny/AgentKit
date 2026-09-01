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
import {
  AgentKitHostError,
  type HostErrorCode,
  type Logger,
} from "@agentkit/host";

/** `type` URIs are `<prefix><code>`; the code is the documented identity. */
export const PROBLEM_TYPE_PREFIX = "https://agentkit.dev/problems/";

export const PROBLEM_CONTENT_TYPE = "application/problem+json";

/**
 * Every code in the closed {@link HostErrorCode} union → HTTP status.
 *
 * `satisfies Record<HostErrorCode, number>` makes this exhaustive at compile
 * time: a code added to {@link HOST_ERROR_CODES} without a status here fails
 * `bun run typecheck`, rather than silently falling back to 500 in
 * production. The conflicts are all the same shape — "the record moved since
 * you read it" — and 409 is the only status that says so without implying the
 * client can fix its request and retry verbatim. `executor_not_found` is a
 * deployment fault, not a client one, so it is a 500 despite arriving from a
 * named code.
 *
 * `invalid_fork_point` is a 400 and not a 409: nothing moved and nothing
 * conflicts — the client named a message that is not a place this conversation
 * can be forked from, and the fix is a different `fromMessageId`, which is the
 * definition of a bad request.
 *
 * `usage_denied` is a 429 and not a 403: the request was well-formed and the
 * caller was entitled to make it — a budget said not now. 429 is the one status
 * that means "ask again later", which is what
 * `UsageAuthorizationDecision.retryAfterMs` is for; a 403 would tell a client to
 * stop asking about a quota that refills.
 *
 * `chat_busy` joins the 409s and belongs there for the same reason they do:
 * nothing about the request is wrong, the resource is in a state that forbids
 * it, and the state changes on its own when the run finishes. `invalid_import`
 * is a 400 for the mirror-image reason — the payload itself is malformed, and
 * the same bytes will never be accepted however long the client waits.
 */
const STATUS_BY_HOST_CODE = {
  not_found: 404,
  invalid_fork_point: 400,
  invalid_import: 400,
  chat_busy: 409,
  invalid_task_transition: 409,
  invalid_proposal_transition: 409,
  duplicate_task: 409,
  duplicate_action_id: 409,
  revision_conflict: 409,
  lease_lost: 409,
  seq_conflict: 409,
  unknown_dependency: 409,
  usage_denied: 429,
  executor_not_found: 500,
} satisfies Record<HostErrorCode, number>;

/**
 * Host error code → HTTP status, including codes outside the closed
 * {@link HostErrorCode} union.
 *
 * `@agentkit/host` also throws `AgentKitHostError` directly (not through a
 * named subclass) with situational codes that are not part of that stable
 * vocabulary — `invalid_decision` is the one this table intentionally reports
 * as a client error rather than the generic 500 fallback below; the rest
 * (`no_model`, `task_not_executable`, …) fall back same as any unrecognized
 * code, unchanged from before this table was split.
 */
const STATUS_BY_CODE: Readonly<Record<string, number>> = Object.freeze({
  ...STATUS_BY_HOST_CODE,
  invalid_decision: 400,
});

const TITLE_BY_STATUS: Readonly<Record<number, string>> = Object.freeze({
  400: "Bad Request",
  401: "Unauthorized",
  403: "Forbidden",
  404: "Not Found",
  405: "Method Not Allowed",
  409: "Conflict",
  429: "Too Many Requests",
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
 * The 403 an {@link AuthorizationPort} refusal becomes.
 *
 * A transport-level code, like `not_implemented` and `method_not_allowed`: the
 * decision is made here, by this adapter, before any host call — no
 * `AgentKitHostError` was thrown and none should be invented to carry it. The
 * port's `reason` is passed through as the `detail` because the port documents
 * it as something to say out loud; a host that does not want its reasons
 * published leaves `reason` off, and the generic detail below is what a client
 * sees.
 *
 * 403 and not 404: hiding a resource's existence from someone who may not touch
 * it is a decision only the host can make, and it makes it by returning its own
 * `Response` from `authenticate` (or by refusing with no reason). An adapter
 * that silently downgraded every denial to a 404 would take that choice away
 * and make a real missing resource indistinguishable from a permissions bug.
 */
export function forbidden(detail: string, instance: string): Response {
  return problemResponse({ status: 403, code: "forbidden", detail, instance });
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
