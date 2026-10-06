import { truncateString } from "../tools/limits.js";

const REDACTED = "[REDACTED]";
const CREDENTIAL_VALUE =
  /(["']?\b(?:authorization|api[_-]?key|access[_-]?token|refresh[_-]?token|auth[_-]?token|client[_-]?secret|secret|password|token)["']?\s*[:=]\s*)("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|(?:Bearer|Basic)\s+[^\s"',;}\]]+|[^\s,;}\]]+)/gi;
const BEARER_VALUE = /\bBearer\s+[^\s"',;}\]]+/gi;

/** Diagnostics may echo request headers or provider credentials. */
export function redactDiagnostic(
  message: string,
  secrets: readonly string[] = [],
): string {
  let redacted = message
    .replace(CREDENTIAL_VALUE, (_match, prefix: string, value: string) => {
      const quote = value[0] === '"' || value[0] === "'" ? value[0] : "";
      return `${prefix}${quote}${REDACTED}${quote}`;
    })
    .replace(BEARER_VALUE, `Bearer ${REDACTED}`);
  for (const secret of secrets) {
    if (!secret) continue;
    for (const representation of new Set([
      secret,
      JSON.stringify(secret).slice(1, -1),
    ]))
      redacted = redacted.split(representation).join(REDACTED);
  }
  return redacted;
}

export function providerDiagnostic(
  error: unknown,
  secrets: readonly string[] = [],
): string {
  const message = error instanceof Error ? error.message : String(error);
  return truncateString(redactDiagnostic(message, secrets), 4096).value;
}
