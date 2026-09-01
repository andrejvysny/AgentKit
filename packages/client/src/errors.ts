/**
 * The one exception every non-2xx answer becomes.
 *
 * The server writes exactly one error shape — RFC 7807 `application/problem+json`,
 * see `packages/transport-http/src/problem.ts` — and the member a client branches
 * on is `code`, not `status`: two different 409s (`duplicate_alias`,
 * `revision_conflict`) need two different recoveries, and the status cannot tell
 * them apart. So `code` is lifted onto the error object rather than left for the
 * caller to dig out of `problem`.
 *
 * A NON-PROBLEM body still becomes this error rather than something else. A
 * reverse proxy's HTML 502, a gateway's plain-text 429, a body that was cut off
 * mid-JSON — none of them are the contract, all of them are "the call failed",
 * and a client that had to distinguish "the server said no" from "something
 * between us said no" before it could show a message would get it wrong in the
 * one place it matters. The fallback code is `http_<status>`, which is
 * recognisably not a contract code, and the raw body is kept on `problem` so a
 * log line can still carry what actually arrived.
 */
import type { ProblemDetailsDto } from "@agentkit/contracts";

export class AgentKitClientError extends Error {
  /** HTTP status of the response that failed. */
  readonly status: number;
  /**
   * The stable machine-readable code — `not_found`, `duplicate_provider`,
   * `idempotency_key_required`, … — or `http_<status>` when the body was not
   * problem+json.
   */
  readonly code: string;
  /** The problem's human-readable detail, when it carried one. */
  readonly detail?: string;
  /** The parsed body verbatim, or the raw text when it did not parse. */
  readonly problem: unknown;

  constructor(init: {
    status: number;
    code: string;
    detail?: string;
    problem: unknown;
    message: string;
  }) {
    super(init.message);
    this.name = "AgentKitClientError";
    this.status = init.status;
    this.code = init.code;
    if (init.detail !== undefined) this.detail = init.detail;
    this.problem = init.problem;
  }
}

/** True for a value this package threw, across realms and bundles. */
export function isAgentKitClientError(
  value: unknown,
): value is AgentKitClientError {
  return value instanceof AgentKitClientError;
}

/**
 * Build the error for a failed response, reading its body once.
 *
 * Body reading is best-effort by design: a 502 from a proxy that closed the
 * connection mid-body must still produce an error a caller can act on, so a
 * throw here degrades to "no body" rather than replacing the real failure with
 * a parse failure nobody can trace back.
 */
export async function errorForResponse(
  response: Response,
  method: string,
  url: string,
): Promise<AgentKitClientError> {
  let raw = "";
  try {
    raw = await response.text();
  } catch {
    raw = "";
  }

  let parsed: unknown;
  try {
    parsed = raw === "" ? undefined : JSON.parse(raw);
  } catch {
    parsed = undefined;
  }

  const problem = asProblem(parsed);
  const code = problem?.code ?? `http_${response.status}`;
  const detail = problem?.detail;
  const title = problem?.title ?? response.statusText;
  const message =
    `${method} ${url} failed: ${response.status} ${title || "Error"} [${code}]` +
    (detail === undefined ? "" : ` — ${detail}`);

  return new AgentKitClientError({
    status: response.status,
    code,
    ...(detail === undefined ? {} : { detail }),
    // The parsed body when there was one, otherwise the raw text — never
    // `undefined`, so a log line always has the evidence.
    problem: parsed ?? raw,
    message,
  });
}

/** The problem members this client reads, when the body actually carries them. */
function asProblem(value: unknown): Partial<ProblemDetailsDto> | null {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  const body = value as Record<string, unknown>;
  const out: Partial<ProblemDetailsDto> = {};
  if (typeof body["code"] === "string") out.code = body["code"];
  if (typeof body["detail"] === "string") out.detail = body["detail"];
  if (typeof body["title"] === "string") out.title = body["title"];
  return out;
}
