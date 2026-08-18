import type { AiJsonSchemaObject } from "../json-schema.js";
import type { AiContextBinding } from "../context/bindings.js";
import type { AiSourceRef } from "../sources/source-ref.js";
import type { AiToolLimits } from "./limits.js";

export type AiToolEffect = "read" | "write";

export type AiToolStatus =
  | "requested"
  | "running"
  | "succeeded"
  | "failed"
  | "rejected";

export const TOOL_NAME_PATTERN = /^[a-zA-Z0-9_-]+$/;

export interface AiToolDefinition {
  name: string;
  version: string;
  effect: AiToolEffect;
  capability: string;
  description: string;
  inputSchema: AiJsonSchemaObject;
  outputSchema?: AiJsonSchemaObject;
  /**
   * Optional per-tool execution timeout (ms). When set, the run-loop races
   * `execute()` against the deadline via an AbortController linked to
   * `ctx.signal`, so either run-cancellation or the timeout aborts the call.
   * Local tools omit it (unchanged fast path); remote tools set it from their
   * manifest duration class.
   */
  timeoutMs?: number;
}

export interface AiToolCall {
  id: string;
  name: string;
  argumentsJson: string;
}

export interface AiToolExecutionContext {
  runId: string;
  chatId?: string;
  userId?: string;
  bindings: AiContextBinding[];
  limits: AiToolLimits;
  signal?: AbortSignal;
  metadata?: Record<string, unknown>;
}

export interface AiToolResult<T = unknown> {
  ok: boolean;
  data: T;
  sources: AiSourceRef[];
  warnings: string[];
  truncated: boolean;
  limits: AiToolLimits;
  /** Slim object the model should see instead of `data` (Track A/D). `data` stays for UI. */
  modelData?: unknown;
  /** One-line status the model should see (Track A/D). */
  summary?: string;
  /** Tool-reported status; distinguishes a partial apply from a clean success (Track A/D). */
  status?: "ok" | "partial";
}

/**
 * Balanced model-facing envelope (Wave 0 §0.2). `data` carries `modelData` when present,
 * else the trimmed `data`.
 */
export type AiToolEnvelope = {
  ok: boolean;
  status: "ok" | "error" | "partial";
  summary?: string;
  warnings: string[];
  truncated: boolean;
  data: unknown;
};

export interface AiTool<TInput = unknown, TOutput = unknown> {
  definition: AiToolDefinition;
  execute(
    ctx: AiToolExecutionContext,
    input: TInput,
  ): Promise<AiToolResult<TOutput>>;
}
