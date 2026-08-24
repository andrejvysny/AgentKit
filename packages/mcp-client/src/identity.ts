import { TOOL_NAME_PATTERN } from "@agentkit/contracts";
import { McpError } from "./errors.js";

/**
 * Canonical tool identity for MCP-contributed tools.
 *
 * The canonical id is `mcp.<serverAlias>.<effectiveToolName>` — the grammar is
 * carried over verbatim from OneMind, where it survived contact with real
 * servers. It is the routing key: `McpClientManager.callTool` parses it back
 * into "which server, which tool", so a tool call recorded in a transcript
 * stays resolvable without a side table.
 *
 * `registryName` is a SECOND name, and it exists because the two namespaces
 * disagree. `AiToolRegistry` (and every provider's function-calling schema)
 * accepts `TOOL_NAME_PATTERN` — `[a-zA-Z0-9_-]+`, no dots — so registering the
 * dotted canonical id would be refused and the tool silently dropped during
 * registry staging. The projection is mechanical (`.` → `__`) and checked, so
 * the model-facing name reads as the canonical id with different punctuation,
 * while the canonical id keeps its role as the identity we route and log on.
 */
export interface McpToolIdentity {
  /** `mcp.<serverAlias>.<effectiveToolName>` — routing + collision key. */
  canonicalId: string;
  /** Registry/provider-safe projection of {@link canonicalId} (`.` → `__`). */
  registryName: string;
  serverAlias: string;
  /** The name the SERVER knows the tool by — what `tools/call` must send. */
  toolName: string;
  /** `toolAliases[toolName] ?? toolName` — what the canonical id embeds. */
  effectiveToolName: string;
}

export interface McpToolIdentityInput {
  serverAlias: string;
  toolName: string;
  /** Host-chosen rename, applied before the canonical id is built. */
  toolAlias?: string;
}

const SERVER_ALIAS_PATTERN = /^[a-z][a-z0-9-]*$/;
const MCP_TOOL_NAME_PATTERN = /^[a-z][a-z0-9_-]*(?:\.[a-z0-9_-]+)*$/;
/** Every canonical id is `mcp.` + an alias + `.` + a non-empty remainder. */
const CANONICAL_ID_PATTERN = /^mcp\.[a-z][a-z0-9-]*\..+$/;

const CANONICAL_PREFIX = "mcp.";

export function normalizeServerAlias(value: string): string {
  const trimmed = value.trim();
  if (!SERVER_ALIAS_PATTERN.test(trimmed)) {
    throw new McpError(
      "mcp_invalid_alias",
      `MCP server alias "${value}" is invalid; expected ${SERVER_ALIAS_PATTERN.source}.`,
      { details: { alias: value } },
    );
  }
  return trimmed;
}

export function normalizeMcpToolName(
  value: string,
  field: "toolName" | "toolAlias",
): string {
  const trimmed = value.trim();
  if (!MCP_TOOL_NAME_PATTERN.test(trimmed)) {
    throw new McpError(
      "mcp_invalid_tool_name",
      `MCP ${field} "${value}" is invalid; expected ${MCP_TOOL_NAME_PATTERN.source}.`,
      { details: { [field]: value } },
    );
  }
  return trimmed;
}

/**
 * Project a canonical id onto a name the tool registry will accept.
 *
 * Dots are the only illegal character the canonical grammar can produce, so the
 * mapping is a single substitution. It is verified rather than assumed: a future
 * grammar change that admits another character must fail here, loudly, instead
 * of producing a name that registry staging quietly discards.
 */
export function toRegistryToolName(canonicalId: string): string {
  const projected = canonicalId.split(".").join("__");
  if (!TOOL_NAME_PATTERN.test(projected)) {
    throw new McpError(
      "mcp_invalid_tool_name",
      `Canonical id "${canonicalId}" does not project onto a registry-legal tool name.`,
      { details: { canonicalId, projected } },
    );
  }
  return projected;
}

export function buildMcpToolIdentity(
  input: McpToolIdentityInput,
): McpToolIdentity {
  const serverAlias = normalizeServerAlias(input.serverAlias);
  const toolName = normalizeMcpToolName(input.toolName, "toolName");
  const effectiveToolName =
    input.toolAlias === undefined
      ? toolName
      : normalizeMcpToolName(input.toolAlias, "toolAlias");
  const canonicalId = `${CANONICAL_PREFIX}${serverAlias}.${effectiveToolName}`;
  if (!CANONICAL_ID_PATTERN.test(canonicalId)) {
    throw new McpError(
      "mcp_invalid_canonical_id",
      `Canonical id "${canonicalId}" must be mcp.<serverAlias>.<toolName>.`,
      { details: { canonicalId } },
    );
  }
  return {
    canonicalId,
    registryName: toRegistryToolName(canonicalId),
    serverAlias,
    toolName,
    effectiveToolName,
  };
}

/** The two halves of a canonical id, for routing a call back to its server. */
export function parseMcpCanonicalToolId(canonicalId: string): {
  serverAlias: string;
  effectiveToolName: string;
} {
  if (!CANONICAL_ID_PATTERN.test(canonicalId)) {
    throw new McpError(
      "mcp_invalid_canonical_id",
      `Canonical id "${canonicalId}" must be mcp.<serverAlias>.<toolName>.`,
      { details: { canonicalId } },
    );
  }
  const rest = canonicalId.slice(CANONICAL_PREFIX.length);
  const split = rest.indexOf(".");
  // Guaranteed by the pattern, but the slice below is only safe because of it.
  if (split <= 0 || split === rest.length - 1) {
    throw new McpError(
      "mcp_invalid_canonical_id",
      `Canonical id "${canonicalId}" must be mcp.<serverAlias>.<toolName>.`,
      { details: { canonicalId } },
    );
  }
  return {
    serverAlias: normalizeServerAlias(rest.slice(0, split)),
    effectiveToolName: normalizeMcpToolName(rest.slice(split + 1), "toolName"),
  };
}

/**
 * Build the identities for ONE server's tool batch, failing closed on a clash.
 *
 * A collision is detected before any identity is handed back, so a server whose
 * `toolAliases` collapse two tools onto one name contributes nothing rather than
 * contributing a set where one tool shadows another. Last-write-wins here would
 * mean the model calls `mcp.gh.search` and reaches whichever tool the server
 * happened to list second.
 */
export function buildMcpToolIdentityIndex(
  inputs: readonly McpToolIdentityInput[],
): Map<string, McpToolIdentity> {
  const index = new Map<string, McpToolIdentity>();
  for (const input of inputs) {
    const identity = buildMcpToolIdentity(input);
    const existing = index.get(identity.canonicalId);
    if (existing) throw canonicalCollision(identity, existing);
    index.set(identity.canonicalId, identity);
  }
  return index;
}

/** The one collision error, so both the batch and cross-server checks read alike. */
export function canonicalCollision(
  incoming: McpToolIdentity,
  existing: McpToolIdentity,
): McpError {
  return new McpError(
    "mcp_canonical_id_collision",
    `Canonical id "${incoming.canonicalId}" collides between ` +
      `"${existing.serverAlias}:${existing.toolName}" and ` +
      `"${incoming.serverAlias}:${incoming.toolName}".`,
    {
      details: {
        canonicalId: incoming.canonicalId,
        existing: `${existing.serverAlias}:${existing.toolName}`,
        incoming: `${incoming.serverAlias}:${incoming.toolName}`,
      },
    },
  );
}
