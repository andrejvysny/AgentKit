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
import type { RestHandlerDeps } from "./deps.js";
import { problemForError, problemResponse } from "./problem.js";
import { matchRoute } from "./router.js";
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
  return async (req: Request): Promise<Response> => {
    const url = new URL(req.url);
    const instance = url.pathname;

    // Routing first, and it touches nothing: matching is pure string work, so
    // an unauthenticated probe of a nonsense path costs a comparison and gets
    // an honest 404 instead of a 401 that hides whether the route exists at
    // all. Authentication runs before the first store read, which is the point
    // where a request starts costing something.
    const match = matchRoute(req.method, url.pathname);
    if (match.kind === "not_found") {
      return problemResponse({
        status: 404,
        code: "not_found",
        detail: `No route for ${req.method} ${url.pathname}.`,
        instance,
      });
    }
    if (match.kind === "method_not_allowed") {
      const allow = [...new Set(match.allow)].join(", ");
      return problemResponse({
        status: 405,
        code: "method_not_allowed",
        detail: `${req.method} is not allowed on ${url.pathname}; allowed: ${allow}.`,
        instance,
        headers: { allow },
      });
    }

    try {
      if (deps.authenticate !== undefined) {
        const outcome = await deps.authenticate(req);
        // A Response is the host's own answer — 401, a redirect, a rate limit —
        // and is returned verbatim rather than re-wrapped, because the host
        // knows its scheme's error shape and this adapter does not.
        if (outcome instanceof Response) return outcome;
      }
      return await HANDLERS[match.operation]({
        deps,
        req,
        url,
        params: match.params,
        instance,
      });
    } catch (err) {
      return problemForError(err, instance, deps.logger);
    }
  };
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
