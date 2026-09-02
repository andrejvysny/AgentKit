import type { Clock, Logger, SecretStore } from "@agentkit/host";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import {
  ErrorCode,
  McpError as SdkMcpError,
} from "@modelcontextprotocol/sdk/types.js";
import { isServerEnabled, type McpServerConfig } from "./config.js";
import { describeCause, McpError, redactedCause } from "./errors.js";
import {
  buildReverseToolAliases,
  projectMcpCallResult,
  projectMcpTools,
  type McpListedTool,
  type McpToolCallOutcome,
  type McpToolDescriptor,
} from "./projection.js";
import {
  backoffDelayMs,
  delay,
  McpCircuitBreaker,
  resolveMcpResilience,
  withDeadline,
  type McpResilienceOptions,
} from "./resilience.js";
import {
  applySecretsToTransport,
  EMPTY_SECRET_MATERIAL,
  resolveMcpSecrets,
  type McpSecretMaterial,
} from "./secrets.js";
import type { McpTransportFactory } from "./transport.js";

/** Identity we advertise during `initialize`. */
const CLIENT_INFO = { name: "agentkit-mcp-client", version: "0.1.0" };

export interface McpSessionDeps {
  secrets: SecretStore;
  transportFactory: McpTransportFactory;
  clock: Clock;
  logger?: Logger;
}

/**
 * One MCP server connection, with the failure behaviour that makes it usable
 * from a long-lived agent host.
 *
 * The connection is lazy and self-healing. Nothing connects until a tool list or
 * a call needs it; a peer that goes away is noticed through the SDK's `onclose`
 * rather than discovered by a hanging request; and a request that fails because
 * the session died is retried after ONE shared reconnect, not one reconnect per
 * concurrent caller.
 *
 * Every error leaving this class is an {@link McpError} whose code was decided
 * where the cause was known — never by matching text in a message.
 */
export class McpSession {
  readonly alias: string;
  readonly resilience: McpResilienceOptions;

  private readonly circuit: McpCircuitBreaker;
  private readonly reverseAliases: ReadonlyMap<string, string>;

  private client: Client | undefined;
  private transport: Transport | undefined;
  private material: McpSecretMaterial = EMPTY_SECRET_MATERIAL;
  private connecting: Promise<void> | undefined;
  private reconnecting: Promise<void> | undefined;
  /**
   * Bumped on every successful open. A caller that failed on generation N and
   * finds the session already at N+1 has nothing to reconnect — someone else
   * did it. This is what makes concurrent recovery converge on ONE reconnect
   * without depending on the two failures overlapping in time.
   */
  private generation = 0;
  /** Suppresses the peer-closed bookkeeping while WE are the ones closing. */
  private closingDeliberately = false;
  /**
   * Set by {@link close}, cleared when a NEW connect cycle starts.
   *
   * What it buys is the ordering `close()` cannot get any other way: an
   * `openOnce` already past its `transportFactory` call owns a process (or a
   * socket) that `close()` cannot see yet, and installing it after the close
   * has swept leaves it unowned for the rest of the run — the stdio-child leak
   * ADR 0004 says was fixed. So a close marks the session, waits for the open
   * in flight, and that open closes what it built instead of installing it.
   */
  private disposed = false;

  constructor(
    readonly config: McpServerConfig,
    private readonly deps: McpSessionDeps,
  ) {
    this.alias = config.alias;
    this.resilience = resolveMcpResilience(config.resilience);
    this.circuit = new McpCircuitBreaker(
      this.alias,
      this.resilience,
      deps.clock,
    );
    this.reverseAliases = buildReverseToolAliases(config);
  }

  get isConnected(): boolean {
    return this.client !== undefined;
  }

  /** Idempotent. A connected session returns at once; concurrent calls share one attempt. */
  async connect(): Promise<void> {
    if (this.client) return;
    return this.open();
  }

  async listTools(): Promise<McpToolDescriptor[]> {
    const result = await this.withSession("tools/list", (client, signal) =>
      client.listTools(undefined, {
        signal,
        timeout: this.resilience.requestTimeoutMs,
      }),
    );
    return projectMcpTools(
      this.alias,
      this.config.toolAliases,
      result.tools as readonly McpListedTool[],
    );
  }

