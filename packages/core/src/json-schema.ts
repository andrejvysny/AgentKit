export type AiJsonPrimitiveType =
  | "string"
  | "number"
  | "integer"
  | "boolean"
  | "object"
  | "array"
  | "null";

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
