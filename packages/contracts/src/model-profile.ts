import { Type, type Static } from "@sinclair/typebox";

/**
 * Rust-owned per-model tool-use profile (native tool use T2.1, ADR 0030).
 * Resolved when a session starts and frozen with it; the session prefix
 * records only its id, dialect version and digest.
 */
export const AiModelFamilySchema = Type.Union([
  Type.Literal("anthropic"),
  Type.Literal("openai"),
  Type.Literal("qwen"),
  Type.Literal("generic"),
]);
export type AiModelFamily = Static<typeof AiModelFamilySchema>;

export const AiModelLoadModeSchema = Type.Union([
  Type.Literal("indirect"),
  Type.Literal("promote"),
]);
export type AiModelLoadMode = Static<typeof AiModelLoadModeSchema>;

export const AiModelToolChoicePolicySchema = Type.Union([
  Type.Literal("auto_only"),
  Type.Literal("auto_required_named"),
]);
export type AiModelToolChoicePolicy = Static<
  typeof AiModelToolChoicePolicySchema
>;

export const AiModelEffortChannelSchema = Type.Union([
  Type.Literal("effort_field"),
  Type.Literal("template_option"),
  Type.Literal("none"),
]);
export type AiModelEffortChannel = Static<typeof AiModelEffortChannelSchema>;

export const AiModelImagePositionSchema = Type.Union([
  Type.Literal("user"),
  Type.Literal("tool_result"),
]);
export type AiModelImagePosition = Static<typeof AiModelImagePositionSchema>;

export const AiModelLimitSourceSchema = Type.Union([
  Type.Literal("user"),
  Type.Literal("proxy"),
  Type.Literal("server"),
  Type.Literal("probe"),
  Type.Literal("estimated"),
]);
export type AiModelLimitSource = Static<typeof AiModelLimitSourceSchema>;

const AiModelSamplingSetSchema = Type.Object({
  temperature: Type.Optional(Type.Number()),
  topP: Type.Optional(Type.Number()),
  topK: Type.Optional(Type.Integer({ minimum: 0 })),
  minP: Type.Optional(Type.Number()),
  presencePenalty: Type.Optional(Type.Number()),
  repetitionPenalty: Type.Optional(Type.Number()),
});

const AiModelEffortSettingSchema = Type.Object({
  thinking: Type.Boolean(),
  effort: Type.Optional(Type.String({ minLength: 1 })),
});

export const AiModelToolProfileSchema = Type.Object({
  id: Type.String({ minLength: 1 }),
  family: AiModelFamilySchema,
  dialect: Type.Object({
    id: Type.String({ minLength: 1 }),
    version: Type.String({ minLength: 1 }),
  }),
  directTools: Type.Array(Type.String({ minLength: 1 })),
  maxVisibleTools: Type.Integer({ minimum: 1 }),
  loadMode: AiModelLoadModeSchema,
  toolChoice: AiModelToolChoicePolicySchema,
  sampling: Type.Object({
    thinking: AiModelSamplingSetSchema,
    nonThinking: AiModelSamplingSetSchema,
  }),
  reasoning: Type.Object({
    effortChannel: AiModelEffortChannelSchema,
    effortMap: Type.Object({
      off: Type.Optional(AiModelEffortSettingSchema),
      minimal: Type.Optional(AiModelEffortSettingSchema),
      low: Type.Optional(AiModelEffortSettingSchema),
      medium: Type.Optional(AiModelEffortSettingSchema),
      high: Type.Optional(AiModelEffortSettingSchema),
    }),
    sendBack: Type.Boolean(),
  }),
  bodyExtras: Type.Object(
    {
      chat_template_kwargs: Type.Optional(
        Type.Record(Type.String(), Type.Unknown()),
      ),
      parallel_tool_calls: Type.Optional(Type.Boolean()),
    },
    { additionalProperties: false },
  ),
  messageRules: Type.Object({
    systemFirstOnly: Type.Boolean(),
    escapeToolResultTags: Type.Boolean(),
    imagePositions: Type.Array(AiModelImagePositionSchema),
  }),
  limits: Type.Object({
    contextWindow: Type.Integer({ minimum: 1 }),
    maxOutput: Type.Integer({ minimum: 1 }),
    source: AiModelLimitSourceSchema,
  }),
  recovery: Type.Object({
    salvage: Type.Boolean(),
    nudge: Type.Boolean(),
  }),
  qualifiedToolsets: Type.Array(Type.String({ minLength: 1 })),
});
export type AiModelToolProfile = Static<typeof AiModelToolProfileSchema>;