  /**
   * Call one tool by its EFFECTIVE name (the one the canonical id embeds); the
   * server-side name is recovered from `toolAliases` here, so callers never have
   * to know a rename happened.
   */
  async callTool(
    effectiveToolName: string,
    args: Record<string, unknown> | undefined,
    options?: { signal?: AbortSignal },
  ): Promise<McpToolCallOutcome> {
    const toolName =
      this.reverseAliases.get(effectiveToolName) ?? effectiveToolName;
    const result = await this.withSession(
      `tools/call ${toolName}`,
      (client, signal) =>
        client.callTool(
          {
            name: toolName,
            ...(args === undefined ? {} : { arguments: args }),
          },
          undefined,
          { signal, timeout: this.resilience.requestTimeoutMs },
        ),
      options,
    );
    return projectMcpCallResult(
      this.alias,
      effectiveToolName,
      toolName,
      result,
    );
  }

  /**
   * Close the session. A deliberate shutdown is not a failure, so the circuit is
   * untouched.
   *
   * It also WAITS for a connect that is in flight, and marks the session so that
   * connect closes whatever it opened rather than installing it. Returning
   * before then is how `McpClientManager.dispose()` used to come back while a
   * child process was still being adopted: the manager reported everything
   * closed, and a moment later a `Client` and its stdio child were installed on
   * a session nobody would ever close again.
   *
   * The resolved secret material goes with it. A closed session — a disposed
   * manager, a server removed from the config — has no business still holding
   * live tokens in memory for the lifetime of the process, and `openOnce`
   * re-resolves from the store on every attempt anyway, so nothing needs them
   * to survive. The reconnect path closes and reopens, and no request can be
   * issued in between: `withSession` re-enters `connect()` at the top of every
   * loop iteration.
   */
  async close(): Promise<void> {
    this.disposed = true;
    await this.teardown();
  }

  /**
   * Close what is open, without declaring the session finished.
   *
   * Split from {@link close} for the reconnect path: a reconnect closes in order
   * to open again immediately, and marking the session disposed would make the
   * open it exists to perform refuse itself.
   */
  private async teardown(): Promise<void> {
    this.closingDeliberately = true;
    try {
      // Awaited FIRST: until it settles, the client and transport this is about
      // to read may not exist yet.
      const connecting = this.connecting;
      if (connecting) await swallow(() => connecting);
      const client = this.client;
      const transport = this.transport;
      this.client = undefined;
      this.transport = undefined;
      this.material = EMPTY_SECRET_MATERIAL;
      if (client) await swallow(() => client.close());
      else if (transport) await swallow(() => transport.close());
    } finally {
      this.closingDeliberately = false;
    }
  }

  // --- connection lifecycle -------------------------------------------------

  /** The dedup'd "make me a connection" path. Both connect and reconnect go through it. */
  private open(): Promise<void> {
    const inFlight = this.connecting;
    if (inFlight) return inFlight;
    // A deliberate new cycle supersedes an earlier close: `disposed` only has to
    // beat the open that was ALREADY in flight when the close ran, and that one
    // is the branch above.
    this.disposed = false;
    const started = (async () => {
      this.circuit.assertClosed();
      await this.runConnectCycle();
    })();
    const tracked = started.finally(() => {
      if (this.connecting === tracked) this.connecting = undefined;
    });
    this.connecting = tracked;
    return tracked;
  }

  /**
   * Attempt to connect up to `maxConnectAttempts` times, then open the circuit.
   *
   * A NON-retryable failure (a missing secret, an unparseable url) aborts the
   * cycle without arming the lockout: retrying cannot fix it, and hiding the
   * precise error behind `mcp_circuit_open` for the next five seconds would cost
   * the operator the one message that says what to do.
   */
  private async runConnectCycle(): Promise<void> {
    const { maxConnectAttempts } = this.resilience;
    let last: unknown;
    for (let attempt = 1; attempt <= maxConnectAttempts; attempt += 1) {
      try {
        await this.openOnce();
        this.circuit.recordSuccess();
        return;
      } catch (err) {
        last = err;
        if (err instanceof McpError && !err.retryable) throw err;
        this.deps.logger?.debug("mcp connect attempt failed", {
          alias: this.alias,
          attempt,
          error: this.redact(describeCause(err)),
        });
        if (attempt < maxConnectAttempts) {
          await delay(
            backoffDelayMs(
              attempt,
              this.resilience.connectBackoffBaseMs,
              2,
              this.resilience.connectBackoffMaxMs,
            ),
          );
        }
      }
    }
    this.circuit.recordCycleFailure();
    throw new McpError(
      "mcp_connect_failed",
      `MCP server "${this.alias}" failed to connect after ${maxConnectAttempts} ` +
        `attempt(s): ${this.redact(describeCause(last))}`,
      {
        details: { alias: this.alias, attempts: maxConnectAttempts },
        cause: redactedCause(last, (text) => this.redact(text)),
      },
    );
  }

