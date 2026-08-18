import { Type, type Static } from "@sinclair/typebox";

export const AiJsonPrimitiveTypeSchema = Type.Union([
  Type.Literal("string"),
  Type.Literal("number"),
  Type.Literal("integer"),
  Type.Literal("boolean"),
  Type.Literal("object"),
  Type.Literal("array"),
  Type.Literal("null"),
]);
export type AiJsonPrimitiveType = Static<typeof AiJsonPrimitiveTypeSchema>;

/**
 * EXEMPTION from the "TypeBox is the single source" rule.
 *
 * `AiJsonSchemaObject` is a *meta-type*: it describes the JSON Schema documents a
 * tool author writes for `AiToolDefinition.inputSchema`/`outputSchema`. It is not a
 * wire DTO of this contract. Encoding it recursively in TypeBox (`Type.Recursive`)
 * would buy nothing — the value it describes is itself a schema, and the authoring
 * ergonomics of a plain, fully-documented interface are strictly better. So the
 * interface below stays hand-written and is the source of truth for its own type.
 *
 * {@link AiJsonSchemaObjectSchema} is the TypeBox handle used when this shape is
 * embedded in another schema (e.g. `AiToolDefinitionSchema.inputSchema`). It is a
 * `Type.Unsafe` over "any JSON object", carrying `AiJsonSchemaObject` as its static
 * type: the wire check is structural (an object), the TS type stays exact.
 */
export interface AiJsonSchemaObject {
  type?: AiJsonPrimitiveType | AiJsonPrimitiveType[];
  description?: string;
  properties?: Record<string, AiJsonSchemaObject>;
  required?: string[];
  items?: AiJsonSchemaObject;
  enum?: Array<string | number | boolean | null>;
  additionalProperties?: boolean | AiJsonSchemaObject;
  default?: unknown;
  minimum?: number;
  maximum?: number;
  minLength?: number;
  maxLength?: number;
  minItems?: number;
  maxItems?: number;
}

export const AiJsonSchemaObjectSchema = Type.Unsafe<AiJsonSchemaObject>({
  type: "object",
  additionalProperties: true,
  description: "A JSON Schema document (see AiJsonSchemaObject).",
});
