import { defaultClock, type Clock, type Logger, type SecretStore } from "@agentkit/host";
import { isServerEnabled, type McpServerConfig } from "./config.js";
import { describeCause, McpError } from "./errors.js";
import {
  normalizeServerAlias,
  parseMcpCanonicalToolId,
} from "./identity.js";
import type { McpToolCallOutcome, McpToolDescriptor } from "./projection.js";
import { McpSession } from "./session.js";
import {
  defaultMcpTransportFactory,
  type McpTransportFactory,
} from "./transport.js";

export interface McpClientManagerDeps {
  secrets: SecretStore;
  logger?: Logger;
  clock?: Clock;
  /** Defaults to {@link defaultMcpTransportFactory}; tests inject in-memory pairs. */
  transportFactory?: McpTransportFactory;
}

/** Per-server outcome of {@link McpClientManager.connectAll}. */
export interface McpConnectAllResult {
  connected: string[];
  /** Aliases that failed, with the coded reason. Never thrown — see below. */
  failed: { alias: string; error: McpError }[];
  /** Aliases skipped because `enabled === false`. */
  skipped: string[];
}

/**
 * The set of MCP servers a host has configured, and the only thing that talks
 * to them.
 *
 * The manager owns two things sessions cannot own individually: the static
 * config list (so an alias resolves to a server without a registry lookup) and
 * canonical-id ROUTING (so a tool call recorded as `mcp.gh.search` finds its way
 * back to the `gh` session, even across a restart).
 *
 * Connection failures are isolated per server. One misconfigured MCP server must
 * not cost a chat every OTHER server's tools, so {@link connectAll} reports
 * failures instead of throwing them — the caller decides whether a partial tool
 * set is acceptable, and it almost always is.
 */
export class McpClientManager {
  private readonly sessions = new Map<string, McpSession>();
  private readonly logger: Logger | undefined;

  constructor(deps: McpClientManagerDeps, configs: readonly McpServerConfig[]) {
    this.logger = deps.logger;
    const clock = deps.clock ?? defaultClock;
    const transportFactory = deps.transportFactory ?? defaultMcpTransportFactory;
    for (const config of configs) {
      // Validate eagerly: a bad alias is a wiring bug, and discovering it on the
      // first tool call of the first chat is strictly worse than at construction.
      const alias = normalizeServerAlias(config.alias);
      if (this.sessions.has(alias)) {
        throw new McpError(
          "mcp_invalid_config",
          `Duplicate MCP server alias "${alias}".`,
          { details: { alias } },
        );
      }
      this.sessions.set(
        alias,
        new McpSession(
          { ...config, alias },
          {
            secrets: deps.secrets,
            transportFactory,
            clock,
            ...(deps.logger === undefined ? {} : { logger: deps.logger }),
          },
        ),
      );
    }
  }

  /** Every configured alias, enabled or not. */
  aliases(): string[] {
    return [...this.sessions.keys()];
  }

  /** Aliases with a live session right now. */
  connectedAliases(): string[] {
    return [...this.sessions.values()]
      .filter((session) => session.isConnected)
      .map((session) => session.alias);
  }

  isConnected(alias: string): boolean {
    return this.session(alias).isConnected;
  }

  async connect(alias: string): Promise<void> {
    await this.session(alias).connect();
  }

  /**
   * Connect every enabled server, sequentially isolating failures.
   *
   * Sequential rather than parallel: connecting typically spawns processes, and
   * a host with a dozen stdio servers should not fork a dozen children at once
   * on the first turn of a chat.
   */
  async connectAll(): Promise<McpConnectAllResult> {
    const result: McpConnectAllResult = {
      connected: [],
      failed: [],
      skipped: [],
    };
    for (const session of this.sessions.values()) {
      if (!isServerEnabled(session.config)) {
        result.skipped.push(session.alias);
        continue;
      }
      try {
        await session.connect();
        result.connected.push(session.alias);
      } catch (err) {
        const failure = this.asMcpError(session.alias, err);
        result.failed.push({ alias: session.alias, error: failure });
        this.logger?.warn("mcp server unavailable", {
          alias: session.alias,
          code: failure.code,
          error: failure.message,
        });
      }
    }
    return result;
  }

  async listTools(alias: string): Promise<McpToolDescriptor[]> {
    return this.session(alias).listTools();
  }

  /**
   * Call a tool by its canonical id.
   *
   * The id IS the route: it is parsed back into `(serverAlias, effectiveToolName)`
   * here, which is why the canonical grammar is enforced at both ends.
   */
  async callTool(
    canonicalId: string,
    args: Record<string, unknown> | undefined,
    options?: { signal?: AbortSignal },
  ): Promise<McpToolCallOutcome> {
    const { serverAlias, effectiveToolName } =
      parseMcpCanonicalToolId(canonicalId);
    return this.session(serverAlias).callTool(effectiveToolName, args, options);
  }

  async close(alias: string): Promise<void> {
    await this.session(alias).close();
  }

  /**
   * Close every session.
   *
   * Closing a stdio transport (SDK 1.30) ends the child's stdin and escalates to
   * SIGTERM then SIGKILL, so this genuinely reaps the processes rather than
   * leaking one per configured server.
   */
  async dispose(): Promise<void> {
    for (const session of this.sessions.values()) {
      try {
        await session.close();
      } catch (err) {
        this.logger?.warn("mcp session close failed", {
          alias: session.alias,
          error: describeCause(err),
        });
      }
    }
  }

  private session(alias: string): McpSession {
    const session = this.sessions.get(alias);
    if (!session) {
      throw new McpError(
        "mcp_not_connected",
        `No MCP server is configured under alias "${alias}".`,
        { retryable: false, details: { alias } },
      );
    }
    return session;
  }

  private asMcpError(alias: string, err: unknown): McpError {
    if (err instanceof McpError) return err;
    return new McpError(
      "mcp_connect_failed",
      `MCP server "${alias}" failed to connect: ${describeCause(err)}`,
      { details: { alias }, cause: err },
    );
  }
}
