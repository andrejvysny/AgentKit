import {
  defaultClock,
  type Clock,
  type Logger,
  type SecretStore,
} from "@agentkit/host";
import { isServerEnabled, type McpServerConfig } from "./config.js";
import { describeCause, McpError } from "./errors.js";
import { normalizeServerAlias, parseMcpCanonicalToolId } from "./identity.js";
import type { McpToolCallOutcome, McpToolDescriptor } from "./projection.js";
import { McpSession, type McpSessionDeps } from "./session.js";
import {
  defaultMcpTransportFactory,
  type McpTransportFactory,
} from "./transport.js";

export interface McpClientManagerDeps {
  secrets: SecretStore;
  logger?: Logger;
  elicitation?: McpSessionDeps["elicitation"];
  onCatalogChanged?: McpSessionDeps["onCatalogChanged"];
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

  constructor(
    private readonly deps: McpClientManagerDeps,
    configs: readonly McpServerConfig[],
  ) {
    this.logger = deps.logger;
    const clock = deps.clock ?? defaultClock;
    const transportFactory =
      deps.transportFactory ?? defaultMcpTransportFactory;
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
          { ...structuredClone(config), alias },
          {
            secrets: deps.secrets,
            elicitation: deps.elicitation,
            onCatalogChanged: deps.onCatalogChanged,
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

  async connect(
    alias: string,
    options?: { signal?: AbortSignal },
  ): Promise<void> {
    await this.session(alias).connect(options);
  }

  /**
   * Connect every enabled server, sequentially isolating failures.
   *
   * Sequential rather than parallel: connecting typically spawns processes, and
   * a host with a dozen stdio servers should not fork a dozen children at once
   * on the first turn of a chat.
   */
  async connectAll(options?: {
    signal?: AbortSignal;
    aliases?: readonly string[];
  }): Promise<McpConnectAllResult> {
    const result: McpConnectAllResult = {
      connected: [],
      failed: [],
      skipped: [],
    };
    for (const session of this.sessions.values()) {
      options?.signal?.throwIfAborted();
      if (options?.aliases && !options.aliases.includes(session.alias))
        continue;
      if (!isServerEnabled(session.config)) {
        result.skipped.push(session.alias);
        continue;
      }
      try {
        await session.connect(options);
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

  async listTools(
    alias: string,
    options?: { signal?: AbortSignal },
  ): Promise<McpToolDescriptor[]> {
    return this.session(alias).listTools(options);
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
    options?: { signal?: AbortSignal; replaySafe?: boolean },
  ): Promise<McpToolCallOutcome> {
    const { serverAlias, effectiveToolName } =
      parseMcpCanonicalToolId(canonicalId);
    return this.session(serverAlias).callTool(effectiveToolName, args, options);
  }

  /** Replace invalidates the old session before exposing new configuration. */
  async replace(config: McpServerConfig): Promise<void> {
    const alias = normalizeServerAlias(config.alias);
    const next = new McpSession(structuredClone(config), {
      elicitation: this.deps.elicitation,
      onCatalogChanged: this.deps.onCatalogChanged,
      secrets: this.deps.secrets,
      clock: this.deps.clock ?? defaultClock,
      transportFactory:
        this.deps.transportFactory ?? defaultMcpTransportFactory,
      logger: this.deps.logger,
    });
    const old = this.sessions.get(alias);
    this.sessions.delete(alias);
    if (old) await old.close();
    this.sessions.set(alias, next);
  }
  async remove(alias: string): Promise<void> {
    const session = this.sessions.get(alias);
    this.sessions.delete(alias);
    if (session) await session.close();
  }
  listResources(alias: string, options?: { signal?: AbortSignal }) {
    return this.session(alias).listResources(options);
  }
  listResourceTemplates(alias: string, options?: { signal?: AbortSignal }) {
    return this.session(alias).listResourceTemplates(options);
  }
  readResource(alias: string, uri: string, options?: { signal?: AbortSignal }) {
    return this.session(alias).readResource(uri, options);
  }
  listPrompts(alias: string, options?: { signal?: AbortSignal }) {
    return this.session(alias).listPrompts(options);
  }
  getPrompt(
    alias: string,
    name: string,
    args?: Record<string, string>,
    options?: { signal?: AbortSignal },
  ) {
    return this.session(alias).getPrompt(name, args, options);
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
