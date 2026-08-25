/**
 * What a failed attempt means for the queue: try again, give up, or record that
 * someone asked it to stop.
 *
 * THE REGRESSION THIS FIXES: task-system's `toTaskError` stamped
 * `retryable: !aborted` on EVERY failure. A 401 from the provider, a malformed
 * request, a bug in the executor — all of it came back "retryable", so the
 * runtime re-ran it until the attempt budget burned out. Retrying a request the
 * server will refuse identically is not resilience; it multiplies the damage,
 * delays the real error reaching the user, and turns one bad key into N provider
 * calls per run.
 *
 * So the default here is the opposite: an error nobody recognises is TERMINAL.
 * Retrying blind is a decision, and it should have to be justified by evidence
 * (a network error code, a 5xx/429, an explicit `retryable: true`) rather than
 * being what happens when the classifier has no idea what it is looking at. A
 * host that knows better about its own failures says so by setting `retryable`.
 */

export type ExecutionErrorKind = "transient" | "terminal" | "cancelled";

export interface ExecutionErrorClassification {
  kind: ExecutionErrorKind;
  /**
   * Stable, machine-readable justification (`"network:ECONNRESET"`,
   * `"http_5xx"`, `"unclassified"`). Recorded on the attempt and, for a poison
   * run, on the dead-letter row — matching on `code`/`reason` is what keeps
   * retry policy from rotting the way message-matching does.
   */
  reason: string;
}

/**
 * `@agentkit/host` error codes with a settled retry meaning.
 *
 * `lease_lost` is transient: the work itself never got a verdict — a slow
 * heartbeat or a takeover ended the attempt, and a fresh attempt under a fresh
 * lease is exactly right.
 *
 * `seq_conflict` is terminal, and deliberately so. A duplicate or gapped `seq`
 * means the writer's numbering disagrees with the log — a logic bug in the
 * caller. Retrying reproduces it, with a second helping of half-written events.
 *
 * `executor_not_found`, `duplicate_task`, `invalid_task_payload` and
 * `unknown_dependency` are the wiring failures: a kind nobody registered, an id
 * already taken, a payload the executor cannot read, an edge pointing at a task
 * that does not exist. All four reproduce identically on every attempt — a
 * dependency id the store did not have when the submit ran will not appear on
 * the retry — and none of them is poison: the runner fails the task without
 * dead-lettering it, because a clean diagnosis is not a queue that needs
 * protecting.
 *
 * `invalid_fork_point` joins them: a message that is not on a chat's active path
 * does not wander onto it because an attempt was retried, and the caller named
 * the wrong point either way.
 */
const HOST_CODE_KINDS: Readonly<Record<string, ExecutionErrorKind>> =
  Object.freeze({
    lease_lost: "transient",
    seq_conflict: "terminal",
    invalid_task_transition: "terminal",
    invalid_proposal_transition: "terminal",
    duplicate_action_id: "terminal",
    duplicate_task: "terminal",
    executor_not_found: "terminal",
    invalid_task_payload: "terminal",
    unknown_dependency: "terminal",
    revision_conflict: "terminal",
    not_found: "terminal",
    invalid_fork_point: "terminal",
  });

/** Socket-level failures: the request never got an answer, so ask again. */
const NETWORK_MARKERS = [
  "ECONNRESET",
  "ECONNREFUSED",
  "ECONNABORTED",
  "ETIMEDOUT",
  "EPIPE",
  "EHOSTUNREACH",
  "ENETUNREACH",
  "EAI_AGAIN",
] as const;

/** Lower-cased message fragments that mean the same thing as the codes above. */
const NETWORK_MESSAGE_MARKERS = [
  "fetch failed",
  "socket",
  "network error",
  "connection reset",
  "connection refused",
  "timed out",
] as const;

/** Lower-cased fragments that mean "this request is wrong / not allowed". */
const REJECTION_MESSAGE_MARKERS = [
  "unauthorized",
  "invalid",
  "forbidden",
  "authentication",
  "permission denied",
  "not permitted",
  "bad request",
  "unprocessable",
] as const;

