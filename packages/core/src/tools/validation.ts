import Ajv, { type ValidateFunction, type ErrorObject } from "ajv";
import type { AiJsonSchemaObject } from "@agentkit/contracts";

export interface AiValidationError {
  path: string;
  message: string;
}

/**
 * Hand-rolled structural validator kept for existing callers/tests. Validates
 * `type`, required properties, nested properties, and array items. Does NOT
 * cover `oneOf`/`enum`/`maxLength`/`additionalProperties` — use
 * {@link validateToolInput} (Ajv) for full JSON-Schema coverage.
 */
export function validateAgainstSchema(
  value: unknown,
  schema: AiJsonSchemaObject,
  path = "",
): AiValidationError[] {
  const errors: AiValidationError[] = [];
  const type = schema.type;
  if (type !== undefined) {
    const types = Array.isArray(type) ? type : [type];
    if (!types.some((t) => matchesType(value, t))) {
      errors.push({
        path: path || "$",
        message: `expected type ${types.join("|")}`,
      });
      return errors;
    }
  }
  if (
    typeof value === "object" &&
    value !== null &&
    !Array.isArray(value) &&
    schema.properties
  ) {
    const obj = value as Record<string, unknown>;
    for (const required of schema.required ?? []) {
      if (!(required in obj)) {
        errors.push({
          path: joinPath(path, required),
          message: "required property missing",
        });
      }
    }
    for (const [key, propSchema] of Object.entries(schema.properties)) {
      if (key in obj) {
        errors.push(
          ...validateAgainstSchema(obj[key], propSchema, joinPath(path, key)),
        );
      }
    }
  }
  if (Array.isArray(value) && schema.items) {
    value.forEach((item, idx) => {
      errors.push(
        ...validateAgainstSchema(
          item,
          schema.items as AiJsonSchemaObject,
          `${path}[${idx}]`,
        ),
      );
    });
  }
  return errors;
}

function matchesType(value: unknown, type: string): boolean {
  switch (type) {
    case "string":
      return typeof value === "string";
    case "number":
      return typeof value === "number" && Number.isFinite(value);
    case "integer":
      return typeof value === "number" && Number.isInteger(value);
    case "boolean":
      return typeof value === "boolean";
    case "object":
      return (
        typeof value === "object" && value !== null && !Array.isArray(value)
      );
    case "array":
      return Array.isArray(value);
    case "null":
      return value === null;
    default:
      return true;
  }
}

function joinPath(base: string, key: string): string {
  return base ? `${base}.${key}` : key;
}

export type ValidationError = { path: string; message: string };

/**
 * Shared Ajv instance for {@link validateToolInput}. `strict: false` because tool
 * inputSchemas are authored loosely (extra/unknown keywords tolerated);
 * `allErrors: true` so every problem is surfaced at once.
 */
const ajv = new Ajv({ allErrors: true, strict: false });

/**
 * Compiled-validator cache keyed by the schema object identity. Tool inputSchemas
 * are stable references (defined once per tool), so a WeakMap keeps validators
 * alive without leaking and avoids recompiling on every call.
 */
const schemaCache = new WeakMap<object, ValidateFunction>();

function getValidator(schema: object): ValidateFunction {
  const cached = schemaCache.get(schema);
  if (cached) return cached;
  const validate = ajv.compile(schema);
  schemaCache.set(schema, validate);
  return validate;
}

/**
 * Map one Ajv error to `{ path, message }`. Path prefers `instancePath` (the data
 * location), falls back to `schemaPath`, and strips the leading slash so paths
 * read like `source` not `/source`. For `additionalProperties` the offending key
 * is appended (Ajv reports it via params, not the path).
 */
export function mapAjvError(err: ErrorObject): ValidationError {
  const raw = err.instancePath || err.schemaPath || "";
  const path = raw.replace(/^\//, "");
  const extra =
    err.keyword === "additionalProperties" &&
    typeof err.params.additionalProperty === "string"
      ? ` (${err.params.additionalProperty})`
      : "";
  return { path, message: `${err.message ?? "invalid"}${extra}` };
}

/**
 * Validate already-parsed tool arguments against a JSON Schema using Ajv.
 * Supports the full draft-07 core (`oneOf`, `enum`, `maxLength`,
 * `additionalProperties`, …) that the hand-rolled {@link validateAgainstSchema}
 * cannot. Returns `[]` when valid, else `{ path, message }[]`.
 */
export function validateToolInput(
  schema: unknown,
  args: unknown,
): ValidationError[] {
  if (typeof schema !== "object" || schema === null) {
    // No usable schema → nothing to validate against.
    return [];
  }
  const validate = getValidator(schema);
  const valid = validate(args);
  if (valid) return [];
  return (validate.errors ?? []).map(mapAjvError);
}

/**
 * Map the errors of an already-run Ajv {@link ValidateFunction} to
 * `{ path, message }[]`. Returns `[]` when there are none. Lets callers that
 * own a precompiled validator (e.g. {@link AiToolRegistry}) reuse the same
 * error-mapping logic as {@link validateToolInput} instead of duplicating it.
 */
export function mapValidatorErrors(
  validate: ValidateFunction,
): ValidationError[] {
  return (validate.errors ?? []).map(mapAjvError);
}

export function parseToolArguments(
  argumentsJson: string,
): { ok: true; value: unknown } | { ok: false; error: string } {
  try {
    return { ok: true, value: JSON.parse(argumentsJson) };
  } catch (err) {
    return {
      ok: false,
      error: err instanceof Error ? err.message : "invalid JSON",
    };
  }
}
