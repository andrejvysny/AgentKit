/**
 * The catalogue routes: the tool set, and the version pair a client negotiates
 * against.
 *
 * Providers moved to `routes/providers.ts` when they grew from two reads into
 * CRUD plus two probe routes — a module that held the version constant and the
 * credential-handling path would be a module nobody could name.
 */
import {
  CONTRACT_VERSION,
  REST_API_VERSION,
  type ToolDefinitionDto,
  type VersionDto,
} from "@agentkit/contracts";
import { jsonResponse } from "../http.js";
import { notImplemented } from "../problem.js";
import type { RouteContext } from "./context.js";

/**
 * The tool catalogue — when the host can name one.
 *
 * `ToolSetContributor.contribute` is a per-RUN call: it takes the chat's
 * context bindings, the run's limits and its scope, and a host's contributor
 * legitimately returns different tools for different conversations. `GET
 * /v1/tools` names no conversation, so it asks the catalogue with NO scope,
 * which is the port's chat-independent question (no bindings, unbound rules) —
 * not a fabricated run context. A host that cannot answer it leaves
 * `deps.toolCatalog` out and the route reports 501 rather than lying.
 *
 * `ToolCatalogEntry.namespace` is dropped here rather than published: the DTO
 * this route serves is `ToolDefinitionDto`, and the namespace is a host-side
 * attribution, not part of the versioned wire contract.
 */
export async function listTools(ctx: RouteContext): Promise<Response> {
  const catalog = ctx.deps.toolCatalog;
  if (catalog === undefined) {
    return notImplemented(
      "This deployment exposes no chat-independent tool catalogue; tools are contributed per run.",
      ctx.instance,
    );
  }
  const entries = await catalog.listTools();
  const items: ToolDefinitionDto[] = entries.map((entry) => entry.definition);
  return jsonResponse(items);
}

/**
 * Both versions, because they move independently: `contractVersion` is the
 * DTO/event shape, `restApiVersion` the URL surface. A client that pins only
 * one of them cannot tell an additive DTO change from a route change.
 */
export async function getVersion(ctx: RouteContext): Promise<Response> {
  const body: VersionDto = {
    contractVersion: CONTRACT_VERSION,
    restApiVersion: REST_API_VERSION,
    ...(ctx.deps.packages === undefined ? {} : { packages: ctx.deps.packages }),
  };
  return jsonResponse(body);
}
