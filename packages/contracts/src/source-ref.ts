import { Type, type Static } from "@sinclair/typebox";

/**
 * A citation attached to a tool result: "here is where this came from".
 *
 * `kind` is deliberately an open string on the wire and a type parameter in
 * TypeScript. AgentKit is domain-agnostic — the set of things a host can cite is
 * the host's business, not the framework's. A host narrows the field by declaring
 * its own vocabulary and passing it in:
 *
 * ```ts
 * type EdaSourceKind =
 *   | "design" | "schematic" | "pcb" | "net" | "part"
 *   | "library-component" | "symbol" | "footprint"
 *   | "file" | "tool" | "external";
 *
 * type EdaSourceRef = AiSourceRef<EdaSourceKind>;
 * ```
 *
 * (That list is the EDA vocabulary this contract used to hard-code; it lives here
 * only as an example of the shape a host's own union takes.) The default
 * `K = string` keeps every unparameterised `AiSourceRef` usage working unchanged.
 */
export const AiSourceRefSchema = Type.Object({
  id: Type.String(),
  kind: Type.String({
    description:
      "Host-defined source kind. Open on the wire; narrowed at the type level via AiSourceRef<K>.",
  }),
  label: Type.String(),
  refId: Type.Optional(Type.String()),
  path: Type.Optional(Type.String()),
  excerpt: Type.Optional(Type.String()),
  metadata: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});

/**
 * DIVERGENCE (hybrid): the schema types `kind` as an open string; the TS type is
 * generic in it so a host can pin its own vocabulary. `Static<>` cannot express a
 * type parameter, so `kind` is re-declared over the schema's static.
 */
export type AiSourceRef<K extends string = string> = Omit<
  Static<typeof AiSourceRefSchema>,
  "kind"
> & { kind: K };
