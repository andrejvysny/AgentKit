import { Type, type Static } from "@sinclair/typebox";

export const AiContextBindingKindSchema = Type.Union([
  Type.Literal("design"),
  Type.Literal("library-component"),
  Type.Literal("symbol"),
  Type.Literal("footprint"),
  Type.Literal("file"),
  Type.Literal("selection"),
  Type.Literal("net"),
  Type.Literal("part"),
]);
export type AiContextBindingKind = Static<typeof AiContextBindingKindSchema>;

export const AiContextBindingRoleSchema = Type.Union([
  Type.Literal("primary"),
  Type.Literal("reference"),
  Type.Literal("comparison"),
]);
export type AiContextBindingRole = Static<typeof AiContextBindingRoleSchema>;

export const AiContextBindingStatusSchema = Type.Union([
  Type.Literal("active"),
  Type.Literal("missing"),
  Type.Literal("stale"),
]);
export type AiContextBindingStatus = Static<
  typeof AiContextBindingStatusSchema
>;

export const AiContextBindingSchema = Type.Object({
  id: Type.String(),
  kind: AiContextBindingKindSchema,
  refId: Type.String(),
  label: Type.String(),
  role: AiContextBindingRoleSchema,
  status: AiContextBindingStatusSchema,
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});
export type AiContextBinding = Static<typeof AiContextBindingSchema>;
