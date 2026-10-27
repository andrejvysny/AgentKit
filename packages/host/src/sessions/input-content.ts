import type { AiMessageContent } from "@agentkit/contracts";

/**
 * The message body of a stored steer or follow-up (`{ content }`); undefined
 * when the record is malformed.
 */
export function readInputContent(
  content: unknown,
): AiMessageContent | undefined {
  if (typeof content !== "object" || content === null) return undefined;
  const body = (content as { content?: unknown }).content;
  if (typeof body === "string") return body;
  return Array.isArray(body) ? (body as AiMessageContent) : undefined;
}
