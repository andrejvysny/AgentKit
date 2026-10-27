/**
 * Provider-view elision of stale tool results (ADR 0017, "Tool-result elision").
 *
 * A long coding session resends every old `read_file` / command output on every
 * turn. Old results are rarely needed verbatim and the tool can be called again,
 * so results older than the last few turns are cut to head + tail. Pure: the
 * canonical history and the input array are never touched, and only the content
 * of `tool` messages changes, so `toolCallId` pairing and record alignment hold.
 */
import type { AiChatMessage } from "@agentkit/contracts";

export interface ToolResultElisionOptions {
  /** Newest turns left untouched; the current turn always counts as one. */
  keepRecentTurns: number;
  /** Results (or text parts) at most this long are kept whole. */
  maxChars: number;
  headChars: number;
  tailChars: number;
}

export const DEFAULT_TOOL_RESULT_ELISION: ToolResultElisionOptions = {
  keepRecentTurns: 2,
  maxChars: 6000,
  headChars: 2000,
  tailChars: 1000,
};

/** The session's override applied over the defaults; undefined when disabled. */
export function resolveToolResultElision(
  override: { keepRecentTurns?: number; maxChars?: number } | false | undefined,
): ToolResultElisionOptions | undefined {
  if (override === false) return undefined;
  return {
    ...DEFAULT_TOOL_RESULT_ELISION,
    ...(override?.keepRecentTurns === undefined
      ? {}
      : { keepRecentTurns: override.keepRecentTurns }),
    ...(override?.maxChars === undefined
      ? {}
      : { maxChars: override.maxChars }),
  };
}

export interface ToolResultElision {
  messages: AiChatMessage[];
  elided: number;
  savedChars: number;
}

function elideText(
  text: string,
  options: ToolResultElisionOptions,
): string | undefined {
  if (text.length <= options.maxChars) return undefined;
  const head = text.slice(0, options.headChars);
  const tail = options.tailChars > 0 ? text.slice(-options.tailChars) : "";
  const omitted = text.length - head.length - tail.length;
  if (omitted <= 0) return undefined;
  const cut = `${head}\n…[${omitted} characters of an earlier tool result elided; call the tool again if you need them]…\n${tail}`;
  // Odd overrides (a tiny maxChars next to large head/tail) must not grow it.
  return cut.length < text.length ? cut : undefined;
}

/** Index of the first message of the oldest turn that is kept. */
function firstKeptIndex(
  messages: readonly AiChatMessage[],
  keepRecentTurns: number,
): number {
  let turnsSeen = 0;
  for (let index = messages.length - 1; index >= 0; index -= 1) {
    if (messages[index]?.role !== "user") continue;
    turnsSeen += 1;
    if (turnsSeen >= keepRecentTurns) return index;
  }
  return 0;
}

export function elideStaleToolResults(
  messages: AiChatMessage[],
  options: ToolResultElisionOptions,
): ToolResultElision {
  const boundary = firstKeptIndex(
    messages,
    Math.max(1, options.keepRecentTurns),
  );
  let elided = 0;
  let savedChars = 0;
  const out = messages.map((message, index) => {
    if (index >= boundary || message.role !== "tool") return message;
    if (typeof message.content === "string") {
      const cut = elideText(message.content, options);
      if (cut === undefined) return message;
      elided += 1;
      savedChars += message.content.length - cut.length;
      return { ...message, content: cut };
    }
    let changed = false;
    const parts = message.content.map((part) => {
      if (part.type !== "text") return part;
      const cut = elideText(part.text, options);
      if (cut === undefined) return part;
      changed = true;
      savedChars += part.text.length - cut.length;
      return { ...part, text: cut };
    });
    if (!changed) return message;
    elided += 1;
    return { ...message, content: parts as typeof message.content };
  });
  return { messages: out, elided, savedChars };
}
