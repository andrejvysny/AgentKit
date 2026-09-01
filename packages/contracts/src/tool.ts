import { Type, type Static } from "@sinclair/typebox";
import { AiJsonSchemaObjectSchema } from "./json-schema.js";
import { AiSourceRefSchema } from "./source-ref.js";

export const AiToolEffectSchema = Type.Union([
  Type.Literal("read"),
  Type.Literal("write"),
]);
export type AiToolEffect = Static<typeof AiToolEffectSchema>;

export const AiToolStatusSchema = Type.Union([
  Type.Literal("requested"),
  Type.Literal("running"),
  Type.Literal("succeeded"),
  Type.Literal("failed"),
  Type.Literal("rejected"),
]);
export type AiToolStatus = Static<typeof AiToolStatusSchema>;

/** Not a wire DTO — a validation constant applied to `AiToolDefinition.name`. */
export const TOOL_NAME_PATTERN = /^[a-zA-Z0-9_-]+$/;

export const AiToolDefinitionSchema = Type.Object({
  name: Type.String(),
  version: Type.String(),
  effect: AiToolEffectSchema,
  capability: Type.String(),
  description: Type.String(),
  inputSchema: AiJsonSchemaObjectSchema,
  outputSchema: Type.Optional(AiJsonSchemaObjectSchema),
  /**
   * Optional per-tool execution timeout (ms). When set, the run-loop races
   * `execute()` against the deadline via an AbortController linked to
   * `ctx.signal`, so either run-cancellation or the timeout aborts the call.
   * Local tools omit it (unchanged fast path); remote tools set it from their
   * manifest duration class.
   */
  timeoutMs: Type.Optional(
    Type.Number({
      description:
        "Optional per-tool execution timeout (ms). When set, the run-loop races execute() against the deadline via an AbortController linked to ctx.signal.",
    }),
  ),
});
export type AiToolDefinition = Static<typeof AiToolDefinitionSchema>;

export const AiToolCallSchema = Type.Object({
  id: Type.String(),
  name: Type.String(),
  argumentsJson: Type.String(),
});
export type AiToolCall = Static<typeof AiToolCallSchema>;

export const AiContextSizePreferenceSchema = Type.Union([
  Type.Literal("small"),
  Type.Literal("medium"),
  Type.Literal("large"),
]);
export type AiContextSizePreference = Static<
  typeof AiContextSizePreferenceSchema
>;

export const AiToolLimitsSchema = Type.Object({
  profile: AiContextSizePreferenceSchema,
  maxBytes: Type.Number(),
  maxItems: Type.Optional(Type.Number()),
});
export type AiToolLimits = Static<typeof AiToolLimitsSchema>;

export const AiToolResultSchema = Type.Object({
  ok: Type.Boolean(),
  data: Type.Unknown(),
  sources: Type.Array(AiSourceRefSchema),
  warnings: Type.Array(Type.String()),
  truncated: Type.Boolean(),
  limits: AiToolLimitsSchema,
  /** Slim object the model should see instead of `data` (Track A/D). `data` stays for UI. */
  modelData: Type.Optional(Type.Unknown()),
  /** One-line status the model should see (Track A/D). */
  summary: Type.Optional(Type.String()),
  /** Tool-reported status; distinguishes a partial apply from a clean success (Track A/D). */
  status: Type.Optional(
    Type.Union([Type.Literal("ok"), Type.Literal("partial")]),
  ),
});

/**
 * DIVERGENCE (hybrid): the wire schema types `data` as unknown, but the TS type is
 * generic in the payload (`AiToolResult<T>`) — a type-level nicety `Static<>` cannot
 * express, since a TypeBox schema is a value and cannot carry a type parameter. So
 * the type is layered over the schema: every field but `data` comes from
 * `Static<typeof AiToolResultSchema>`; `data` is re-declared as `T`.
 */
export type AiToolResult<T = unknown> = Omit<
  Static<typeof AiToolResultSchema>,
  "data"
> & { data: T };

/**
 * Balanced model-facing envelope (Wave 0 §0.2). `data` carries `modelData` when present,
 * else the trimmed `data`.
 */
export const AiToolEnvelopeSchema = Type.Object({
  ok: Type.Boolean(),
  status: Type.Union([
    Type.Literal("ok"),
    Type.Literal("error"),
    Type.Literal("partial"),
  ]),
  summary: Type.Optional(Type.String()),
  warnings: Type.Array(Type.String()),
  truncated: Type.Boolean(),
  data: Type.Unknown(),
});
export type AiToolEnvelope = Static<typeof AiToolEnvelopeSchema>;

/**
 * WHERE a tool call died. Additive and optional: an envelope without it is a
 * failure whose stage was never recorded, not a failure that happened nowhere.
 *
 * The three are the three gates a call passes through, in order — the argument
 * schema (`validation`), the guard chain (`guard`), and the tool's own body
 * (`execution`) — so a model, a UI, or a retry policy can tell "you wrote the
 * wrong arguments" from "you are not allowed to call this" from "it broke",
 * which the message string alone never distinguishes reliably.
 */
export const AiToolErrorPhaseSchema = Type.Union([
  Type.Literal("validation"),
  Type.Literal("guard"),
  Type.Literal("execution"),
]);
export type AiToolErrorPhase = Static<typeof AiToolErrorPhaseSchema>;

/**
 * The `data` an error {@link AiToolEnvelopeSchema} carries — the structured half
 * of a failed call, as opposed to the human `summary`.
 *
 * `retryable` is the tool's/the framework's own verdict on whether another
 * attempt could plausibly succeed; it is advice for whoever is deciding, never
 * an instruction the run loop acts on by itself (nothing in this repository
 * retries a tool call automatically).
 */
export const AiToolErrorDataSchema = Type.Object({
  errorCode: Type.String(),
  errorMessage: Type.String(),
  phase: Type.Optional(AiToolErrorPhaseSchema),
  retryable: Type.Optional(Type.Boolean()),
});
export type AiToolErrorData = Static<typeof AiToolErrorDataSchema>;
