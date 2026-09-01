import type { AiToolEnvelope } from "@agentkit/contracts";
import type { ToolCatalogEntry } from "@agentkit/host";
import type { CallToolResult, Tool } from "@modelcontextprotocol/sdk/types.js";

/**
 * Is this tool a write?
 *
 * Read off `AiToolDefinition.effect`, which every AgentKit tool already
 * declares and which `createProposalBuilderTool` REQUIRES to be `"write"` —
 * there is no second marker to invent, and inventing one would leave two places
 * that disagree about what a write is.
 */
export function isWriteTool(entry: ToolCatalogEntry): boolean {
  return entry.definition.effect === "write";
}

/**
 * The catalogue entries an MCP client is allowed to see.
 *
 * With `writesEnabled: false` (the default), write tools are not merely hidden
 * from `tools/list` — the same filter runs on the `tools/call` path, so a client
 * that learned a name from an earlier, permissive session cannot reach it. A
 * filter that only hid things would be a UI, not a policy.
 */
export function visibleEntries(
  entries: readonly ToolCatalogEntry[],
  writesEnabled: boolean,
): ToolCatalogEntry[] {
  return entries.filter((entry) => writesEnabled || !isWriteTool(entry));
}

/**
 * Project one `AiToolDefinition` onto the MCP `Tool` shape — VERBATIM.
 *
 * Three fields cross, unchanged: `name`, `description`, `inputSchema`. In
 * particular the name is the tool's own, NOT `<namespace>__<name>`: the
 * namespace is AgentKit's attribution for a tool (who contributed it, which
 * reserved words are taken), not part of the identifier a caller uses, and
 * rewriting it here would mean an MCP client and a chat turn call the same tool
 * by two different names.
 *
 * `outputSchema` is deliberately NOT forwarded even when the definition carries
 * one: an MCP tool that declares `outputSchema` promises `structuredContent`,
 * and what this server returns is the AgentKit envelope rendered as text. A
 * declaration we do not honour is worse than none — spec-compliant clients
 * validate against it.
 */
export function projectToolDefinition(entry: ToolCatalogEntry): Tool {
  const definition = entry.definition;
  return {
    name: definition.name,
    description: definition.description,
    inputSchema: definition.inputSchema as Tool["inputSchema"],
  };
}

/**
 * Project an {@link AiToolEnvelope} onto an MCP `tools/call` result.
 *
 * The envelope IS the model-facing projection already — `@agentkit/core`'s run
 * loop builds it as `modelData ?? data` plus a one-line `summary` — so the job
 * here is to carry both across without re-summarizing:
 *
 * - `summary`, when present, becomes its OWN leading text block. Clients render
 *   content blocks in order, and burying a one-line status inside a JSON blob is
 *   how it stops being read.
 * - the payload follows as one JSON text block. `envelope.data` is what the
 *   model sees in a chat turn; an envelope with no `data` at all falls back to
 *   the whole envelope, so a client never gets an empty result.
 * - `ok: false` sets `isError`. That includes a partial apply (`ok: false`,
 *   `status: "partial"`), because MCP has no third state and reporting a
 *   half-finished write as a clean success is the failure worth avoiding. The
 *   `status` field is still in the JSON for a client that can tell them apart.
 *
 * A failed envelope's `data` is the structured `AiToolErrorData`
 * (`errorCode`/`errorMessage`/`phase`/`retryable`), so the error detail crosses
 * as data rather than as prose.
 */
export function projectEnvelope(envelope: AiToolEnvelope): CallToolResult {
  const content: CallToolResult["content"] = [];
  if (envelope.summary !== undefined && envelope.summary !== "") {
    content.push({ type: "text", text: envelope.summary });
  }
  const payload = envelope.data === undefined ? envelope : envelope.data;
  content.push({ type: "text", text: safeStringify(payload) });
  return envelope.ok ? { content } : { content, isError: true };
}

/**
 * `JSON.stringify` that cannot throw the session down.
 *
 * A tool is host code returning host data; a cycle or a BigInt in it is a bug in
 * that tool, and the right report is a failed call carrying the reason, not an
 * exception escaping the request handler.
 */
function safeStringify(value: unknown): string {
  try {
    return JSON.stringify(value) ?? "null";
  } catch (err) {
    return JSON.stringify({
      errorCode: "tool_result_unserializable",
      errorMessage: err instanceof Error ? err.message : String(err),
    });
  }
}
