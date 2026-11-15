import { Type, type Static } from "@sinclair/typebox";
import { AiToolDefinitionSchema } from "./tool.js";

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

const Sha256HexSchema = Type.String({ pattern: "^[0-9a-f]{64}$" });

/**
 * The frozen head of a session's prompt (segments S1-S4), recorded once at
 * session start and replaced only by a re-freeze. Digests are lowercase
 * SHA-256 hex. The three optional fields belong to the tool-use track and stay
 * absent until their producers exist.
 */
export const AiSessionPrefixSchema = Type.Object({
  revision: Type.Integer({ minimum: 1, maximum: Number.MAX_SAFE_INTEGER }),
  toolsDigest: Sha256HexSchema,
  systemDigest: Sha256HexSchema,
  rulesDigest: Sha256HexSchema,
  memoryCoreDigest: Sha256HexSchema,
  paramsDigest: Sha256HexSchema,
  frozenAtTask: Type.String({ minLength: 1 }),
  cacheKey: Type.String({ minLength: 1 }),
  toolManifestHash: Type.Optional(Sha256HexSchema),
  /** The frozen tool manifest, sorted by name; sent every turn (roadmap 1.3). */
  toolManifest: Type.Optional(Type.Array(AiToolDefinitionSchema)),
  /** Tools that joined after the freeze; listed, never sent. */
  deferredTools: Type.Optional(Type.Array(AiToolDefinitionSchema)),
  modelProfileId: Type.Optional(Type.String({ minLength: 1 })),
  dialectVersion: Type.Optional(Type.String({ minLength: 1 })),
  modelProfileDigest: Type.Optional(Sha256HexSchema),
});
export type AiSessionPrefix = Static<typeof AiSessionPrefixSchema>;

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
