import { Type, type Static } from "@sinclair/typebox";

/** Why this binding is in the context. Framework-level, so it stays a closed union. */
export const AiContextBindingRoleSchema = Type.Union([
  Type.Literal("primary"),
  Type.Literal("reference"),
  Type.Literal("comparison"),
]);
export type AiContextBindingRole = Static<typeof AiContextBindingRoleSchema>;

/** Whether the bound thing still resolves. Framework-level, so it stays a closed union. */
export const AiContextBindingStatusSchema = Type.Union([
  Type.Literal("active"),
  Type.Literal("missing"),
  Type.Literal("stale"),
]);
export type AiContextBindingStatus = Static<
  typeof AiContextBindingStatusSchema
>;

/**
 * A host object pinned into a run's context.
 *
 * As with {@link AiSourceRef}, `kind` is an open string on the wire and a type
 * parameter in TypeScript: what can be bound is the host's domain, not the
 * framework's. A host declares its own vocabulary and passes it in:
 *
 * ```ts
 * type EdaBindingKind =
 *   | "design" | "library-component" | "symbol" | "footprint"
 *   | "file" | "selection" | "net" | "part";
 *
 * type EdaContextBinding = AiContextBinding<EdaBindingKind>;
 * ```
 *
 * (That list is the EDA vocabulary this contract used to hard-code; it lives here
 * only as an example.) The default `K = string` keeps every unparameterised
 * `AiContextBinding` usage working unchanged. `role` and `status` stay closed
 * unions — those *are* framework concepts.
 */
export const AiContextBindingSchema = Type.Object({
  id: Type.String(),
  kind: Type.String({
    description:
      "Host-defined binding kind. Open on the wire; narrowed at the type level via AiContextBinding<K>.",
  }),
  refId: Type.String(),
  label: Type.String(),
  role: AiContextBindingRoleSchema,
  status: AiContextBindingStatusSchema,
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});

/**
 * DIVERGENCE (hybrid): the schema types `kind` as an open string; the TS type is
 * generic in it so a host can pin its own vocabulary. `Static<>` cannot express a
 * type parameter, so `kind` is re-declared over the schema's static.
 */
export type AiContextBinding<K extends string = string> = Omit<
  Static<typeof AiContextBindingSchema>,
  "kind"
> & { kind: K };
