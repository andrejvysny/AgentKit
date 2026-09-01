/**
 * MCP server-config CRUD, over an `McpServerConfigStore` this package knows
 * only structurally (see {@link McpServerConfigOperations}).
 *
 * NO SECRET MATERIAL CROSSES THESE ROUTES, in either direction — not because
 * anything is filtered out, but because the record carries none: `secretRefs`
 * maps a `${placeholder}` token to a `SecretStore` REF, and the value behind
 * the ref is injected into an env var or a header at connect time and stored
 * nowhere. That is why the map can be published whole while a provider's
 * `apiKey` cannot.
 *
 * **501 without `deps.mcpConfigs`.** A host that declares its MCP servers in a
 * config file has nothing for these routes to write to.
 */
import type { McpServerDto } from "@agentkit/contracts";
import { jsonResponse, readJsonObject } from "../http.js";
import { badRequest, conflict, notFound, notImplemented } from "../problem.js";
import { mcpServerDto } from "../projections.js";
import { pathParam, type RouteContext } from "./context.js";
import type { McpServerConfigOperations } from "../deps.js";
import {
  validateCreateMcpServerRequest,
  validateUpdateMcpServerRequest,
} from "../validate.js";

/** The store, or the 501 that says this deployment has none. */
function storeOf(
  ctx: RouteContext,
):
  | { ok: true; store: McpServerConfigOperations }
  | { ok: false; response: Response } {
  const store = ctx.deps.mcpConfigs;
  if (store === undefined) {
    return {
      ok: false,
      response: notImplemented(
        "This deployment does not manage MCP server configs; no McpServerConfigStore is wired.",
        ctx.instance,
      ),
    };
  }
  return { ok: true, store };
}

/**
 * The 409 a taken alias answers with, or `null` when the alias is free.
 *
 * Checked HERE, ahead of the store, even though every implementation of the
 * port enforces the same uniqueness. The store's refusal is an `McpError` from
 * a package this adapter deliberately does not import, so it would arrive as an
 * unrecognized throw and become a 500 — an honest 409 naming the alias is what
 * a settings form can act on. The store's own check remains the backstop for
 * the race between this read and the write.
 */
async function aliasTaken(
  store: McpServerConfigOperations,
  alias: string,
  exceptId: string | null,
): Promise<boolean> {
  const existing = await store.list();
  return existing.some(
    (record) => record.alias === alias && record.id !== exceptId,
  );
}

export async function listMcpServers(ctx: RouteContext): Promise<Response> {
  const resolved = storeOf(ctx);
  if (!resolved.ok) return resolved.response;
  const items: McpServerDto[] = (await resolved.store.list()).map(mcpServerDto);
  return jsonResponse(items);
}

export async function createMcpServer(ctx: RouteContext): Promise<Response> {
  const resolved = storeOf(ctx);
  if (!resolved.ok) return resolved.response;
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateCreateMcpServerRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const request = validated.value;

  if (await aliasTaken(resolved.store, request.alias, null)) {
    return conflict(
      "duplicate_alias",
      `An MCP server with alias ${request.alias} already exists; aliases are the tool namespace and must be unique.`,
      ctx.instance,
    );
  }

  // The server mints identity and both timestamps — the store takes the record
  // verbatim so an importer can preserve ids, and this route is not an import.
  const now = new Date().toISOString();
  const created = await resolved.store.create({
    id: `mcp_${crypto.randomUUID()}`,
    alias: request.alias,
    transport: request.transport,
    ...(request.secretRefs === undefined
      ? {}
      : { secretRefs: request.secretRefs }),
    ...(request.enabled === undefined ? {} : { enabled: request.enabled }),
    ...(request.toolAliases === undefined
      ? {}
      : { toolAliases: request.toolAliases }),
    ...(request.resilience === undefined
      ? {}
      : { resilience: request.resilience }),
    createdAt: now,
    updatedAt: now,
  });
  return jsonResponse(mcpServerDto(created), 201);
}

/**
 * Patch one. FIELD-LEVEL REPLACE — a present `secretRefs` or `toolAliases`
 * replaces the stored bag wholesale, because a merge makes "remove this entry"
 * unexpressible.
 */
export async function updateMcpServer(ctx: RouteContext): Promise<Response> {
  const resolved = storeOf(ctx);
  if (!resolved.ok) return resolved.response;
  const serverId = pathParam(ctx, "serverId");
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateUpdateMcpServerRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const patch = validated.value;

  // Read before write: the store raises its own typed not-found, but it does so
  // as an `McpError` this package cannot recognize — so the 404 is decided here
  // where it can be an honest one.
  if ((await resolved.store.get(serverId)) === null) {
    return notFound(`MCP server not found: ${serverId}`, ctx.instance);
  }
  if (
    patch.alias !== undefined &&
    (await aliasTaken(resolved.store, patch.alias, serverId))
  ) {
    return conflict(
      "duplicate_alias",
      `An MCP server with alias ${patch.alias} already exists; aliases are the tool namespace and must be unique.`,
      ctx.instance,
    );
  }

  const updated = await resolved.store.update(serverId, {
    ...(patch.alias === undefined ? {} : { alias: patch.alias }),
    ...(patch.transport === undefined ? {} : { transport: patch.transport }),
    ...(patch.secretRefs === undefined ? {} : { secretRefs: patch.secretRefs }),
    ...(patch.enabled === undefined ? {} : { enabled: patch.enabled }),
    ...(patch.toolAliases === undefined
      ? {}
      : { toolAliases: patch.toolAliases }),
    ...(patch.resilience === undefined ? {} : { resilience: patch.resilience }),
  });
  return jsonResponse(mcpServerDto(updated));
}

/** 204. An unknown id is a 404 — a delete a caller cannot confirm is not one. */
export async function deleteMcpServer(ctx: RouteContext): Promise<Response> {
  const resolved = storeOf(ctx);
  if (!resolved.ok) return resolved.response;
  const serverId = pathParam(ctx, "serverId");
  if ((await resolved.store.get(serverId)) === null) {
    return notFound(`MCP server not found: ${serverId}`, ctx.instance);
  }
  await resolved.store.delete(serverId);
  return new Response(null, { status: 204 });
}
