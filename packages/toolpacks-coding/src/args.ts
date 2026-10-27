import { CodingError } from "./errors.js";

/** Tool arguments arrive as parsed JSON; narrow them without trusting them. */
export function asRecord(input: unknown): Record<string, unknown> {
  if (typeof input !== "object" || input === null || Array.isArray(input))
    throw new CodingError("invalid_arguments", "arguments must be an object");
  return input as Record<string, unknown>;
}

export function reqString(
  input: Record<string, unknown>,
  key: string,
  maxLength = 4096,
): string {
  const value = input[key];
  if (typeof value !== "string" || value.length === 0)
    throw new CodingError(
      "invalid_arguments",
      `"${key}" must be a non-empty string`,
    );
  if (value.length > maxLength)
    throw new CodingError(
      "invalid_arguments",
      `"${key}" is longer than ${maxLength} characters`,
    );
  return value;
}

export function optString(
  input: Record<string, unknown>,
  key: string,
  maxLength = 4096,
): string | undefined {
  if (input[key] === undefined) return undefined;
  return reqString(input, key, maxLength);
}

export function optBool(
  input: Record<string, unknown>,
  key: string,
): boolean | undefined {
  const value = input[key];
  if (value === undefined) return undefined;
  if (typeof value !== "boolean")
    throw new CodingError("invalid_arguments", `"${key}" must be a boolean`);
  return value;
}

export function optInt(
  input: Record<string, unknown>,
  key: string,
  min: number,
  max: number,
): number | undefined {
  const value = input[key];
  if (value === undefined) return undefined;
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value < min ||
    value > max
  )
    throw new CodingError(
      "invalid_arguments",
      `"${key}" must be an integer between ${min} and ${max}`,
    );
  return value;
}
