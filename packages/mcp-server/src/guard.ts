/**
 * DNS-rebinding defense for a locally-bound MCP server.
 *
 * The attack this exists for: a page on `evil.com` whose DNS is re-pointed at
 * `127.0.0.1` after the page loads, so the browser sends same-origin requests to
 * a server that assumed "bound to loopback" meant "only reachable by this
 * machine's own software". The browser still sets `Host: evil.com` (it is
 * derived from the URL, not from the resolved address), which is why the HOST
 * check is the load-bearing one and is on by default. The `Origin` check is the
 * secondary one and is opt-in — the ordinary MCP client is a native process that
 * sends no `Origin` at all, so a default that required one would refuse every
 * real client to defend against a browser that is not there.
 */

interface HostPort {
  /** Lowercased; IPv6 keeps its brackets, so `[::1]` compares as written. */
  hostname: string;
  port?: string;
}

/**
 * Split a `Host`-header-shaped value into hostname and optional port.
 *
 * Returns `null` for anything malformed — an unterminated bracket, a bare IPv6
 * literal with no brackets, junk after the bracket. A `null` is treated as a
 * refusal by the caller rather than "no port": a value this cannot parse is a
 * value it cannot honestly compare, and guessing is how a guard gets walked
 * past.
 */
export function splitHostPort(value: string): HostPort | null {
  const raw = value.trim();
  if (raw === "") return null;
  if (raw.startsWith("[")) {
    const end = raw.indexOf("]");
    if (end < 0) return null;
    const hostname = raw.slice(0, end + 1).toLowerCase();
    const rest = raw.slice(end + 1);
    if (rest === "") return { hostname };
    if (!rest.startsWith(":") || rest.length === 1) return null;
    return { hostname, port: rest.slice(1) };
  }
  const colon = raw.indexOf(":");
  if (colon < 0) return { hostname: raw.toLowerCase() };
  // A second colon in an unbracketed value means a bare IPv6 literal, which is
  // not legal in a Host header — refuse rather than truncate it to garbage.
  if (raw.indexOf(":", colon + 1) >= 0) return null;
  if (colon === 0 || colon === raw.length - 1) return null;
  return {
    hostname: raw.slice(0, colon).toLowerCase(),
    port: raw.slice(colon + 1),
  };
}

/**
 * Is `hostHeader` one of `allowed`?
 *
 * An allowed entry WITHOUT a port matches that hostname on any port (a desktop
 * host picks its port at boot, and making the operator restate it in two places
 * is how the two drift). An entry WITH a port must match exactly.
 */
export function isHostAllowed(
  hostHeader: string | null,
  allowed: readonly string[],
): boolean {
  if (hostHeader === null) return false;
  const actual = splitHostPort(hostHeader);
  if (actual === null) return false;
  for (const entry of allowed) {
    const expected = splitHostPort(entry);
    if (expected === null) continue;
    if (expected.hostname !== actual.hostname) continue;
    if (expected.port === undefined) return true;
    if (expected.port === actual.port) return true;
  }
  return false;
}

/**
 * Is `originHeader` one of `allowed`?
 *
 * Compared as a parsed `URL.origin`, not as a string, so `http://localhost:8787`
 * and `http://localhost:8787/` are the same answer and a trailing-slash typo in
 * a config is not a silent lockout. No wildcards: an origin list with a `*` in
 * it is a list that stopped being a list.
 */
export function isOriginAllowed(
  originHeader: string | null,
  allowed: readonly string[],
): boolean {
  if (originHeader === null) return false;
  const actual = normalizeOrigin(originHeader);
  if (actual === null) return false;
  return allowed.some((entry) => normalizeOrigin(entry) === actual);
}

function normalizeOrigin(value: string): string | null {
  const raw = value.trim();
  if (raw === "" || raw === "null") return null;
  try {
    return new URL(raw).origin.toLowerCase();
  } catch {
    return null;
  }
}

export interface RebindingGuardOptions {
  allowedHosts: readonly string[];
  /** `undefined` means "do not check `Origin`" — see the module comment. */
  allowedOrigins?: readonly string[];
}

/**
 * The whole guard, as one call: `null` when the request may proceed, or the
 * reason it may not (for the log — the response body says nothing).
 */
export function checkRebindingGuard(
  headers: Headers,
  options: RebindingGuardOptions,
): string | null {
  const host = headers.get("host");
  if (!isHostAllowed(host, options.allowedHosts)) {
    return `Host header ${JSON.stringify(host ?? "")} is not allowed.`;
  }
  const origins = options.allowedOrigins;
  if (origins === undefined) return null;
  const origin = headers.get("origin");
  // No Origin is not a claim to check. Native MCP clients send none.
  if (origin === null) return null;
  if (!isOriginAllowed(origin, origins)) {
    return `Origin ${JSON.stringify(origin)} is not allowed.`;
  }
  return null;
}
