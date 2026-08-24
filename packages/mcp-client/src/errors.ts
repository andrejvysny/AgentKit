/**
 * MCP-layer error vocabulary.
 *
 * Every failure crossing the bridge carries a stable `code` and a `retryable`
 * verdict, because the code above it (the reconnect loop, the tool result the
 * model reads) branches on the *cause*, not on prose. Classification happens at
 * the point where the cause is known — the timeout we armed, the transport's
 * `onclose`, the SDK's own `McpError` — never by matching substrings in a
 * message, which is how retry logic silently rots when a dependency reworded
 * an error string.
 */

export type McpErrorCode =
  /** Connect is under a timed lockout after a full attempt cycle failed. */
  | "mcp_circuit_open"
  /** Every connect attempt in the cycle failed. */
  | "mcp_connect_failed"
  /** Our own request deadline fired before the server answered. */
  | "mcp_request_timeout"
  /** The caller's `AbortSignal` fired (run cancelled), not a fault. */
  | "mcp_request_aborted"
  /** Retryable request failure survived every reconnect+retry attempt. */
  | "mcp_reconnect_exhausted"
  /** No live session: never connected, or the peer closed the transport. */
  | "mcp_not_connected"
  /** The server answered with a JSON-RPC error (or an unreadable result). */
  | "mcp_remote_error"
  /** A `secretRefs` entry resolved to null in the {@link SecretStore}. */
  | "mcp_secret_missing"
  /** Two tools would occupy the same canonical id. Fail-closed, never last-wins. */
  | "mcp_canonical_id_collision"
  /** A server alias that does not match the alias grammar. */
  | "mcp_invalid_alias"
  /** A tool name (or tool alias) that does not match the tool-name grammar. */
  | "mcp_invalid_tool_name"
  /** The server config itself is unusable (duplicate alias, bad url, ...). */
  | "mcp_invalid_config"
  /** A canonical id that is not `mcp.<alias>.<tool>`. */
  | "mcp_invalid_canonical_id";

/**
 * Default retry verdict per code.
 *
 * "Retryable" here means exactly one thing: *reconnecting and re-issuing the
 * request could plausibly succeed*. A circuit that is open is deliberately NOT
 * retryable — the whole point of the lockout is that the caller stops hammering
 * a server that just failed a full attempt cycle.
 */
const RETRYABLE_BY_CODE: Record<McpErrorCode, boolean> = {
  mcp_circuit_open: false,
  mcp_connect_failed: true,
  mcp_request_timeout: true,
  mcp_request_aborted: false,
  mcp_reconnect_exhausted: false,
  mcp_not_connected: true,
  mcp_remote_error: false,
  mcp_secret_missing: false,
  mcp_canonical_id_collision: false,
  mcp_invalid_alias: false,
  mcp_invalid_tool_name: false,
  mcp_invalid_config: false,
  mcp_invalid_canonical_id: false,
};

export interface McpErrorOptions {
  /** Overrides the code's default verdict; use only when the cause narrows it. */
  retryable?: boolean;
  details?: Record<string, unknown>;
  cause?: unknown;
}

/**
 * The only error type this package throws.
 *
 * `message` is always redacted before it reaches here when it was built from
 * config-derived text (a URL, a header, a spawn command): resolved secret values
 * must never travel in an error or a log line. See `secrets.ts`.
 */
export class McpError extends Error {
  readonly code: McpErrorCode;
  readonly retryable: boolean;
  readonly details?: Record<string, unknown>;

  constructor(code: McpErrorCode, message: string, options: McpErrorOptions = {}) {
    super(message, options.cause === undefined ? undefined : { cause: options.cause });
    this.name = "McpError";
    this.code = code;
    this.retryable = options.retryable ?? RETRYABLE_BY_CODE[code];
    if (options.details !== undefined) this.details = options.details;
  }
}

export function isMcpError(err: unknown): err is McpError {
  return err instanceof McpError;
}

/** Best-effort human text for an unknown thrown value. Never trusted for control flow. */
export function describeCause(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  return String(err);
}
