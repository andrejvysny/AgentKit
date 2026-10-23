import type { ConnectionRecord, PromptTemplate } from "@agentkit/contracts";
import { jsonResponse, readJsonObject } from "../http.js";
import { badRequest, notImplemented } from "../problem.js";
import { pathParam, type RouteContext } from "./context.js";
import { validateCreateMcpServerRequest } from "../validate.js";
import { mcpServerDto } from "../projections.js";

const unavailable = (ctx: RouteContext) =>
  notImplemented("Managed content service is not configured.", ctx.instance);
function publicConnection(record: ConnectionRecord) {
  const config = mcpServerDto({
    ...record.config,
    id: record.id,
    createdAt: "",
    updatedAt: "",
  });
  return {
    ...record,
    config: { ...record.config, transport: config.transport },
  };
}
export async function listConnections(ctx: RouteContext) {
  if (!ctx.deps.connections) return unavailable(ctx);
  return jsonResponse(
    (await ctx.deps.connections.list()).map(publicConnection),
  );
}
export async function saveConnection(ctx: RouteContext) {
  const service = ctx.deps.connections;
  if (!service) return unavailable(ctx);
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const value = body.value;
  const config = value["config"];
  if (!config || typeof config !== "object" || Array.isArray(config))
    return badRequest(
      "invalid_request",
      "config must be an object.",
      ctx.instance,
    );
  const checked = validateCreateMcpServerRequest(
    config as Record<string, unknown>,
  );
  if (!checked.ok)
    return badRequest("invalid_request", checked.detail, ctx.instance);
  if (
    typeof value["connectorId"] !== "string" ||
    typeof value["accountId"] !== "string" ||
    (value["workspaceId"] !== undefined &&
      typeof value["workspaceId"] !== "string") ||
    !Number.isSafeInteger(value["revision"]) ||
    (value["revision"] as number) < 1 ||
    (value["expectedRevision"] !== null &&
      (!Number.isSafeInteger(value["expectedRevision"]) ||
        (value["expectedRevision"] as number) < 1))
  )
    return badRequest(
      "invalid_request",
      "Invalid connection identity or revision.",
      ctx.instance,
    );
  const record = {
    id: pathParam(ctx, "connectionId"),
    connectorId: value["connectorId"],
    accountId: value["accountId"],
    workspaceId: value["workspaceId"],
    revision: value["revision"],
    config: checked.value,
  } as Omit<ConnectionRecord, "status" | "errorCode">;
  return jsonResponse(
    publicConnection(
      await service.save(record, value["expectedRevision"] as number | null),
    ),
  );
}
export async function deleteConnection(ctx: RouteContext) {
  if (!ctx.deps.connections) return unavailable(ctx);
  await ctx.deps.connections.remove(pathParam(ctx, "connectionId"));
  return new Response(null, { status: 204 });
}
export async function connectionAction(ctx: RouteContext) {
  const service = ctx.deps.connections;
  if (!service) return unavailable(ctx);
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const id = pathParam(ctx, "connectionId");
  const action = body.value["action"];
  if (action === "connect" || action === "reconnect")
    return jsonResponse(publicConnection(await service[action](id)));
  if (action === "disconnect") {
    await service.disconnect(id);
    return new Response(null, { status: 204 });
  }
  if (action === "test") {
    await service.test(id);
    return jsonResponse({ ok: true });
  }
  return badRequest(
    "invalid_request",
    "Unknown connection action.",
    ctx.instance,
  );
}
export async function listPromptTemplates(ctx: RouteContext) {
  if (!ctx.deps.promptTemplates) return unavailable(ctx);
  return jsonResponse(await ctx.deps.promptTemplates.list());
}
export async function publishPromptTemplate(ctx: RouteContext) {
  if (!ctx.deps.promptTemplates) return unavailable(ctx);
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const template = body.value;
  if (
    typeof template["id"] !== "string" ||
    typeof template["revision"] !== "string" ||
    typeof template["description"] !== "string" ||
    !template["argumentsSchema"] ||
    typeof template["argumentsSchema"] !== "object" ||
    !Array.isArray(template["messages"])
  )
    return badRequest(
      "invalid_request",
      "Invalid prompt template.",
      ctx.instance,
    );
  try {
    return jsonResponse(
      await ctx.deps.promptTemplates.publish(
        template as unknown as PromptTemplate,
      ),
      201,
    );
  } catch {
    return badRequest(
      "invalid_template",
      "Template is invalid or its revision already exists.",
      ctx.instance,
    );
  }
}
export async function deletePromptTemplate(ctx: RouteContext) {
  if (!ctx.deps.promptTemplates) return unavailable(ctx);
  await ctx.deps.promptTemplates.remove(
    pathParam(ctx, "templateId"),
    pathParam(ctx, "revision"),
  );
  return new Response(null, { status: 204 });
}
export async function renderPromptTemplate(ctx: RouteContext) {
  if (!ctx.deps.promptTemplates) return unavailable(ctx);
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  try {
    return jsonResponse(
      await ctx.deps.promptTemplates.render({
        id: pathParam(ctx, "templateId"),
        revision: pathParam(ctx, "revision"),
        arguments: body.value,
      }),
    );
  } catch {
    return badRequest(
      "invalid_template_arguments",
      "Template revision or arguments are invalid.",
      ctx.instance,
    );
  }
}
export async function listSkills(ctx: RouteContext) {
  if (!ctx.deps.skills) return unavailable(ctx);
  return jsonResponse(await ctx.deps.skills.catalog(ctx.req.signal));
}
