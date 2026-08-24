import type { Clock } from "@agentkit/host";
import { McpError, redactedCause } from "./errors.js";

/**
 * Per-server failure policy.
 *
 * The two halves answer different questions. *Connect* resilience decides how
 * hard to try to get a session at all, and how long to stop trying after that
 * fails — a server whose command does not exist must not be re-spawned on every
 * turn. *Request* resilience decides what happens when a session that WAS
 * healthy drops mid-call: reconnect once or twice and re-issue, because the
 * common case is a server that restarted, not a server that is gone. That
 * replay is deliberately limited to failures the request did not survive — a
 * `tools/call` is not assumed idempotent. See `McpSession.isAutoRetryable`.
 */
export interface McpResilienceOptions {
  /** Deadline for one `tools/list` or `tools/call`. */
  requestTimeoutMs: number;
  /** Deadline for one connect attempt (transport open + `initialize`). */
  connectTimeoutMs: number;
  /** Attempts in ONE connect cycle before the circuit opens. */
  maxConnectAttempts: number;
  /** First backoff step; doubles per attempt, capped by `connectBackoffMaxMs`. */
  connectBackoffBaseMs: number;
  connectBackoffMaxMs: number;
  /** How long connect stays locked out after a cycle failed. */
  circuitOpenMs: number;
  /** Reconnect+retry rounds for ONE retryable request failure. */
  reconnectMaxAttempts: number;
  reconnectBackoffFactor: number;
  /**
   * Whether a REQUEST TIMEOUT may be replayed. Off by default.
   *
   * A timeout is an ambiguous delivery: our deadline fired without an answer,
   * which says nothing about whether the server ran the tool. Auto-retrying one
   * means a slow-but-successful write executes again — twice, or
   * `reconnectMaxAttempts + 1` times — while the model is eventually told the
   * call failed. Turn this on only for a server whose tools are all idempotent.
   */
  retryTimeouts: boolean;
}

export const DEFAULT_MCP_RESILIENCE: McpResilienceOptions = {
  requestTimeoutMs: 5_000,
  connectTimeoutMs: 5_000,
  maxConnectAttempts: 3,
  connectBackoffBaseMs: 250,
  connectBackoffMaxMs: 2_000,
  circuitOpenMs: 5_000,
  reconnectMaxAttempts: 2,
  reconnectBackoffFactor: 2,
  retryTimeouts: false,
};

export function resolveMcpResilience(
  overrides?: Partial<McpResilienceOptions>,
): McpResilienceOptions {
  return { ...DEFAULT_MCP_RESILIENCE, ...(overrides ?? {}) };
}

/** `min(max, base * factor^(attempt-1))`. Deliberately jitter-free: one client, one server. */
export function backoffDelayMs(
  attempt: number,
  base: number,
  factor: number,
  max: number,
): number {
  if (attempt < 1) return 0;
  return Math.min(max, Math.round(base * factor ** (attempt - 1)));
}

export function delay(ms: number): Promise<void> {
  if (ms <= 0) return Promise.resolve();
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * A HARD TIMED LOCKOUT, not a half-open probe.
 *
 * After one full connect cycle fails, every `connect()` is refused outright
 * until the window elapses; the first call after it runs a fresh full cycle. No
 * half-open state on purpose: an MCP connect is expensive (a process spawn, an
 * `initialize` round trip), so letting one caller "test the water" mid-window
 * just reintroduces the storm the lockout exists to stop, one caller at a time.
 */
export class McpCircuitBreaker {
  private openedAtMs: number | null = null;

  constructor(
    private readonly alias: string,
    private readonly options: McpResilienceOptions,
    private readonly clock: Clock,
  ) {}

  /** Throws `mcp_circuit_open` while locked out; silently re-arms once expired. */
  assertClosed(): void {
    if (this.openedAtMs === null) return;
    const elapsed = this.clock.now().getTime() - this.openedAtMs;
    if (elapsed >= this.options.circuitOpenMs) {
      this.openedAtMs = null;
      return;
    }
    const remaining = this.options.circuitOpenMs - elapsed;
    throw new McpError(
      "mcp_circuit_open",
      `MCP server "${this.alias}" is in a connect lockout for another ${remaining}ms.`,
      { details: { alias: this.alias, remainingMs: remaining } },
    );
  }

  /** A whole cycle exhausted its attempts. Start the lockout. */
  recordCycleFailure(): void {
    this.openedAtMs = this.clock.now().getTime();
  }

  recordSuccess(): void {
    this.openedAtMs = null;
  }

  get isOpen(): boolean {
    return this.openedAtMs !== null;
  }
}

export interface DeadlineParams {
  timeoutMs: number;
  /** The caller's cancellation (a run being stopped), if any. */
  signal?: AbortSignal;
  /** Code to raise when OUR timer fires — never inferred from the error text. */
  timeoutCode: "mcp_request_timeout" | "mcp_connect_failed";
  /** Phrase naming the operation, already redacted. */
  describe: string;
  /**
   * The caller's redactor. Required, not optional: whatever `fn` throws came
   * from a transport carrying resolved secrets, and it is attached as the
   * failure's `cause`. See {@link redactedCause}.
   */
  redact(text: string): string;
}

/**
 * Run `fn` under a deadline linked to the caller's signal.
 *
 * The classification comes from state WE own — `timedOut` was set by our own
 * timer, `signal.aborted` by our own caller — so the resulting code is right
 * even when the underlying SDK rejects with something unrecognisable.
 */
export async function withDeadline<T>(
  params: DeadlineParams,
  fn: (signal: AbortSignal) => Promise<T>,
): Promise<T> {
  const controller = new AbortController();
  let timedOut = false;
  const onCallerAbort = (): void => controller.abort();
  if (params.signal) {
    if (params.signal.aborted) {
      throw new McpError(
        "mcp_request_aborted",
        `${params.describe} was cancelled before it started.`,
      );
    }
    params.signal.addEventListener("abort", onCallerAbort, { once: true });
  }
  const timer = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, params.timeoutMs);
  try {
    return await fn(controller.signal);
  } catch (err) {
    if (timedOut) {
      throw new McpError(
        params.timeoutCode,
        `${params.describe} timed out after ${params.timeoutMs}ms.`,
        { cause: redactedCause(err, params.redact) },
      );
    }
    if (params.signal?.aborted) {
      throw new McpError(
        "mcp_request_aborted",
        `${params.describe} was cancelled.`,
        { cause: redactedCause(err, params.redact) },
      );
    }
    throw err;
  } finally {
    clearTimeout(timer);
    params.signal?.removeEventListener("abort", onCallerAbort);
  }
}
