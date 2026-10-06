import { Type, type Static } from "@sinclair/typebox";
import { Value } from "@sinclair/typebox/value";

/** Non-secret connection scope; generation changes when the host replaces a connection. */
export const AiProviderContinuationSchema = Type.Object(
  {
    version: Type.Literal(1),
    scope: Type.Object(
      {
        providerId: Type.String({ minLength: 1 }),
        protocol: Type.Literal("responses"),
        connectionId: Type.String({ minLength: 1 }),
        generation: Type.Integer({ minimum: 0 }),
        chatId: Type.String({ minLength: 1 }),
        branchId: Type.String({ minLength: 1 }),
        model: Type.String({ minLength: 1 }),
      },
      { additionalProperties: false },
    ),
    messageCount: Type.Integer({ minimum: 0 }),
    messageDigest: Type.String({ pattern: "^[a-f0-9]{64}$" }),
    /** Contains opaque encrypted provider state. Trusted storage only, never a public DTO. */
    inputItems: Type.Array(Type.Record(Type.String(), Type.Unknown()), {
      maxItems: 1024,
    }),
  },
  { additionalProperties: false },
);
export type AiProviderContinuation = Static<
  typeof AiProviderContinuationSchema
>;

/** Uses the same private-state schema as provider request validation. */
export function isAiProviderContinuation(
  value: unknown,
): value is AiProviderContinuation {
  return Value.Check(AiProviderContinuationSchema, value);
}

export function isAiProviderContinuationScope(
  value: unknown,
): value is AiProviderContinuation["scope"] {
  return Value.Check(AiProviderContinuationSchema.properties.scope, value);
}
