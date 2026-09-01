import type { McpServerAuth } from "./types.js";

/** `Authorization: Bearer <token>`, scheme matched case-insensitively. */
const BEARER = /^Bearer[ \t]+(\S.*)$/i;

/**
 * Constant-time string equality.
 *
 * Both sides are SHA-256'd first, then compared byte-by-byte with an
 * accumulating XOR that never short-circuits. Hashing is not about secrecy
 * here — it is what makes the comparison safe to perform at all. A direct
 * comparison of the raw strings has to deal with unequal lengths somehow, and
 * every way of doing that leaks: an early `length !== length` return answers
 * "your guess is the wrong length", which is one bit, and it is the bit that
 * tells an attacker to stop guessing at that length. Digesting makes every
 * comparison exactly 32 bytes wide, so length carries no information and the
 * only remaining signal is the fixed-width compare.
 *
 * WEB CRYPTO, not `node:crypto`'s `timingSafeEqual`. Every other `@agentkit/*`
 * package's `src/` imports nothing from `node:*`, and the SDK transport this
 * package is built on is the web-standard one precisely so it runs wherever
 * `Request`/`Response` do. `crypto.subtle` is a global on Node ≥ 20, Bun, Deno
 * and Workers alike; `node:crypto` would narrow the package to two of them and
 * would need `@types/node` in a build that deliberately declares `types: []`.
 * The cost is that this is async — which the auth path already is.
 */
export async function timingSafeEqualString(
  a: string,
  b: string,
): Promise<boolean> {
  const [left, right] = await Promise.all([sha256(a), sha256(b)]);
  // Both operands are SHA-256 digests, so this loop's length is a constant and
  // the seed below is always 0. It is written this way rather than assumed so
  // the function stays correct if it is ever handed something else.
  let diff = left.length ^ right.length;
  for (let index = 0; index < left.length; index += 1) {
    diff |= (left[index] ?? 0) ^ (right[index] ?? 0);
  }
  return diff === 0;
}

async function sha256(value: string): Promise<Uint8Array> {
  const bytes = new TextEncoder().encode(value);
  return new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
}

/**
 * Parse an `Authorization` header and compare its bearer token to `expected`.
 *
 * A missing header, a different scheme, or an empty token is a refusal — and so
 * is a token that parsed fine but does not match, with no way for the caller to
 * tell those apart. That is the point: the 401 this feeds carries no body.
 */
export async function verifyBearer(
  authorizationHeader: string | null,
  expected: string,
): Promise<boolean> {
  if (authorizationHeader === null) return false;
  const match = BEARER.exec(authorizationHeader.trim());
  if (!match?.[1]) return false;
  return timingSafeEqualString(match[1], expected);
}

/**
 * Normalize the two {@link McpServerAuth} shapes into one predicate.
 *
 * An empty or whitespace-only `bearerToken` is refused HERE, at wiring time,
 * rather than at the first request: a host that read its token out of an unset
 * environment variable has configured a server whose "secret" is the empty
 * string, and finding that out from a 401 that never happens is finding it out
 * too late.
 */
export function resolveAuth(
  auth: McpServerAuth,
): (authorizationHeader: string | null) => boolean | Promise<boolean> {
  if ("bearerToken" in auth) {
    const expected = auth.bearerToken;
    if (typeof expected !== "string" || expected.trim() === "") {
      throw new Error(
        "createMcpServerHandler: auth.bearerToken is empty. An MCP server that " +
          "executes host tools must not be reachable without a real token.",
      );
    }
    return (header) => verifyBearer(header, expected);
  }
  if (typeof auth.verify !== "function") {
    throw new Error(
      "createMcpServerHandler: auth must be { bearerToken } or { verify }.",
    );
  }
  return (header) => auth.verify(header);
}

/**
 * The fingerprint an MCP session is bound to: SHA-256 of the RAW
 * `Authorization` header, rendered as hex — of the empty string when the
 * request carries no header at all.
 *
 * The RAW header, not the parsed token, because {@link McpServerAuth.verify}
 * hosts may put anything in there (a JWT, a scheme this package does not
 * know), and the binding must be to whatever the caller actually proved with.
 * A digest rather than the header itself so a live session map is not a place
 * credentials sit in memory for the session's whole life.
 *
 * Compare two of these with {@link timingSafeEqualString} — never with `===`.
 */
export async function authFingerprint(
  authorizationHeader: string | null,
): Promise<string> {
  const digest = await sha256(authorizationHeader ?? "");
  return Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join(
    "",
  );
}
