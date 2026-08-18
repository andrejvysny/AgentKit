import { Type, type Static } from "@sinclair/typebox";

export const AiSourceRefKindSchema = Type.Union([
  Type.Literal("design"),
  Type.Literal("schematic"),
  Type.Literal("pcb"),
  Type.Literal("net"),
  Type.Literal("part"),
  Type.Literal("library-component"),
  Type.Literal("symbol"),
  Type.Literal("footprint"),
  Type.Literal("file"),
  Type.Literal("tool"),
  Type.Literal("external"),
]);
export type AiSourceRefKind = Static<typeof AiSourceRefKindSchema>;

export const AiSourceRefSchema = Type.Object({
  id: Type.String(),
  kind: AiSourceRefKindSchema,
  label: Type.String(),
  refId: Type.Optional(Type.String()),
  path: Type.Optional(Type.String()),
  excerpt: Type.Optional(Type.String()),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type AiSourceRef = Static<typeof AiSourceRefSchema>;
