import type { AiJsonSchemaObject, AiToolEffect } from "@agentkit/contracts";
import type { McpServerConfig } from "./config.js";
import { McpError } from "./errors.js";
import {
  buildMcpToolIdentityIndex,
  type McpToolIdentity,
  type McpToolIdentityInput,
} from "./identity.js";

/** One MCP tool, already named the AgentKit way. */
export interface McpToolDescriptor extends McpToolIdentity {
  description: string;
  /** The server's `inputSchema`, VERBATIM. Never widened, never tightened. */
  inputSchema: AiJsonSchemaObject;
  /** `annotations.readOnlyHint === true` ⇒ read; everything else ⇒ write. */
  effect: AiToolEffect;
}

/** What a `tools/call` produced, before it is shaped into an `AiToolResult`. */
export interface McpToolCallOutcome {
  canonicalId: string;
  serverAlias: string;
  /** The name the SERVER was called with. */
  toolName: string;
  /** Text parts joined with newlines — what the model reads. */
  text: string;
  /** Every content part, untouched, for a UI that can render images/resources. */
  content: readonly unknown[];
  structuredContent?: Record<string, unknown>;
  /** Content-part types other than `text`, surfaced as a warning. */
  nonTextTypes: string[];
  /** The server reported the CALL failed. Not a transport fault. */
  isError: boolean;
}

/** The subset of the SDK's listed-tool shape this bridge reads. */
export interface McpListedTool {
  name: string;
  description?: string;
  /** Required by the MCP spec; typed optional so a lax server cannot crash us. */
  inputSchema?: { type: "object"; [key: string]: unknown };
  annotations?: { readOnlyHint?: boolean };
}

/**
 * Project one server's `tools/list` batch onto descriptors.
 *
 * Identity is built for the WHOLE batch first, so a collision (two tools that
 * `toolAliases` collapses onto one name) throws before a single descriptor is
 * returned. Contributing the survivors would mean the model calls
 * `mcp.gh.search` and reaches whichever tool the server happened to list second.
 */
export function projectMcpTools(
  serverAlias: string,
  toolAliases: Record<string, string> | undefined,
  tools: readonly McpListedTool[],
): McpToolDescriptor[] {
  const inputs: McpToolIdentityInput[] = tools.map((tool) => {
    const alias = toolAliases?.[tool.name];
    return {
      serverAlias,
      toolName: tool.name,
      ...(alias === undefined ? {} : { toolAlias: alias }),
    };
  });
  const byToolName = new Map<string, McpToolIdentity>();
  for (const identity of buildMcpToolIdentityIndex(inputs).values()) {
    byToolName.set(identity.toolName, identity);
  }
  return tools.map((tool) => {
    const identity = byToolName.get(tool.name);
    if (!identity) {
      // Unreachable: every listed tool produced exactly one identity above.
      throw new McpError(
        "mcp_invalid_tool_name",
        `MCP server "${serverAlias}" listed tool "${tool.name}" without an identity.`,
      );
    }
    return {
      ...identity,
      description: tool.description ?? `MCP tool ${tool.name} from ${serverAlias}`,
      // Verbatim passthrough. An MCP `inputSchema` is a full draft-07 document
      // and `AiJsonSchemaObject` only models the subset a hand-authored tool
      // uses, so the cast preserves keywords the interface cannot name (`oneOf`,
      // `pattern`, `$defs`) instead of dropping them. Ajv compiles the real
      // document, not our summary of it — and nothing is INJECTED either: adding
      // `additionalProperties: false` here would reject arguments the server
      // documented, while adding `true` would disable the check the schema asked
      // for. The server's schema is the contract.
      // The empty object schema is a FALLBACK for a server that omitted the
      // (spec-required) field, not a default that overwrites anything.
      inputSchema: (tool.inputSchema ??
        EMPTY_INPUT_SCHEMA) as unknown as AiJsonSchemaObject,
      effect: (tool.annotations?.readOnlyHint === true
        ? "read"
        : "write") satisfies AiToolEffect,
    };
  });
}

/** Used only when a server omits `inputSchema` entirely. */
const EMPTY_INPUT_SCHEMA = { type: "object", properties: {} } as const;

interface ContentPart {
  type?: unknown;
  text?: unknown;
}

/** Shape a raw `tools/call` result into the outcome the contributor renders. */
export function projectMcpCallResult(
  serverAlias: string,
  effectiveToolName: string,
  toolName: string,
  result: Record<string, unknown>,
): McpToolCallOutcome {
  const raw = result["content"];
  const content: readonly unknown[] = Array.isArray(raw) ? raw : [];
  const texts: string[] = [];
  const nonTextTypes = new Set<string>();
  for (const part of content) {
    const typed = part as ContentPart;
    if (typed.type === "text" && typeof typed.text === "string") {
      texts.push(typed.text);
    } else if (typeof typed.type === "string") {
      nonTextTypes.add(typed.type);
    }
  }
  const structured = result["structuredContent"];
  return {
    canonicalId: `mcp.${serverAlias}.${effectiveToolName}`,
    serverAlias,
    toolName,
    text: texts.join("\n"),
    content,
    ...(isRecord(structured) ? { structuredContent: structured } : {}),
    nonTextTypes: [...nonTextTypes],
    isError: result["isError"] === true,
  };
}

/**
 * Effective tool name → server tool name.
 *
 * Built once, from config, rather than cached off the last `tools/list`: routing
 * a call must not depend on whether a list happened first, and a config that
 * renames two tools onto one effective name is a config bug worth failing on
 * here — before it becomes an ambiguous call at runtime.
 */
export function buildReverseToolAliases(
  config: McpServerConfig,
): Map<string, string> {
  const reverse = new Map<string, string>();
  for (const [toolName, alias] of Object.entries(config.toolAliases ?? {})) {
    const existing = reverse.get(alias);
    if (existing !== undefined) {
      throw new McpError(
        "mcp_invalid_config",
        `MCP server "${config.alias}" aliases both "${existing}" and "${toolName}" to "${alias}".`,
        { details: { alias: config.alias, toolAlias: alias } },
      );
    }
    reverse.set(alias, toolName);
  }
  return reverse;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
