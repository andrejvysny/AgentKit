import type { McpResilienceOptions } from "./resilience.js";

/** Spawn an MCP server as a child process and speak JSON-RPC over its stdio. */
export interface McpStdioTransportConfig {
  kind: "stdio";
  command: string;
  args?: string[];
  /**
   * Extra environment for the child. Values may contain `${placeholder}` tokens
   * that {@link McpServerConfig.secretRefs} resolves at connect time.
   */
  env?: Record<string, string>;
}

/** Talk to a remote MCP server over Streamable HTTP. */
export interface McpHttpTransportConfig {
  kind: "http";
  url: string;
  /** Values may contain `${placeholder}` tokens (see `secretRefs`). */
  headers?: Record<string, string>;
}

export type McpTransportConfig =
  | McpStdioTransportConfig
  | McpHttpTransportConfig;

/**
 * One MCP server, as a host declares it.
 *
 * The config is a *record*: it is listed, logged, and shown in a UI. No secret
 * material lives here — `secretRefs` names {@link SecretStore} refs and the
 * values are injected into env/header placeholders at connect time and nowhere
 * else. See `secrets.ts` for the substitution and the redaction that keeps the
 * resolved values out of errors and logs.
 */
export interface McpServerConfig {
  /** Namespace for every tool this server contributes. `^[a-z][a-z0-9-]*$`. */
  alias: string;
  transport: McpTransportConfig;
  /** `${placeholder}` token → SecretStore ref. */
  secretRefs?: Record<string, string>;
  /** Default true. A disabled server is never connected and contributes nothing. */
  enabled?: boolean;
  /** Server tool name → the name the canonical id should use instead. */
  toolAliases?: Record<string, string>;
  resilience?: Partial<McpResilienceOptions>;
}

export function isServerEnabled(config: McpServerConfig): boolean {
  return config.enabled !== false;
}
