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
