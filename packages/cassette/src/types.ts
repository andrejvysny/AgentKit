import type { AiToolResult } from "@agentkit/contracts";
import type { RedactPolicy } from "./redaction.js";

/**
 * Cassette modes (ADR 0021).
 *
 * - `off`: pass-through; zero behavior change, zero recording (the default).
 * - `record`: execute live through the authorized path and persist effects.
 * - `replay`: serve recorded effects only; the live provider/tools are never
 *   called and cannot be reached.
 * - `verify`: replay plus comparison of the normalized execution trace —
 *   every call's identity, order and outcome kind must match the fixture.
 */
export type CassetteMode = "off" | "record" | "replay" | "verify";

/**
 * Data-capture policy.
 *
 * - `metadata` (default): record structure and hashes only — event types and
 *   counts, tool-call ids/names/argument hashes, usage numbers, outcome
 *   flags. Prompt text, streamed text, reasoning content, file contents and
 *   tool payloads are replaced by placeholders. Fixture verification works
 *   against this sanitized view.
 * - `content`: additionally persist the literal text (provider deltas and
 *   completed content, tool result payloads). Opt-in, and a fixture recorded
 *   this way is a real data artifact: treat it as potentially sensitive.
 *
 * Neither mode ever records: transport headers, API keys, credentials, or
 * provider continuation state (only its presence is noted).
 */
export type CassetteCapture = "metadata" | "content";

export interface CassetteOptions {
  mode?: CassetteMode;
  /** Fixture path. Required for record (written on close) and replay/verify. */
  fixture?: string;
  capture?: CassetteCapture;
  /** Fail (default) or warn when recorded effects were never consumed. */
  onUnconsumed?: "fail" | "ignore";
  /**
   * Application-specific redaction, applied on top of the built-in key
   * denylist and credential-shape detection (both captures, always). Return a
   * replacement to redact a value; undefined keeps the scrub's own verdict.
   */
  redactPolicy?: RedactPolicy;
  /** Recording-memory cap (default 50 000 effects); exceeding it fails the run. */
  maxEffects?: number;
}

/** A recorded provider stream: the client's events with volatile stamps removed. */
export interface RecordedRunEvent {
  type: string;
  /** Event payload as recorded (sanitized per the capture policy). */
  data: unknown;
}

export interface RecordedUsage {
  promptTokens?: number;
  completionTokens?: number;
  cachedPromptTokens?: number;
  reasoningTokens?: number;
}

/** The request shape a provider effect can be matched on. */
export interface RecordedProviderRequest {
  providerId: string;
  providerKind: string;
  model: string;
  temperature?: number;
  maxOutputTokens?: number;
  reasoningEffort?: string;
  /** sha256 over the canonical tool definitions offered to the model. */
  toolsFingerprint: string;
  /** Structure-only view of the conversation (see the capture policy). */
  messageShape: RecordedMessageShape[];
  /** Presence only — the state itself is never recorded. */
  continuation: {
    scopeChatId?: string;
    scopeBranchId?: string;
    present: boolean;
  };
  hasRunContent: boolean;
}

export interface RecordedMessageShape {
  role: string;
  /**
   * Under `metadata`: user/system content sha256; assistant/tool content is
   * structure only (calls below). Under `content`: every role's sha256.
   */
  contentSha256?: string;
  /** Only under `content` capture: the literal text, for full matching. */
  contentText?: string;
  toolCalls?: { id: string; name?: string; argsSha256: string }[];
  toolCallId?: string;
  name?: string;
}

/** The normalized trace `verify` compares against, one line per effect. */
export interface EffectTrace {
  /** Event types in order (provider) or the outcome kind (tool). */
  steps: string[];
  usage?: RecordedUsage;
}

interface EffectBase {
  effectId: string;
  parentEffectId?: string;
  sequence: number;
  /** Match keys, computed under BOTH capture policies at record time. */
  structuralFingerprint: string;
  fullFingerprint: string;
  trace: EffectTrace;
}

export interface RecordedProviderEffect extends EffectBase {
  kind: "provider";
  request: RecordedProviderRequest;
  outcome:
    | { type: "stream"; events: RecordedRunEvent[] }
    | {
        type: "error";
        errorCode: string;
        message?: string;
        /** Events yielded before the failure, so replay reproduces the prefix. */
        events?: RecordedRunEvent[];
      }
    | { type: "abort"; message?: string; events?: RecordedRunEvent[] };
}

export interface RecordedToolEffect extends EffectBase {
  kind: "tool";
  tool: { name: string; definitionFingerprint: string };
  argsSha256: string;
  /** Only under `content`. */
  arguments?: unknown;
  /**
   * sha256 of the result's METADATA-normalized form, recorded under both
   * captures so `verify` can compare result content without the fixture
   * holding it. Absent in fixtures recorded before this field existed.
   */
  resultSha256?: string;
  outcome:
    | { type: "result"; result: unknown }
    | { type: "suspended"; prompt: string }
    | { type: "error"; errorCode: string; message?: string }
    | { type: "abort"; message?: string };
}

/** Optional metadata/catalog effects so replay never reaches the network. */
export interface RecordedCapabilitiesEffect extends EffectBase {
  kind: "capabilities";
  model?: string;
  capabilities: unknown;
}

export interface RecordedModelsEffect extends EffectBase {
  kind: "models";
  models: unknown;
}

export type RecordedEffect =
  | RecordedProviderEffect
  | RecordedToolEffect
  | RecordedCapabilitiesEffect
  | RecordedModelsEffect;

export interface CassetteFixture {
  schemaVersion: 1;
  /** The policy the effects were recorded under; drives matching. */
  capture: CassetteCapture;
  createdAt?: string;
  generator?: { name: string; version?: string };
  effects: RecordedEffect[];
}

/** A tool result as reconstituted for replay under `metadata` capture. */
export interface SanitizedToolResult {
  ok: boolean;
  truncated: boolean;
  sources: unknown[];
  warnings: string[];
  limits: { profile: string; maxBytes: number; maxItems?: number };
  status?: "ok" | "partial";
  summary?: string;
  data?: unknown;
  modelData?: unknown;
  modelContent?: unknown[];
}

export function isSanitizedToolResultShape(
  value: unknown,
): value is SanitizedToolResult {
  if (typeof value !== "object" || value === null) return false;
  const record: Record<string, unknown> = { ...value };
  return (
    typeof record["ok"] === "boolean" &&
    typeof record["truncated"] === "boolean" &&
    Array.isArray(record["sources"]) &&
    Array.isArray(record["warnings"])
  );
}

/** Runtime narrowing for recorded results on the replay path. */
export function asToolResult(value: unknown): AiToolResult {
  if (!isSanitizedToolResultShape(value))
    throw new Error("cassette: recorded tool result is malformed");
  return value as unknown as AiToolResult;
}
