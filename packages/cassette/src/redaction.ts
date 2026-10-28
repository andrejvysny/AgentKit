import type { AiRunEvent, AiToolResult } from "@agentkit/contracts";
import type {
  CassetteCapture,
  RecordedRunEvent,
  RecordedUsage,
} from "./types.js";

/**
 * Field-name denylist for the deep scrub. Transport-level secrets live
 * outside `AiChatRequest`/`AiRunEvent` by construction, but tool payloads are
 * arbitrary JSON: a tool that echoes an environment dump must not smuggle a
 * credential into a fixture.
 */
const SECRET_KEY_PATTERN =
  /^(authorization|proxy[-_]?authorization|api[-_]?key|apikey|access[-_]?token|refresh[-_]?token|token|secret|client[-_]?secret|credential|password|cookie)$/i;

/** Keys that must never appear anywhere in a fixture, whatever they hold. */
const FORBIDDEN_KEY_PATTERN =
  /^(state|continuation|reasoning_content|reasoningContent|headers|_raw)$/i;

/**
 * Deep-copy with the denylist applied. Values under a denied key are dropped
 * (key removed), not redacted-in-place, so the fixture simply lacks them.
 */
export function scrubDeep(value: unknown, depth = 0): unknown {
  if (depth > 32) return "[truncated-depth]";
  if (Array.isArray(value)) return value.map((v) => scrubDeep(v, depth + 1));
  if (value !== null && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [key, entry] of Object.entries(
      value as Record<string, unknown>,
    )) {
      if (SECRET_KEY_PATTERN.test(key) || FORBIDDEN_KEY_PATTERN.test(key))
        continue;
      out[key] = scrubDeep(entry, depth + 1);
    }
    return out;
  }
  return value;
}

/** Content-bearing fields per event type, replaced under `metadata` capture. */
const CONTENT_FIELDS: Record<string, string[]> = {
  "run.message.delta": ["delta"],
  "run.message.completed": ["content", "reasoning"],
  "run.message.reasoning-delta": ["delta"],
  "run.verification": ["reasoningSummary"],
};

function usageFrom(data: unknown): RecordedUsage | undefined {
  if (typeof data !== "object" || data === null) return undefined;
  const record: Record<string, unknown> = { ...data };
  const usage: RecordedUsage = {};
  for (const key of [
    "promptTokens",
    "completionTokens",
    "cachedPromptTokens",
    "reasoningTokens",
  ] as const) {
    const value = record[key];
    if (typeof value === "number") usage[key] = value;
  }
  return Object.keys(usage).length > 0 ? usage : undefined;
}

/**
 * The recorded form of one provider stream event.
 *
 * `metadata` capture keeps the event type and all non-content data fields —
 * finishReason, toolCallCount, toolCalls (ids/names/args), usage numbers,
 * error codes — and replaces text with `""`. `reasoning_delta` events are
 * dropped entirely (the content field stays a placeholder anyway; the content
 * policy is to never persist reasoning text by default).
 */
export function recordRunEvent(
  event: AiRunEvent,
  capture: CassetteCapture,
): RecordedRunEvent | undefined {
  if (capture === "metadata" && event.type === "run.message.reasoning-delta")
    return undefined;
  const data = scrubDeep(event.data);
  if (capture === "metadata" && typeof data === "object" && data !== null) {
    const contentFields = CONTENT_FIELDS[event.type];
    if (contentFields) {
      const record: Record<string, unknown> = { ...data };
      for (const field of contentFields)
        if (field in record) record[field] = "";
      return { type: event.type, data: record };
    }
  }
  return { type: event.type, data };
}

/**
 * The recorded form of a tool result.
 *
 * `metadata` keeps the outcome flags a test asserts on — ok / status /
 * truncated / limits — plus counts, and drops every text or payload field
 * (`data`, `modelData`, `summary`, `modelContent`, source refs, warning text)
 * which may contain file contents. `content` keeps the whole (scrubbed) result.
 */
export function recordToolResult(
  result: AiToolResult,
  capture: CassetteCapture,
): unknown {
  if (capture === "content") return scrubDeep(result);
  return {
    ok: result.ok,
    truncated: result.truncated,
    sources: result.sources.map(() => ({})),
    warnings: result.warnings.map(() => "redacted"),
    limits: scrubDeep(result.limits),
    ...(result.status !== undefined ? { status: result.status } : {}),
  };
}

/** Re-export for the recorder's usage summary. */
export { usageFrom };
