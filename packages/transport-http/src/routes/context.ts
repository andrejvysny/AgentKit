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
