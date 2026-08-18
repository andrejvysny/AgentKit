import type { AiContextBinding } from "./bindings.js";

/**
 * Filter helpers for context bindings. Pure functions; storage is the adapter's concern.
 */

export function findPrimary(
  bindings: AiContextBinding[],
  kind?: AiContextBinding["kind"],
): AiContextBinding | undefined {
  return bindings.find(
    (b) =>
      b.role === "primary" &&
      b.status === "active" &&
      (kind === undefined || b.kind === kind),
  );
}

export function findByRefId(
  bindings: AiContextBinding[],
  refId: string,
): AiContextBinding | undefined {
  return bindings.find((b) => b.refId === refId);
}
