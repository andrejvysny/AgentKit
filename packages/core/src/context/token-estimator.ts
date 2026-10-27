/**
 * Token estimation without a tokenizer (ADR 0017).
 *
 * The heuristic -- ceil(chars / 4), an image costing 4800 chars, a tool call
 * costing its name plus its JSON arguments -- follows Pi's `estimateTokens` in
 * `packages/coding-agent/src/core/compaction/compaction.ts` (MIT, Mario
 * Zechner, upstream commit 4a080252f06caf63a525d7873c5d94d3c23887e0). The
 * constants are adapted; no code is copied.
 *
 * It is a port so a host with a real tokenizer can swap it in. The default is
 * deliberately conservative-by-simplicity: a budget check built on it decides
 * "roughly how full is the window", not "exactly how many tokens".
 */
import type { AiChatMessage, AiToolDefinition } from "@agentkit/contracts";

export interface TokenEstimator {
  estimateMessage(message: AiChatMessage): number;
  estimateText(text: string): number;
}

/** Pi charges an image 4800 chars (~1200 tokens), whatever its size. */
const IMAGE_CHARS = 4800;

function tokensOfChars(chars: number): number {
  return Math.ceil(chars / 4);
}

function contentChars(content: AiChatMessage["content"]): number {
  if (typeof content === "string") return content.length;
  let chars = 0;
  for (const part of content) {
    if (part.type === "text") chars += part.text.length;
    else if (part.type === "image") chars += IMAGE_CHARS;
    else chars += part.data.length;
  }
  return chars;
}

export const defaultTokenEstimator: TokenEstimator = {
  estimateText: (text) => tokensOfChars(text.length),
  estimateMessage(message) {
    let chars = contentChars(message.content);
    for (const call of message.toolCalls ?? []) {
      chars += call.name.length + call.argumentsJson.length;
    }
    return tokensOfChars(chars);
  },
};

/** Tool definitions travel on every request: their JSON length over four. */
export function estimateTools(
  definitions: readonly AiToolDefinition[],
): number {
  if (definitions.length === 0) return 0;
  return tokensOfChars(JSON.stringify(definitions).length);
}
