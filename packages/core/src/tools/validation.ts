import type { ValidateFunction, ErrorObject } from "ajv";

export type ValidationError = { path: string; message: string };

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
 * Map the errors of an already-run Ajv {@link ValidateFunction} to
 * `{ path, message }[]`. Returns `[]` when there are none. Lets callers that
 * own a precompiled validator (e.g. {@link AiToolRegistry}) reuse the same
 * error-mapping logic instead of duplicating it.
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
