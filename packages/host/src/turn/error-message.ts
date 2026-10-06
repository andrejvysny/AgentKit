import { redactDiagnostic } from "@agentkit/core";

const MAX_PUBLIC_ERROR_BYTES = 4096;

/** Bound persisted diagnostics without splitting a UTF-8 code point. */
export function publicErrorMessage(error: unknown): string {
  const message = redactDiagnostic(
    error instanceof Error ? error.message : String(error),
  );
  const encoded = new TextEncoder().encode(message);
  if (encoded.byteLength <= MAX_PUBLIC_ERROR_BYTES) return message;
  let end = MAX_PUBLIC_ERROR_BYTES - 3;
  while ((encoded[end]! & 0xc0) === 0x80) end--;
  return `${new TextDecoder().decode(encoded.subarray(0, end))}...`;
}
