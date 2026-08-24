import type { AiMessageContent, AiTextPart } from "@agentkit/contracts";

/**
 * Flatten a message body down to the text a string-only consumer can use.
 *
 * A string passes through untouched — the common case must stay byte-identical.
 * A parts array is reduced to its text parts, joined with newlines; non-text
 * parts (images today, more later) are dropped, because there is nothing
 * faithful to turn them into. Lossy on purpose: the caller that flattens is
 * expected to say so — the provider client emits a `multimodal_flattened`
 * warning rather than pretending the images arrived.
 */
export function messageContentToText(content: AiMessageContent): string {
  if (typeof content === "string") return content;
  return content
    .filter((part): part is AiTextPart => part.type === "text")
    .map((part) => part.text)
    .join("\n");
}
