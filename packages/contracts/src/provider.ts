import { Type, type Static } from "@sinclair/typebox";
import { AiMessageContentSchema } from "./content.js";
import { AiToolCallSchema } from "./tool.js";

/**
 * Provider kinds this framework ships presets for. The vocabulary is **open**:
 * `AiProviderKind` is a plain string so a host can register its own provider
 * (a gateway, an internal proxy, a vendor the framework has never heard of)
 * without a contract change. This list is the recognised set, useful for UI
 * pickers and preset lookup — not a validation boundary.
 */
export const KNOWN_PROVIDER_KINDS = [
  "openai",
  "openrouter",
  "openai-compatible",
  "lmstudio",
  "omlx",
] as const;
export type KnownProviderKind = (typeof KNOWN_PROVIDER_KINDS)[number];

export const AiProviderKindSchema = Type.String({
  description:
    "Open provider-kind discriminator; see KNOWN_PROVIDER_KINDS for the recognised vocabulary.",
});
export type AiProviderKind = Static<typeof AiProviderKindSchema>;

export const AiProviderConfigSchema = Type.Object({
  id: Type.String(),
  label: Type.String(),
  kind: AiProviderKindSchema,
  baseUrl: Type.String(),
  apiKey: Type.Optional(Type.String()),
  defaultModel: Type.String(),
  enabled: Type.Boolean(),
  /**
   * Static headers sent on every request to this provider. Merged *under* the
   * built-in accept/authorization headers, so they can add attribution or
   * routing metadata but never override auth.
   */
  extraHeaders: Type.Optional(Type.Record(Type.String(), Type.String())),
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

/**
 * One message in a conversation as it goes to a provider.
 *
 * `content` is {@link AiMessageContentSchema}: a plain string, or an ordered
 * array of content parts for multimodal turns. Parts are meaningful on `user`
 * and `assistant` messages only — a provider client handed parts on a `system`
 * or `tool` message flattens them to text (see `./content.ts`).
 */
export const AiChatMessageSchema = Type.Object({
  role: AiChatRoleSchema,
  content: AiMessageContentSchema,
  name: Type.Optional(Type.String()),
  toolCallId: Type.Optional(Type.String()),
  toolCalls: Type.Optional(Type.Array(AiToolCallSchema)),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type AiChatMessage = Static<typeof AiChatMessageSchema>;