  private async openOnce(): Promise<void> {
    if (!isServerEnabled(this.config)) {
      throw new McpError(
        "mcp_not_connected",
        `MCP server "${this.alias}" is disabled.`,
        { retryable: false, details: { alias: this.alias } },
      );
    }
    // Re-resolved on every attempt on purpose: a rotated secret should be picked
    // up by a reconnect rather than by a process restart.
    this.material = await resolveMcpSecrets(
      this.alias,
      this.config.secretRefs,
      this.deps.secrets,
    );
    const transportConfig = applySecretsToTransport(
      this.config.transport,
      this.material,
    );

    await withDeadline(
      {
        timeoutMs: this.resilience.connectTimeoutMs,
        timeoutCode: "mcp_connect_failed",
        describe: `Connecting to MCP server "${this.alias}"`,
        redact: (text) => this.redact(text),
      },
      async (signal) => {
        const transport = await this.deps.transportFactory({
          alias: this.alias,
          transport: transportConfig,
        });
        const client = new Client(CLIENT_INFO);
        // `Protocol.connect` overwrites `transport.onclose`, so the peer-closed
        // hook has to hang off the CLIENT, not the transport.
        client.onclose = () => this.handleClosed(client);
        try {
          await client.connect(transport, {
            signal,
            timeout: this.resilience.connectTimeoutMs,
          });
        } catch (err) {
          await swallow(() => transport.close());
          throw err;
        }
        // The two ways this connection is already unwanted, checked at the one
        // moment it is fully built and not yet owned by anything: a `close()`
        // that ran while we were connecting, and a deadline that fired and
        // handed our caller a failure (`withDeadline` races, so it did not wait
        // for us). Installing either would leave a live client — and, over
        // stdio, a live child process — that nothing will ever close.
        if (this.disposed || signal.aborted) {
          await swallow(() => client.close());
          throw new McpError(
            "mcp_not_connected",
            `MCP server "${this.alias}" was closed while connecting.`,
            { retryable: false, details: { alias: this.alias } },
          );
        }
        this.client = client;
        this.transport = transport;
        this.generation += 1;
      },
    );
  }

  /** The peer went away. Drop the session so the next request reconnects instead of hanging. */
  private handleClosed(client: Client): void {
    if (this.closingDeliberately) return;
    if (this.client !== client) return;
    this.client = undefined;
    this.transport = undefined;
    this.deps.logger?.warn("mcp session closed by peer", { alias: this.alias });
  }

  private async reconnectFrom(staleGeneration: number): Promise<void> {
    if (this.client && this.generation > staleGeneration) return;
    const inFlight = this.reconnecting;
    if (inFlight) return inFlight;
    const started = (async () => {
      await this.teardown();
      await this.open();
    })();
    const tracked = started.finally(() => {
      if (this.reconnecting === tracked) this.reconnecting = undefined;
    });
    this.reconnecting = tracked;
    return tracked;
  }

  // --- request path ---------------------------------------------------------

  /**
   * Run one request against a live session, reconnecting and retrying when the
   * failure says the SESSION — not the request — was the problem, and the
   * request cannot already have run server-side. See {@link isAutoRetryable}.
   */
  private async withSession<T>(
    operation: string,
    fn: (client: Client, signal: AbortSignal) => Promise<T>,
    options?: { signal?: AbortSignal },
  ): Promise<T> {
    const { reconnectMaxAttempts } = this.resilience;
    let retries = 0;
    for (;;) {
      await this.connect();
      const client = this.client;
      const generation = this.generation;
      // Captured, not read from `this` in the catch: a CONCURRENT caller's
      // reconnect clears `this.material` between the two, and redacting this
      // request's failure with an empty redactor would publish the very token
      // the message was built from.
      const material = this.material;
      const redact = (text: string): string => material.redact(text);
      if (!client) {
        throw new McpError(
          "mcp_not_connected",
          `MCP server "${this.alias}" has no live session for ${operation}.`,
          { retryable: false },
        );
      }
      try {
        return await withDeadline(
          {
            timeoutMs: this.resilience.requestTimeoutMs,
            ...(options?.signal === undefined
              ? {}
              : { signal: options.signal }),
            timeoutCode: "mcp_request_timeout",
            describe: `${operation} on MCP server "${this.alias}"`,
            redact,
          },
          (signal) => fn(client, signal),
        );
      } catch (err) {
        const failure = this.classify(err, operation, redact);
        if (!this.isAutoRetryable(failure)) throw failure;
        if (retries >= reconnectMaxAttempts) {
          // Nothing was ever retried (`reconnectMaxAttempts: 0`): report the
          // actual cause rather than a wrapper implying recovery was attempted.
          if (retries === 0) throw failure;
          throw new McpError(
            "mcp_reconnect_exhausted",
            `${operation} on MCP server "${this.alias}" failed after ` +
              `${reconnectMaxAttempts} reconnect attempt(s): ` +
              this.redact(failure.message),
            {
              details: { alias: this.alias, operation, lastCode: failure.code },
              // `failure` is ours: its message is already redacted, and its own
              // cause was summarized where it was classified.
              cause: failure,
            },
          );
        }
        retries += 1;
        await delay(
          backoffDelayMs(
            retries,
            this.resilience.connectBackoffBaseMs,
            this.resilience.reconnectBackoffFactor,
            this.resilience.connectBackoffMaxMs,
          ),
        );
        await this.reconnectFrom(generation);
      }
    }
  }

