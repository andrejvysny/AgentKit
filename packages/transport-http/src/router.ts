/**
 * The router, compiled from {@link REST_ROUTES} rather than transcribed from it.
 *
 * The route table in `@agentkit/contracts` is data precisely so an adapter, a
 * client generator and a docs page cannot drift apart by someone retyping a
 * path. Hand-writing `"/v1/chats/:chatId/messages"` here a second time would
 * throw that away on the first day someone renames a segment: the contract
 * would say one thing, this server another, and both would compile.
 *
 * Matching is exact-segment with `:param` capture. No wildcards, no optional
 * segments, no regex per route — the contract has none of those, and a matcher
 * that supports more than the table needs is a matcher with untested branches.
 */
import {
  REST_ROUTES,
  type RestMethod,
  type RestOperation,
} from "@agentkit/contracts";

interface CompiledRoute {
  operation: RestOperation;
  method: RestMethod;
  /** Path segments; a `:name` entry captures instead of comparing. */
  segments: readonly string[];
}

function compile(): readonly CompiledRoute[] {
  return Object.entries(REST_ROUTES).map(([operation, def]) => ({
    operation: operation as RestOperation,
    method: def.method,
    segments: splitPath(def.path),
  }));
}

/** `/v1/chats/:chatId` → `["v1", "chats", ":chatId"]`. */
function splitPath(path: string): string[] {
  return path.split("/").filter((segment) => segment !== "");
}

const ROUTES = compile();

export type RouteParams = Readonly<Record<string, string>>;

export type RouteMatch =
  | { kind: "matched"; operation: RestOperation; params: RouteParams }
  /** The path exists; this method does not. `allow` is the `Allow` header. */
  | { kind: "method_not_allowed"; allow: readonly RestMethod[] }
  | { kind: "not_found" };

/**
 * Resolve one request to an operation.
 *
 * The path is matched before the method so a wrong verb on a real path answers
 * 405 with the truth about what the path DOES accept, instead of 404 sending a
 * client to check its spelling. `%2F` in a path parameter is decoded here —
 * ids are opaque strings and one of them containing a slash must not silently
 * become two segments.
 */
export function matchRoute(method: string, pathname: string): RouteMatch {
  const segments = splitPath(pathname);
  const pathMatches: CompiledRoute[] = [];
  for (const route of ROUTES) {
    const params = matchSegments(route.segments, segments);
    if (params === null) continue;
    pathMatches.push(route);
    if (route.method === method) {
      return { kind: "matched", operation: route.operation, params };
    }
  }
  if (pathMatches.length === 0) return { kind: "not_found" };
  return {
    kind: "method_not_allowed",
    allow: pathMatches.map((route) => route.method),
  };
}

function matchSegments(
  pattern: readonly string[],
  actual: readonly string[],
): RouteParams | null {
  if (pattern.length !== actual.length) return null;
  const params: Record<string, string> = {};
  for (let i = 0; i < pattern.length; i += 1) {
    const expected = pattern[i]!;
    const got = actual[i]!;
    if (expected.startsWith(":")) {
      if (got === "") return null;
      params[expected.slice(1)] = safeDecode(got);
      continue;
    }
    if (expected !== got) return null;
  }
  return params;
}

/**
 * A `basePath` option in the one form the rest of this package uses: either the
 * empty string (no prefix) or a leading slash with no trailing one.
 *
 * `""`, `"/"`, and whitespace all normalize to "no prefix" rather than to a
 * prefix that matches nothing — those are the three ways a config file spells
 * "unset", and a handler that answered 404 to every request because
 * `BASE_PATH=""` was exported is a fault that looks like a routing bug.
 */
export function normalizeBasePath(basePath: string | undefined): string {
  if (basePath === undefined) return "";
  const trimmed = basePath.trim().replace(/\/+$/, "");
  if (trimmed === "") return "";
  return trimmed.startsWith("/") ? trimmed : `/${trimmed}`;
}

/**
 * The path to route on, with the mount prefix removed — or `null` when the
 * request is not under the prefix at all.
 *
 * `null` is the caller's cue to answer the ordinary 404: a request outside the
 * mount is a request for a route this handler does not serve, which is exactly
 * what "no route" means. `/api` with a base of `/api` strips to `/`, so the
 * mount root itself 404s like any other unrouted path rather than matching the
 * first route by accident.
 */
export function stripBasePath(
  pathname: string,
  basePath: string,
): string | null {
  if (basePath === "") return pathname;
  if (pathname === basePath) return "/";
  return pathname.startsWith(`${basePath}/`)
    ? pathname.slice(basePath.length)
    : null;
}

/** A malformed percent-escape is passed through rather than thrown on. */
function safeDecode(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}
