/** Keys a JSON object must carry to look like an attempted tool invocation. */
const NAME_KEYS = ["name", "tool", "tool_name", "function"];
const ARG_KEYS = ["arguments", "parameters", "args", "input"];

const FENCE_PATTERN = /```(?:json|tool_call|json5)?\s*\n?([\s\S]*?)```/gi;

/**
 * The wrapper tags the common "tool calls as text" model families print.
 *
 * Qwen and the Hermes/ChatML lineage emit `<tool_call>{…}</tool_call>`; a few
 * fine-tunes use `<function_call>`. This is the shape a detector that only
 * looked for fenced JSON missed entirely — and it is the most common one.
 * Closed blocks only: an unterminated tag is indistinguishable from prose about
 * tool calls, and this predicate's cost of being wrong is a banner telling a
 * user their working model is broken.
 */
const XML_CALL_PATTERN = /<(tool_call|function_call)>([\s\S]*?)<\/\1>/gi;

/**
 * Does this answer contain something shaped like a tool call the model wrote
 * out as TEXT instead of calling?
 *
 * Weaker and distilled models frequently "call" a tool by printing
 * ```json {"name": "...", "arguments": {...}} ``` into the message body, or by
 * emitting `<tool_call>{…}</tool_call>` verbatim, instead of emitting real tool
 * calls. The run then completes successfully, the model narrates what it did,
 * and nothing happened — a far more confusing failure than an outright error,
 * because every visible signal says it worked.
 *
 * THREE PROBES, one rule. A fenced block, an XML call block, and — only when
 * the WHOLE answer is one JSON value — the answer itself. Each candidate object
 * must carry both a name-ish and an args-ish key, and **the name it carries must
 * be a tool that was actually staged for this run**. That last condition is what
 * separates "the model tried to call `search_docs` and failed" from "the model
 * showed me a `{"name": …, "parameters": …}` config example", which is a shape
 * that turns up in documentation, JSON-schema explanations and API samples all
 * the time — and which the fence-only predicate flagged.
 *
 * Callers additionally require that the run produced no REAL tool calls before
 * acting on the answer.
 */
export function looksLikeEmulatedToolCall(
  content: string,
  stagedToolNames: ReadonlySet<string>,
): boolean {
  if (!content || stagedToolNames.size === 0) return false;
  for (const block of candidateBlocks(content)) {
    if (namesAStagedTool(block, stagedToolNames)) return true;
  }
  return false;
}

/** Every substring of `content` that might parse as a printed tool call. */
function* candidateBlocks(content: string): Generator<string> {
  // `matchAll` needs a fresh lastIndex each call — the patterns are
  // module-level and carry the /g flag, so state would leak between
  // invocations.
  FENCE_PATTERN.lastIndex = 0;
  for (const match of content.matchAll(FENCE_PATTERN)) {
    const block = match[1]?.trim();
    if (block) yield block;
  }
  XML_CALL_PATTERN.lastIndex = 0;
  for (const match of content.matchAll(XML_CALL_PATTERN)) {
    const block = match[2]?.trim();
    if (block) yield block;
  }
  // The bare case: the entire answer IS the JSON object, with no fence and no
  // tag around it. Deliberately the WHOLE trimmed answer and not "a JSON object
  // found somewhere in the prose" — scanning for embedded braces would flag
  // every message that quotes a payload, and this predicate's job is to explain
  // a turn in which nothing ran, not to hunt for JSON.
  const trimmed = content.trim();
  if (trimmed.startsWith("{") || trimmed.startsWith("[")) yield trimmed;
}

/**
 * Whether a block parses as a tool call naming one of the staged tools.
 *
 * TWO SHAPES, because models print both: the flat `{"name": "x", "arguments":
 * {…}}` of the Hermes/Qwen templates, and the OpenAI wire shape
 * `{"function": {"name": "x", "arguments": "…"}}` that models trained on it
 * reproduce — where the name and the args are BOTH one level down, so a check
 * that only looked at the outer object would find neither.
 */
function namesAStagedTool(
  block: string,
  stagedToolNames: ReadonlySet<string>,
): boolean {
  let parsed: unknown;
  try {
    parsed = JSON.parse(block);
  } catch {
    return false;
  }
  const candidates = Array.isArray(parsed) ? parsed : [parsed];
  for (const candidate of candidates) {
    if (!isRecord(candidate)) continue;
    if (isStagedCall(candidate, stagedToolNames)) return true;
    const nested = Object.hasOwn(candidate, "function")
      ? candidate["function"]
      : undefined;
    if (isRecord(nested) && isStagedCall(nested, stagedToolNames)) return true;
  }
  return false;
}

/** One object carrying both an args-ish key and a staged tool's name. */
function isStagedCall(
  record: Record<string, unknown>,
  stagedToolNames: ReadonlySet<string>,
): boolean {
  const keys = new Set(Object.keys(record));
  if (!ARG_KEYS.some((key) => keys.has(key))) return false;
  for (const key of NAME_KEYS) {
    // `Object.hasOwn` rather than a plain read, so a payload with a key like
    // `constructor` cannot hand back something off the prototype chain.
    if (!Object.hasOwn(record, key)) continue;
    const value = record[key];
    if (typeof value === "string" && stagedToolNames.has(value)) return true;
  }
  return false;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** The message shown when an emulated tool call is detected. */
export const EMULATED_TOOL_CALL_MESSAGE =
  "This model wrote tool calls as text instead of calling the tools, so nothing " +
  "was actually run. Pick a model with native tool-calling support, or check " +
  "that the provider has tool calling enabled.";