/**
 * Classify a thrown value from {@link TaskWorker.execute}.
 *
 * Checked in decreasing order of confidence: cancellation, then structured
 * signals the thrower actually declared (host error `code`, an explicit
 * `retryable`, a numeric HTTP status), and only then string heuristics on the
 * message. Anything left over is terminal.
 */
export function classifyExecutionError(
  err: unknown,
): ExecutionErrorClassification {
  if (isAbortError(err)) return { kind: "cancelled", reason: "aborted" };

  const record = asRecord(err);
  const message = errorMessage(err);

  const code =
    typeof record?.["code"] === "string" ? record["code"] : undefined;
  if (code !== undefined) {
    const kind = HOST_CODE_KINDS[code];
    if (kind !== undefined) return { kind, reason: code };
  }

  // The thrower's own verdict beats every heuristic below it: a host that knows
  // its provider's failure modes is a better classifier than string matching.
  const retryable = record?.["retryable"];
  if (retryable === true)
    return { kind: "transient", reason: "retryable_flag" };
  if (retryable === false) {
    return { kind: "terminal", reason: "non_retryable_flag" };
  }

  const status = httpStatus(record);
  if (status !== undefined) {
    if (status === 429) return { kind: "transient", reason: "http_429" };
    if (status >= 500) return { kind: "transient", reason: `http_${status}` };
    if (status >= 400) return { kind: "terminal", reason: `http_${status}` };
  }

  const marker = NETWORK_MARKERS.find(
    (candidate) => code === candidate || message.includes(candidate),
  );
  if (marker !== undefined) {
    return { kind: "transient", reason: `network:${marker}` };
  }

  const lowered = message.toLowerCase();
  if (NETWORK_MESSAGE_MARKERS.some((needle) => lowered.includes(needle))) {
    return { kind: "transient", reason: "network" };
  }
  if (/\b5\d{2}\b/.test(message))
    return { kind: "transient", reason: "http_5xx" };
  if (/\b429\b/.test(message)) return { kind: "transient", reason: "http_429" };
  if (/\b(400|401|403|404|422)\b/.test(message)) {
    return { kind: "terminal", reason: "http_rejected" };
  }
  if (REJECTION_MESSAGE_MARKERS.some((needle) => lowered.includes(needle))) {
    return { kind: "terminal", reason: "rejected" };
  }

  // The whole point: unknown means terminal. See the module doc.
  return { kind: "terminal", reason: "unclassified" };
}

/**
 * Every shape an abort arrives in: `AbortController.abort()` rejects with a
 * `DOMException` named `AbortError`, `AbortSignal.throwIfAborted()` throws the
 * signal's `reason`, and hand-rolled aborts are plain `Error`s with the name
 * set. All three say "someone asked for this to stop", never "this failed".
 */
function isAbortError(err: unknown): boolean {
  const record = asRecord(err);
  if (record === undefined) return false;
  if (record["name"] === "AbortError") return true;
  if (record["code"] === "ABORT_ERR") return true;
  // One level of `cause`: a worker that wraps its abort still means abort.
  const cause = asRecord(record["cause"]);
  return cause?.["name"] === "AbortError";
}

function httpStatus(
  record: Record<string, unknown> | undefined,
): number | undefined {
  if (record === undefined) return undefined;
  for (const key of ["status", "statusCode", "httpStatus"]) {
    const value = record[key];
    if (typeof value === "number" && Number.isFinite(value)) return value;
  }
  const response = asRecord(record["response"]);
  const nested = response?.["status"];
  return typeof nested === "number" ? nested : undefined;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : undefined;
}

/** The message to match on, for anything throwable — including non-Errors. */
export function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  const record = asRecord(err);
  const message = record?.["message"];
  if (typeof message === "string") return message;
  return String(err);
}
