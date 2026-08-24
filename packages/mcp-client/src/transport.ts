import type { Transport } from "@modelcontextprotocol/sdk/shared/transport.js";
import {
  StdioClientTransport,
  getDefaultEnvironment,
} from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import type { McpTransportConfig } from "./config.js";
import { McpError } from "./errors.js";

/** What the factory is asked to build: one server's transport, secrets already injected. */
export interface McpTransportRequest {
  alias: string;
  /** Placeholders already substituted — treat every value as secret material. */
  transport: McpTransportConfig;
}

/**
 * How a session obtains a transport.
 *
 * Injectable because the two production transports are both un-fakeable in a
 * unit test — one spawns a process, one opens a socket — while the semantics
 * worth testing (reconnect, circuit, timeout, cancellation) are transport-
 * agnostic. Tests hand in a factory over `InMemoryTransport`; production uses
 * {@link defaultMcpTransportFactory}.
 */
export type McpTransportFactory = (
  request: McpTransportRequest,
) => Promise<Transport>;

/**
 * The stdio/http factory.
 *
 * stdio inherits {@link getDefaultEnvironment} beneath the config's `env`: an
 * MCP server launched with an empty environment cannot find its own interpreter,
 * and the SDK's default set is the curated, PATH-carrying minimum.
 *
 * Closing a `StdioClientTransport` (SDK 1.30) ends stdin, then escalates to
 * SIGTERM and SIGKILL — so `dispose()` genuinely reaps the child and no extra
 * kill is needed here.
 */
export const defaultMcpTransportFactory: McpTransportFactory = async (
  request,
) => {
  const { alias, transport } = request;
  if (transport.kind === "stdio") {
    return new StdioClientTransport({
      command: transport.command,
      ...(transport.args === undefined ? {} : { args: transport.args }),
      env: { ...getDefaultEnvironment(), ...(transport.env ?? {}) },
    });
  }
  let url: URL;
  try {
    url = new URL(transport.url);
  } catch (err) {
    throw new McpError(
      "mcp_invalid_config",
      `MCP server "${alias}" has an unparseable url.`,
      { details: { alias }, cause: err },
    );
  }
  return new StreamableHTTPClientTransport(url, {
    ...(transport.headers === undefined
      ? {}
      : { requestInit: { headers: transport.headers } }),
  });
};
