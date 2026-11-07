import { Type, type Static } from "@sinclair/typebox";

export const AiPromptPresetSchema = Type.Object({
  id: Type.String(),
  label: Type.String(),
  description: Type.String(),
  systemText: Type.String(),
});
export type AiPromptPreset = Static<typeof AiPromptPresetSchema>;

export const AiPromptContextBlockSchema = Type.Object({
  id: Type.String(),
  title: Type.String(),
  content: Type.String(),
  priority: Type.Number(),
});
export type AiPromptContextBlock = Static<typeof AiPromptContextBlockSchema>;

export interface PromptTemplate {
  id: string;
  revision: string;
  description: string;
  argumentsSchema: import("./json-schema.js").AiJsonSchemaObject;
  messages: { role: "system" | "user" | "assistant"; content: string }[];
  origin?: { kind: "application" | "mcp"; source: string; trusted: boolean };
}
export interface PromptTemplateSelection {
  id: string;
  revision: string;
  arguments: Record<string, unknown>;
}
export interface SkillSelection {
  id: string;
  revision: string;
}
export interface RunContentSelection {
  template?: PromptTemplateSelection;
  skills?: SkillSelection[];
}

/**
 * How much hidden reasoning the provider may spend on one turn.
 *
 * A closed set, and an unrecognised level is a refusal rather than a rounding:
 * a caller that asked for more thinking and silently got less cannot tell from
 * the transcript, and the answer it got may be the one it was trying to avoid.
 * Absent means "the provider's own default", which is not the same thing as the
 * lowest level — a host that fills in a default here has answered a question the
 * caller never asked.
 *
 * Deliberately NOT re-exported from the `schemas.ts` barrel: it is a member
 * schema, and the two turn requests inline it, so the frozen public schema
 * surface is unchanged and a consumer that needs the set reads it off
 * `SubmitMessageRequestSchema.properties.reasoningEffort`.
 */
export const RunReasoningEffortSchema = Type.Union([
  Type.Literal("minimal"),
  Type.Literal("low"),
  Type.Literal("medium"),
  Type.Literal("high"),
]);
export type RunReasoningEffort = Static<typeof RunReasoningEffortSchema>;

export const RunContentSelectionSchema = Type.Object({
  template: Type.Optional(
    Type.Object({
      id: Type.String(),
      revision: Type.String(),
      arguments: Type.Record(Type.String(), Type.Unknown()),
    }),
  ),
  skills: Type.Optional(
    Type.Array(Type.Object({ id: Type.String(), revision: Type.String() }), {
      maxItems: 20,
    }),
  ),
});
