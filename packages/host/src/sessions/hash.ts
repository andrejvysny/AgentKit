/** Hex SHA-256 of a string; the digest `AgentInputStore` compares for idempotency. */
export async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(text),
  );
  return Array.from(new Uint8Array(digest), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

/** JSON with object keys sorted by code unit, so equal values hash equally. */
export function canonicalJson(value: unknown): string {
  const sorted = (node: unknown): unknown => {
    if (Array.isArray(node)) return node.map(sorted);
    if (typeof node !== "object" || node === null) return node;
    return Object.fromEntries(
      Object.entries(node)
        .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
        .map(([key, entry]) => [key, sorted(entry)]),
    );
  };
  return JSON.stringify(sorted(value));
}

/** The digest an approval is bound to: sha-256 of the call's canonical arguments. */
export function argsSha256Of(args: unknown): Promise<string> {
  return sha256Hex(canonicalJson(args));
}
