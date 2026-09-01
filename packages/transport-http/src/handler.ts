/**
 * The handler: one `(Request) => Promise<Response>`, no framework anywhere.
 *
 * A fetch-standard function is the smallest thing every JavaScript server today
 * already accepts — `Bun.serve({ fetch })`, Deno, a Hono `app.all("*", …)`, a
 * Node adapter — so the adapter can be added to a host without that host
 * adopting this package's opinion about routing, middleware or lifecycle. It is
 * also, not incidentally, testable without a socket: every handler-level test in
 * this package calls this function with a `new Request(...)`.
 *
 * The dispatch table is keyed by {@link RestOperation}, which makes it
 * exhaustive: adding a route to `REST_ROUTES` in `@agentkit/contracts` breaks
 * this file's compile until it is served. That is the intended failure mode —
 * a contract route with no implementation should not be discoverable only by a
 * client's 404.
 */
import type { RestOperation } from "@agentkit/contracts";
import {
  actionForMethod,
  resourceForOperation,
  subjectForPrincipal,
} from "./authorize.js";
import {
  allowedOrigin,
  corsResponseHeaders,
  preflightResponse,
  withCorsHeaders,
} from "./cors.js";
import type { RestHandlerDeps } from "./deps.js";
import { forbidden, problemForError, problemResponse } from "./problem.js";
import { matchRoute, normalizeBasePath, stripBasePath } from "./router.js";
import type { RouteHandler } from "./routes/context.js";
import { activateBranch, forkChat, listSiblings } from "./routes/branches.js";
import {
  createChat,
  getChat,
  listChats,
  listMessages,
  submitMessage,
} from "./routes/chats.js";
import {
  getVersion,
  listModels,
  listProviders,
  listTools,
} from "./routes/meta.js";
import {
  applyProposal,
  approveProposal,
  listProposals,
  rejectProposal,
} from "./routes/proposals.js";
import { cancelRun, getRun, streamRun } from "./routes/runs.js";
import { listToolEvents } from "./routes/tool-events.js";

export type RestFetchHandler = (req: Request) => Promise<Response>;

const HANDLERS: Readonly<Record<RestOperation, RouteHandler>> = Object.freeze({
  createChat,
  listChats,
  getChat,
  listMessages,
  submitMessage,
  forkChat,
  activateBranch,
  listSiblings,
  getRun,
  streamRun,
  cancelRun,
  listToolEvents,
  listProposals,
  approveProposal,
  rejectProposal,
  applyProposal,
  listProviders,
  listModels,
  listTools,
  getVersion,
});

