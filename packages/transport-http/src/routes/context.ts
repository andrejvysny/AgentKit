/**
 * What one route handler is handed. Assembled once by the dispatcher so no
 * handler re-parses the URL or re-derives the problem `instance`.
 */
import type { RestHandlerDeps } from "../deps.js";
import type { RouteParams } from "../router.js";

export interface RouteContext {
  deps: RestHandlerDeps;
  req: Request;
  url: URL;
  params: RouteParams;
  /**
   * Whatever `deps.authenticate` returned, or `undefined` when no host
   * authenticator is wired.
   *
   * Opaque here: no route in this package reads it — authorization has already
   * happened by the time a handler runs, through the host's own
   * `AuthorizationPort`. It is threaded anyway so a host reusing these handlers
   * has the principal where it would need it, rather than having to re-derive
   * it from the request.
   */
  principal?: unknown;
  /** The request path; every problem body reports it as `instance`. */
  instance: string;
}

export type RouteHandler = (ctx: RouteContext) => Promise<Response>;

/**
 * A path parameter the route table guarantees.
 *
 * The matcher only reports a route as matched when every `:param` in its
 * pattern captured a non-empty segment, so the fallback is unreachable — it
 * exists to satisfy `noUncheckedIndexedAccess` without an assertion that would
 * throw a 500 if the invariant ever broke. An empty id 404s instead.
 */
export function pathParam(ctx: RouteContext, name: string): string {
  return ctx.params[name] ?? "";
}
