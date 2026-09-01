/**
 * The single settings row: read it, patch it.
 *
 * No optional dependency and no 501 — `SettingsStore` is part of the
 * `AssistantStore` aggregate every deployment already wires, and the row always
 * exists (an adapter seeds it).
 */
import type { AssistantSettings } from "@agentkit/host";
import { jsonResponse, readJsonObject } from "../http.js";
import { badRequest } from "../problem.js";
import { settingsDto } from "../projections.js";
import type { RouteContext } from "./context.js";
import { validateUpdateSettingsRequest } from "../validate.js";

export async function getSettings(ctx: RouteContext): Promise<Response> {
  return jsonResponse(settingsDto(await ctx.deps.store.settings.getSettings()));
}

/**
 * A PARTIAL patch, answering with the row as it now stands.
 *
 * Fields the request does not mention are left alone — the store's own rule —
 * and `metadata` REPLACES rather than merging, like every other metadata bag in
 * this contract. The three closed unions (`contextSizePreference`,
 * `writePolicyMode`, `toolCalling`) are validated rather than passed through:
 * they are the fields whose wrong value changes behaviour silently, and a
 * `writePolicyMode` nothing matches would fall through to confirming every
 * write forever with nothing in a log to say why.
 */
export async function updateSettings(ctx: RouteContext): Promise<Response> {
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateUpdateSettingsRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const patch = validated.value;
  // Spelled out field by field rather than spread wholesale: the request is a
  // client's JSON, and handing it to the store as a `Partial<AssistantSettings>`
  // would let an unknown member from a later contract version through into a
  // record the host reads by key.
  const applied: Partial<AssistantSettings> = {
    ...(patch.defaultProviderId === undefined
      ? {}
      : { defaultProviderId: patch.defaultProviderId }),
    ...(patch.defaultModel === undefined
      ? {}
      : { defaultModel: patch.defaultModel }),
    ...(patch.contextSizePreference === undefined
      ? {}
      : { contextSizePreference: patch.contextSizePreference }),
    ...(patch.writePolicyMode === undefined
      ? {}
      : { writePolicyMode: patch.writePolicyMode }),
    ...(patch.allowRawToolData === undefined
      ? {}
      : { allowRawToolData: patch.allowRawToolData }),
    ...(patch.maxToolIterations === undefined
      ? {}
      : { maxToolIterations: patch.maxToolIterations }),
    ...(patch.toolCalling === undefined
      ? {}
      : { toolCalling: patch.toolCalling }),
    ...(patch.metadata === undefined ? {} : { metadata: patch.metadata }),
  };
  const settings = await ctx.deps.store.settings.updateSettings(applied);
  return jsonResponse(settingsDto(settings));
}