export function createRestHandler(deps: RestHandlerDeps): RestFetchHandler {
  const basePath = normalizeBasePath(deps.basePath);

  return async (req: Request): Promise<Response> => {
    const url = new URL(req.url);
    // The FULL request path, prefix included: `instance` is what the client saw
    // itself ask for, and reporting the stripped path would send whoever reads
    // the problem body looking for a URL nobody requested.
    const instance = url.pathname;

    const origin = allowedOrigin(req, deps.cors);
    // Computed once and applied to every exit below — including the ones that
    // return before any host code runs, and including the SSE stream. A CORS
    // policy that covered the happy path but not the 404 is a policy a browser
    // reports as a CORS error on the day something actually goes wrong.
    // With CORS configured, `Vary: Origin` goes on EVERY response — the
    // non-matching-origin one included: the body is origin-dependent either
    // way, and a shared cache that stored the headerless answer under a
    // Vary-less key would replay it to an allowed origin, whose browser would
    // then block a legitimate request.
    const cors =
      deps.cors === undefined
        ? null
        : origin === null
          ? { vary: "Origin" }
          : corsResponseHeaders(origin, deps.cors);
    const decorate = (res: Response): Response =>
      cors === null ? res : withCorsHeaders(res, cors);

    const routedPath = stripBasePath(url.pathname, basePath);
    if (routedPath === null) {
      return decorate(noRoute(req.method, url.pathname, instance));
    }

    // Routing first, and it touches nothing: matching is pure string work, so
    // an unauthenticated probe of a nonsense path costs a comparison and gets
    // an honest 404 instead of a 401 that hides whether the route exists at
    // all. Authentication runs before the first store read, which is the point
    // where a request starts costing something.
    const match = matchRoute(req.method, routedPath);

    // The preflight is answered off the route table's own `Allow` set, ahead of
    // the 405 that same match would otherwise become: a browser asking "may I
    // POST here?" with OPTIONS is not a client using the wrong verb. Gated on
    // CORS being configured AND the origin matching, so an OPTIONS request
    // otherwise falls through and 405s exactly as it did before this option
    // existed.
    if (
      origin !== null &&
      deps.cors !== undefined &&
      req.method === "OPTIONS"
    ) {
      if (match.kind === "not_found") {
        return decorate(noRoute(req.method, url.pathname, instance));
      }
      const allow =
        match.kind === "method_not_allowed" ? match.allow : [req.method];
      return preflightResponse(origin, allow, deps.cors);
    }

    if (match.kind === "not_found") {
      return decorate(noRoute(req.method, url.pathname, instance));
    }
    if (match.kind === "method_not_allowed") {
      const allow = [...new Set(match.allow)].join(", ");
      return decorate(
        problemResponse({
          status: 405,
          code: "method_not_allowed",
          detail: `${req.method} is not allowed on ${url.pathname}; allowed: ${allow}.`,
          instance,
          headers: { allow },
        }),
      );
    }

    try {
      let principal: unknown;
      if (deps.authenticate !== undefined) {
        const outcome = await deps.authenticate(req);
        // A Response is the host's own answer — 401, a redirect, a rate limit —
        // and its body and status are returned verbatim rather than re-wrapped,
        // because the host knows its scheme's error shape and this adapter does
        // not. The CORS headers are still added: without them a browser cannot
        // read the host's 401 either, and an auth failure a page cannot see is
        // worse than one it can.
        if (outcome instanceof Response) return decorate(outcome);
        principal = outcome;
      }

      const denied = await checkAuthorization(deps, match, req, principal);
      if (denied !== null) return decorate(denied(instance));

      return decorate(
        await HANDLERS[match.operation]({
          deps,
          req,
          url,
          params: match.params,
          principal,
          instance,
        }),
      );
    } catch (err) {
      return decorate(problemForError(err, instance, deps.logger));
    }
  };
}

/**
 * `null` when the request may proceed; otherwise a builder for the 403.
 *
 * It returns a builder rather than a `Response` so the caller stays the single
 * place that knows the problem `instance` — one path string, derived once.
 */
async function checkAuthorization(
  deps: RestHandlerDeps,
  match: { operation: RestOperation; params: Readonly<Record<string, string>> },
  req: Request,
  principal: unknown,
): Promise<((instance: string) => Response) | null> {
  if (deps.authorize === undefined) return null;
  const resource = resourceForOperation(match.operation, match.params);
  // `null` is `getVersion` and nothing else — see `authorize.ts`.
  if (resource === null) return null;

  const action = actionForMethod(req.method);
  const decision = await deps.authorize.authorize({
    subject: subjectForPrincipal(principal),
    action,
    resource,
  });
  if (decision.allowed) return null;

  const detail =
    decision.reason ?? `Not allowed to ${action} this ${resource.kind}.`;
  return (instance: string) => forbidden(detail, instance);
}

/** The 404 an unrouted path — or one outside `basePath` — answers with. */
function noRoute(method: string, pathname: string, instance: string): Response {
  return problemResponse({
    status: 404,
    code: "not_found",
    detail: `No route for ${method} ${pathname}.`,
    instance,
  });
}

/**
 * The handler, shaped as the object a server constructor takes:
 * `Bun.serve(serveRest(deps))`, `Deno.serve(serveRest(deps).fetch)`.
 *
 * It exists only so the common wiring is one call instead of two lines, and
 * deliberately returns nothing else — a server's port, TLS and lifecycle are
 * the host's business, and a transport package that started owning them would
 * stop being optional.
 */
export function serveRest(deps: RestHandlerDeps): { fetch: RestFetchHandler } {
  return { fetch: createRestHandler(deps) };
}