  /**
   * Whether this failure may be REPLAYED after a reconnect.
   *
   * Deliberately narrower than {@link McpError.retryable}, and not the same
   * question. `retryable` is advisory — "could a retry succeed?" — and belongs
   * to the host and the model, who know whether the tool they called was safe
   * to run twice. This gate answers "may WE re-issue it without being asked?",
   * and only a request that never reached the server qualifies:
   * `mcp_not_connected`, where the session was already dead.
   *
   * A request TIMEOUT does not qualify. Our deadline firing says nothing about
   * whether the server ran the tool, so replaying one makes a slow-but-
   * successful write execute again — and the model is still told it failed.
   * `retryTimeouts` opts a server back into the old behaviour, for a host that
   * knows that server's tools are idempotent.
   *
   * Residual ambiguity, stated rather than hidden: a peer that closes the
   * transport MID-CALL also surfaces as `mcp_not_connected`, and that one is
   * only probably unsent. It is retried because a dropped session is the case
   * this whole reconnect path exists for; a host that cannot tolerate even that
   * sets `reconnectMaxAttempts: 0`.
   */
  private isAutoRetryable(failure: McpError): boolean {
    if (failure.code === "mcp_not_connected") return failure.retryable;
    if (failure.code === "mcp_request_timeout") {
      return this.resilience.retryTimeouts;
    }
    return false;
  }

  /**
   * Turn a thrown value into a coded failure.
   *
   * Ours pass through — their code was chosen at the point of cause. The SDK's
   * `McpError` carries a JSON-RPC code, which is a fact rather than a string to
   * parse: `ConnectionClosed` means the session died and a retry is worth it,
   * `RequestTimeout` means the SDK's deadline beat ours, and anything else is
   * the SERVER answering with an error — a real answer, and not retryable.
   */
  private classify(
    err: unknown,
    operation: string,
    redact: (text: string) => string,
  ): McpError {
    if (err instanceof McpError) return err;
    if (err instanceof SdkMcpError) {
      if (err.code === ErrorCode.ConnectionClosed) {
        return new McpError(
          "mcp_not_connected",
          `MCP server "${this.alias}" closed the connection during ${operation}.`,
          { retryable: true, cause: redactedCause(err, redact) },
        );
      }
      if (err.code === ErrorCode.RequestTimeout) {
        return new McpError(
          "mcp_request_timeout",
          `${operation} on MCP server "${this.alias}" timed out.`,
          { cause: redactedCause(err, redact) },
        );
      }
      return new McpError(
        "mcp_remote_error",
        `MCP server "${this.alias}" returned an error for ${operation}: ` +
          redact(err.message),
        {
          details: { alias: this.alias, jsonRpcCode: err.code },
          cause: redactedCause(err, redact),
        },
      );
    }
    return new McpError(
      "mcp_remote_error",
      `${operation} on MCP server "${this.alias}" failed: ` +
        redact(describeCause(err)),
      { details: { alias: this.alias }, cause: redactedCause(err, redact) },
    );
  }

  private redact(text: string): string {
    return this.material.redact(text);
  }
}

/** Close paths must never mask the error that led to the close. */
async function swallow(fn: () => Promise<void>): Promise<void> {
  try {
    await fn();
  } catch {
    // Intentionally ignored.
  }
}
