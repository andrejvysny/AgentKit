import { createHash } from "node:crypto";
import type { AiRunEvent, AiToolResult } from "@agentkit/contracts";
import type {
  CassetteCapture,
  RecordedRunEvent,
  RecordedUsage,
} from "./types.js";

/**
 * An application-supplied redaction hook. Returning a value REPLACES the
 * entry; returning undefined leaves the scrub's own verdict in place. Applied
 * to every object property (after the key denylist) and to array elements
 * under no key (`key` is then the parent property name, or `"[]"`).
 */
export type RedactPolicy = (key: string, value: unknown) => unknown;

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
 * Value-shape credential detector for free-form strings. Field-name
 * scrubbing cannot see a secret embedded in prose, file contents or a
 * serialized blob, so common credential SHAPES are also caught wherever a
 * string appears. This is defense in depth, not a guarantee: an application
 * with known secret formats should supply `redactPolicy`.
 */
const SECRET_VALUE_PATTERNS: readonly RegExp[] = [
  /\bBearer\s+[A-Za-z0-9._~+/-]{16,}=*/i,
  /\bsk-[A-Za-z0-9_-]{20,}/,
  /\bAKIA[0-9A-Z]{16}\b/,
  /\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}/,
  /\bxox[baprs]-[A-Za-z0-9-]{10,}/,
  /\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b/,
  /-----BEGIN [A-Z ]*PRIVATE KEY-----/,
  /\b[A-Za-z0-9_]*(?:api[_-]?key|token|secret|password)[A-Za-z0-9_]*\s*[=:]\s*\S{8,}/i,
];

export function containsSecretShape(value: string): boolean {
  return SECRET_VALUE_PATTERNS.some((pattern) => pattern.test(value));
}

const REDACTED = "[redacted-secret]";

/**
 * Deep-copy with the denylist, the value-shape detector and the optional
 * application policy applied. Values under a denied key are dropped (key
 * removed), not redacted-in-place, so the fixture simply lacks them.
 */
export function scrubDeep(
  value: unknown,
  depth = 0,
  policy?: RedactPolicy,
  key = "",
): unknown {
  if (depth > 32) return "[truncated-depth]";
  const replaced = policy?.(key, value);
  if (replaced !== undefined && replaced !== value)
    return scrubDeep(replaced, depth + 1, policy, key);
  if (typeof value === "string")
    return containsSecretShape(value) ? REDACTED : value;
  if (Array.isArray(value))
    return value.map((v) => scrubDeep(v, depth + 1, policy, `${key}[]`));
  if (value !== null && typeof value === "object") {
    const out: Record<string, unknown> = {};
    for (const [entryKey, entry] of Object.entries(
      value as Record<string, unknown>,
    )) {
      if (
        SECRET_KEY_PATTERN.test(entryKey) ||
        FORBIDDEN_KEY_PATTERN.test(entryKey)
      )
        continue;
      out[entryKey] = scrubDeep(entry, depth + 1, policy, entryKey);
    }
    return out;
  }
  return value;
}

function sha256(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

/** Content-bearing fields per event type, replaced under `metadata` capture. */
const CONTENT_FIELDS: Record<string, string[]> = {
  "run.message.delta": ["delta"],
  "run.message.completed": ["content", "reasoning"],
  "run.message.reasoning-delta": ["delta"],
  "run.verification": ["reasoningSummary"],
  "run.warning": ["message"],
};

/**
 * Raw argument carriers under `metadata` capture. These are the fields an
 * adversary would not think to name: the property is called `argumentsJson`,
 * not `content`, yet it carries file contents, credentials and user data
 * verbatim. Metadata capture hashes them instead of persisting them.
 */
function stripRawArguments(
  type: string,
  data: Record<string, unknown>,
): Record<string, unknown> {
  const record: Record<string, unknown> = { ...data };
  if (type === "run.tool.requested") {
    if (typeof record.argumentsJson === "string") {
      record.argsSha256 = sha256(record.argumentsJson);
      delete record.argumentsJson;
    }
  }
  if (type === "run.message.completed") {
    const calls = record.toolCalls;
    if (Array.isArray(calls)) {
      record.toolCalls = calls.map((call) => {
        if (typeof call !== "object" || call === null) return call;
        const entry = call as Record<string, unknown>;
        const shape: Record<string, unknown> = {};
        if (typeof entry.id === "string") shape.id = entry.id;
        const name = (entry.function as { name?: unknown } | undefined)?.name;
        if (typeof name === "string") shape.name = name;
        else if (typeof entry.name === "string") shape.name = entry.name;
        const args = entry.argumentsJson ?? entry.arguments;
        if (typeof args === "string") shape.argsSha256 = sha256(args);
        return shape;
      });
    }
  }
  if (type === "run.waiting") {
    // The waiting payload embeds the approval prompt, which quotes command
    // text; keep a matchable hash, not the quote.
    for (const field of ["content", "prompt", "preview"])
      if (typeof record[field] === "string")
        record[field] = sha256(record[field] as string);
  }
  return record;
}

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
 * `metadata` capture keeps the event type and the non-secret data fields —
 * finishReason, tool-call COUNTS and ids/names, argument HASHES, usage
 * numbers, error codes — and replaces every text or argument carrier with
 * `""` or a sha256. `reasoning_delta` events are dropped entirely (the
 * content policy is to never persist reasoning text by default).
 *
 * `content` capture keeps the literal text, but credential-shaped strings are
 * still redacted and the application policy still applies: opting into
 * content does not silently persist an API key that a tool echoed.
 */
export function recordRunEvent(
  event: AiRunEvent,
  capture: CassetteCapture,
  policy?: RedactPolicy,
): RecordedRunEvent | undefined {
  if (capture === "metadata" && event.type === "run.message.reasoning-delta")
    return undefined;
  const data = scrubDeep(event.data, 0, policy);
  if (capture === "metadata" && typeof data === "object" && data !== null) {
    let record = data as Record<string, unknown>;
    const contentFields = CONTENT_FIELDS[event.type];
    if (contentFields) {
      record = { ...record };
      for (const field of contentFields)
        if (field in record) record[field] = "";
    }
    return { type: event.type, data: stripRawArguments(event.type, record) };
  }
  return { type: event.type, data };
}

/**
 * The recorded form of a tool result.
 *
 * `metadata` keeps the outcome flags a test asserts on — ok / status /
 * truncated / limits — plus counts, and drops every text or payload field
 * (`data`, `modelData`, `summary`, `modelContent`, source refs, warning text)
 * which may contain file contents. `content` keeps the whole (scrubbed)
 * result.
 */
export function recordToolResult(
  result: AiToolResult,
  capture: CassetteCapture,
  policy?: RedactPolicy,
): unknown {
  if (capture === "content") return scrubDeep(result, 0, policy);
  return {
    ok: result.ok,
    truncated: result.truncated,
    sources: result.sources.map(() => ({})),
    warnings: result.warnings.map(() => "redacted"),
    limits: scrubDeep(result.limits, 0, policy),
    ...(result.status !== undefined ? { status: result.status } : {}),
  };
}

/** Re-export for the recorder's usage summary. */
export { usageFrom };
