/** `err.message ?? err`, without assuming `err` is an Error at all. */
export function errorMessageOf(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "object" && err !== null && "message" in err) {
    const message = (err as { message: unknown }).message;
    if (message !== undefined && message !== null) return String(message);
  }
  return String(err);
}

/** DOMException/AbortError duck-typing (works across realms and polyfills). */
export function isAbortError(err: unknown): boolean {
  return (
    typeof err === "object" &&
    err !== null &&
    "name" in err &&
    (err as { name: unknown }).name === "AbortError"
  );
}
