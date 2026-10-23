import type { AiContentPart, AiSourceRef } from "@agentkit/contracts";
import { MEDIA_TYPE_PATTERN } from "@agentkit/contracts";
import type { McpToolCallOutcome } from "./projection.js";
export interface McpModelContentCapabilities {
  image?: boolean;
  audio?: boolean;
}

export function projectMcpModelContent(
  outcome: McpToolCallOutcome,
  capabilities: McpModelContentCapabilities = {},
) {
  const texts: string[] = [];
  const sources: AiSourceRef[] = [];
  const attachments: AiContentPart[] = [];
  const unsupported = new Set<string>();
  const source = (uri: string, label: string) =>
    sources.push({
      id: `${outcome.canonicalId}:${uri}`,
      kind: "mcp-resource",
      label,
      path: uri,
      metadata: {
        serverAlias: outcome.serverAlias,
        tool: outcome.toolName,
        trusted: false,
      },
    });
  for (const raw of outcome.content) {
    if (!raw || typeof raw !== "object") continue;
    const part = raw as Record<string, unknown>;
    if (part.type === "text" && typeof part.text === "string")
      texts.push(part.text);
    else if (
      part.type === "resource" &&
      part.resource &&
      typeof part.resource === "object"
    ) {
      const resource = part.resource as Record<string, unknown>;
      if (typeof resource.uri === "string") {
        source(resource.uri, resource.uri);
        if (typeof resource.text === "string")
          texts.push(`[Source: ${resource.uri}]\n${resource.text}`);
      }
    } else if (part.type === "resource_link" && typeof part.uri === "string") {
      source(part.uri, typeof part.name === "string" ? part.name : part.uri);
      texts.push(
        `[Resource: ${part.uri}] ${typeof part.description === "string" ? part.description : ""}`,
      );
    } else if (
      part.type === "image" &&
      capabilities.image &&
      typeof part.data === "string" &&
      typeof part.mimeType === "string" &&
      new RegExp(MEDIA_TYPE_PATTERN).test(part.mimeType) &&
      part.mimeType.startsWith("image/")
    )
      attachments.push({
        type: "image",
        source: { kind: "data", mediaType: part.mimeType, base64: part.data },
      });
    else if (
      part.type === "audio" &&
      capabilities.audio &&
      typeof part.data === "string" &&
      ["audio/wav", "audio/mpeg", "audio/mp3"].includes(String(part.mimeType))
    )
      attachments.push({
        type: "audio",
        data: part.data,
        format: part.mimeType === "audio/wav" ? "wav" : "mp3",
      });
    else if (typeof part.type === "string") unsupported.add(part.type);
  }
  return {
    text: texts.join("\n"),
    sources,
    attachments,
    unsupported: [...unsupported],
  };
}
