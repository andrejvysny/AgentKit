/** Keys a JSON object must carry to look like an attempted tool invocation. */
const NAME_KEYS = ["name", "tool", "tool_name", "function"];
const ARG_KEYS = ["arguments", "parameters", "args", "input"];

const FENCE_PATTERN = /```(?:json|tool_call|json5)?\s*\n?([\s\S]*?)```/gi;

/**
 * Does this answer contain a fenced JSON block shaped like a tool call?
 *
 * Weaker and distilled models frequently "call" a tool by printing
 * ```json {"name": "...", "arguments": {...}} ``` into the message body instead
 * of emitting real tool calls. The run then completes successfully, the model
 * narrates what it did, and nothing happened — a far more confusing failure than
 * an outright error, because every visible signal says it worked.
 *
 * Deliberately conservative: it fires only when a parsed object carries BOTH a
 * name-ish and an args-ish key, so a model legitimately *showing* JSON (a config
 * example, a data sample) is not flagged. Callers additionally require that the
 * run produced no real tool calls before acting on the answer.
 */
export function looksLikeEmulatedToolCall(content: string): boolean {
  if (!content) return false;
  // `matchAll` needs a fresh lastIndex each call — the pattern is module-level
  // and carries the /g flag, so state would leak between invocations.
  FENCE_PATTERN.lastIndex = 0;
  for (const match of content.matchAll(FENCE_PATTERN)) {
    const block = match[1]?.trim();
    if (!block) continue;
    let parsed: unknown;
    try {
      parsed = JSON.parse(block);
    } catch {
      continue;
    }
    const candidates = Array.isArray(parsed) ? parsed : [parsed];
    for (const candidate of candidates) {
      if (!candidate || typeof candidate !== "object") continue;
      const keys = new Set(Object.keys(candidate as Record<string, unknown>));
      const hasName = NAME_KEYS.some((key) => keys.has(key));
      const hasArgs = ARG_KEYS.some((key) => keys.has(key));
      if (hasName && hasArgs) return true;
    }
  }
  return false;
}

/** The message shown when an emulated tool call is detected. */
export const EMULATED_TOOL_CALL_MESSAGE =
  "This model wrote tool calls as text instead of calling the tools, so nothing " +
  "was actually run. Pick a model with native tool-calling support, or check " +
  "that the provider has tool calling enabled.";
