import { Type, type Static } from "@sinclair/typebox";
import { AiToolCallSchema } from "./tool.js";

export const AiProviderKindSchema = Type.Union([
  Type.Literal("openai"),
  Type.Literal("openrouter"),
  Type.Literal("openai-compatible"),
  Type.Literal("lmstudio"),
  Type.Literal("omlx"),
  Type.Literal("openpcb-cloud"),
]);
export type AiProviderKind = Static<typeof AiProviderKindSchema>;

export const AiProviderConfigSchema = Type.Object({
  id: Type.String(),
  label: Type.String(),
  kind: AiProviderKindSchema,
  baseUrl: Type.String(),
  apiKey: Type.Optional(Type.String()),
  defaultModel: Type.String(),
  enabled: Type.Boolean(),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type AiProviderConfig = Static<typeof AiProviderConfigSchema>;

export const AiProviderCapabilitiesSchema = Type.Object({
  streaming: Type.Boolean(),
  toolCalling: Type.Boolean(),
  modelList: Type.Boolean(),
  vision: Type.Optional(Type.Boolean()),
  jsonMode: Type.Optional(Type.Boolean()),
  maxContextTokens: Type.Optional(Type.Number()),
  checkedAt: Type.Optional(Type.String()),
  warning: Type.Optional(Type.String()),
});
export type AiProviderCapabilities = Static<
  typeof AiProviderCapabilitiesSchema
>;

export const AiProviderModelSchema = Type.Object({
  providerId: Type.String(),
  modelId: Type.String(),
  displayName: Type.Union([Type.String(), Type.Null()]),
  contextWindowTokens: Type.Optional(Type.Number()),
  supportsToolCalling: Type.Optional(Type.Boolean()),
  fetchedAt: Type.String(),
});
export type AiProviderModel = Static<typeof AiProviderModelSchema>;

export const AiChatRoleSchema = Type.Union([
  Type.Literal("system"),
  Type.Literal("user"),
  Type.Literal("assistant"),
  Type.Literal("tool"),
]);
export type AiChatRole = Static<typeof AiChatRoleSchema>;

export const AiChatMessageSchema = Type.Object({
  role: AiChatRoleSchema,
  content: Type.String(),
  name: Type.Optional(Type.String()),
  toolCallId: Type.Optional(Type.String()),
  toolCalls: Type.Optional(Type.Array(AiToolCallSchema)),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type AiChatMessage = Static<typeof AiChatMessageSchema>;
