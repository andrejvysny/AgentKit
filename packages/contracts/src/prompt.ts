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
