/**
 * Provider CRUD, plus the two routes that have to talk to the provider itself.
 *
 * THE CREDENTIAL NEVER LANDS IN THE PROVIDER RECORD. `apiKey` is a write-only
 * request field: it goes to the host's `SecretStore` under a ref, and the REF
 * is what `AiProviderConfig.metadata` carries (under the same key `TurnRunner`
 * resolves it by). The alternative — `AiProviderConfig.apiKey`, which the type
 * still allows — would put a live key in the row that `listProviders` reads,
 * that a backup copies, and that any future projection could publish by
 * forgetting one field.
 */
import type { AiProviderConfig } from "@agentkit/contracts";
import type {
  ProviderDto,
  TestProviderResponse,
  UpdateProviderRequest,
} from "@agentkit/contracts";
import { PROVIDER_SECRET_REF_KEY, type SecretStore } from "@agentkit/host";
import { jsonResponse, readJsonObject } from "../http.js";
import {
  badRequest,
  conflict,
  notFound,
  notImplemented,
  problemResponse,
} from "../problem.js";
import { providerDto } from "../projections.js";
import { pathParam, type RouteContext } from "./context.js";
import {
  validateCreateProviderRequest,
  validateUpdateProviderRequest,
  providerIdIssue,
} from "../validate.js";

/**
 * The `SecretStore` ref a provider's key is filed under.
 *
 * DERIVED FROM THE PROVIDER ID, not random, and the determinism is the point:
 * updating a provider's key overwrites the one secret that belongs to it
 * instead of orphaning the old value under a ref nothing names any more, and
 * deleting the provider can find the secret to delete. A ref is a NAME, not a
 * capability — knowing it grants nothing, because reading the value needs the
 * store.
 */
export function providerSecretRef(providerId: string): string {
  return `provider/${providerId}/api-key`;
}

export async function listProviders(ctx: RouteContext): Promise<Response> {
  const configs = await ctx.deps.store.providers.listProviders();
  const items: ProviderDto[] = configs.map(providerDto);
  return jsonResponse(items);
}

export async function createProvider(ctx: RouteContext): Promise<Response> {
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateCreateProviderRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }
  const request = validated.value;

  const providerId =
    request.id !== undefined && request.id.trim() !== ""
      ? request.id.trim()
      : `prov_${crypto.randomUUID()}`;
  if (await ctx.deps.store.providers.getProvider(providerId)) {
    // A conflict, not an overwrite: the store's `upsertProvider` would happily
    // replace the row, and a create that silently replaced someone else's
    // provider is the one outcome nobody asked for.
    return conflict(
      "duplicate_provider",
      `A provider with id ${providerId} already exists.`,
      ctx.instance,
    );
  }

  const secret = planApiKey(ctx, providerId, request.apiKey);
  if (secret.kind === "unavailable") return secret.response;

  const config: AiProviderConfig = {
    id: providerId,
    label: request.label,
    kind: request.kind,
    baseUrl: request.baseUrl,
    defaultModel: request.defaultModel,
    enabled: request.enabled ?? true,
    ...(request.extraHeaders === undefined
      ? {}
      : { extraHeaders: request.extraHeaders }),
    metadata: withSecretRef(request.metadata, secret.ref),
  };
  const stored = await ctx.deps.store.providers.upsertProvider(config);
  const written = await writeApiKey(ctx, secret);
  if (written !== null) return written;
  return jsonResponse(providerDto(stored), 201);
}

export async function updateProvider(ctx: RouteContext): Promise<Response> {
  const providerId = pathParam(ctx, "providerId");
  const body = await readJsonObject(ctx.req, ctx.instance);
  if (!body.ok) return body.response;
  const validated = validateUpdateProviderRequest(body.value);
  if (!validated.ok) {
    return badRequest("invalid_request", validated.detail, ctx.instance);
  }

  const existing = await ctx.deps.store.providers.getProvider(providerId);
  if (existing === null) {
    return notFound(`Provider not found: ${providerId}`, ctx.instance);
  }

  const secret = planApiKey(ctx, providerId, validated.value.apiKey);
  if (secret.kind === "unavailable") return secret.response;

  const stored = await ctx.deps.store.providers.upsertProvider(
    mergeProvider(existing, validated.value, secret.ref),
  );
  const written = await writeApiKey(ctx, secret);
  if (written !== null) return written;
  return jsonResponse(providerDto(stored));
}

/**
 * 204, and the stored credential goes with it.
 *
 * Deleting the provider without deleting its secret would leave a live API key
 * in the secret store under a ref nothing points at any more — unreachable
 * through this API and impossible to notice, which is the worst combination a
 * credential can have. Best-effort: the secret store is asked only when one is
 * wired and the config actually named a ref.
 */
export async function deleteProvider(ctx: RouteContext): Promise<Response> {
  const providerId = pathParam(ctx, "providerId");
  const existing = await ctx.deps.store.providers.getProvider(providerId);
  if (existing === null) {
    return notFound(`Provider not found: ${providerId}`, ctx.instance);
  }
  const ref = existing.metadata?.[PROVIDER_SECRET_REF_KEY];
  await ctx.deps.store.providers.deleteProvider(providerId);
  if (ctx.deps.secrets !== undefined && typeof ref === "string" && ref !== "") {
    await ctx.deps.secrets.delete(ref);
  }
  return new Response(null, { status: 204 });
}

export async function listModels(ctx: RouteContext): Promise<Response> {
  const providerId = pathParam(ctx, "providerId");
  const provider = await ctx.deps.store.providers.getProvider(providerId);
  if (provider === null) {
    return notFound(`Provider not found: ${providerId}`, ctx.instance);
  }
  const models = await ctx.deps.store.providers.listModels(providerId);
  return jsonResponse(models);
}

/**
 * Re-probe the catalogue. **501 without `deps.providerOps`.**
 *
 * This package cannot do it: refreshing means a request to someone else's
 * server with the provider's credential resolved and injected, and a client for
 * that lives in the host (the same `providerFactory` its `TurnRunner` uses). An
 * HTTP client invented here would be a second place deciding what a request to
 * an unknown endpoint looks like.
 */
export async function refreshProviderModels(
  ctx: RouteContext,
): Promise<Response> {
  const ops = ctx.deps.providerOps;
  if (ops === undefined) {
    return notImplemented(
      "This deployment cannot probe providers; no provider operations are wired.",
      ctx.instance,
    );
  }
  const providerId = pathParam(ctx, "providerId");
  if ((await ctx.deps.store.providers.getProvider(providerId)) === null) {
    return notFound(`Provider not found: ${providerId}`, ctx.instance);
  }
  return jsonResponse(await ops.refreshModels(providerId));
}

/**
 * Probe reachability and credentials. **501 without `deps.providerOps`**, for
 * the same reason as the refresh.
 *
 * A FAILED PROBE IS A 200. The request succeeded — the server tried and is
 * reporting what it found — and an HTTP error would make "this endpoint is
 * down" indistinguishable from "your test request was malformed", which is the
 * one distinction a settings pane exists to draw.
 */
export async function testProvider(ctx: RouteContext): Promise<Response> {
  const ops = ctx.deps.providerOps;
  if (ops === undefined) {
    return notImplemented(
      "This deployment cannot probe providers; no provider operations are wired.",
      ctx.instance,
    );
  }
  const providerId = pathParam(ctx, "providerId");
  if ((await ctx.deps.store.providers.getProvider(providerId)) === null) {
    return notFound(`Provider not found: ${providerId}`, ctx.instance);
  }
  const outcome = await ops.testConnection(providerId);
  const body: TestProviderResponse = {
    ok: outcome.ok,
    ...(outcome.error === undefined ? {} : { error: outcome.error }),
  };
  return jsonResponse(body);
}

/**
 * What {@link planApiKey} decided. `ref` is `undefined` on the two plans that
 * write nothing, so a caller can fold it into the config unconditionally.
 */
type ApiKeyPlan =
  | { kind: "none"; ref?: undefined }
  | { kind: "unavailable"; ref?: undefined; response: Response }
  | { kind: "write"; ref: string; apiKey: string; secrets: SecretStore };

/**
 * What to do about the request's `apiKey`, decided BEFORE anything is written:
 * nothing, the ref to record plus the key to file under it, or the 501 that
 * says this deployment cannot hold one.
 *
 * The refusal is deliberate and is the whole reason this returns a union. The
 * only alternatives to it are writing the key into the provider config (where
 * `listProviders` and every log line would find it) or accepting the request
 * and dropping the key on the floor, and a provider that quietly has no
 * credential fails later, somewhere else, as a puzzling 401 from a vendor.
 *
 * Planning is split from {@link writeApiKey} so the CONFIG is persisted first
 * and the secret second — see there for why that order.
 */
function planApiKey(
  ctx: RouteContext,
  providerId: string,
  apiKey: string | undefined,
): ApiKeyPlan {
  if (apiKey === undefined) return { kind: "none" };
  // The ref is derived from the id, so the id's grammar is checked HERE, where
  // the ref is minted — not only on the create body. The update route reaches
  // this with a raw path parameter, and a row that predates the grammar must
  // not keep an escaped ref alive.
  const idIssue = providerIdIssue(providerId);
  if (idIssue !== null) {
    return {
      kind: "unavailable",
      response: badRequest("invalid_request", idIssue, ctx.instance),
    };
  }
  if (ctx.deps.secrets === undefined) {
    return {
      kind: "unavailable",
      response: notImplemented(
        "This deployment cannot store a provider credential; no SecretStore is wired. Omit `apiKey`.",
        ctx.instance,
      ),
    };
  }
  return {
    kind: "write",
    ref: providerSecretRef(providerId),
    apiKey,
    secrets: ctx.deps.secrets,
  };
}

/**
 * File the planned key under its ref. `null` when there was nothing to write or
 * the write succeeded; a 500 when the secret store refused.
 *
 * CALLED AFTER THE CONFIG IS PERSISTED, and the order is the point. The ref is
 * derived from the provider id, so a config pointing at a ref the store does
 * not hold is recoverable: the next `PATCH` carrying an `apiKey` writes the
 * same ref and the provider works. The reverse failure is not — a secret
 * written for a config that never landed is a live credential under a ref
 * nothing names, unreachable through this API and impossible to notice, which
 * is the worst state a key can be in. So the recoverable failure is the one
 * this route is willing to leave behind.
 */
async function writeApiKey(
  ctx: RouteContext,
  plan: ApiKeyPlan,
): Promise<Response | null> {
  if (plan.kind !== "write") return null;
  try {
    await plan.secrets.set(plan.ref, plan.apiKey);
    return null;
  } catch (err) {
    ctx.deps.logger?.error("provider credential write failed", {
      instance: ctx.instance,
      ref: plan.ref,
      message: err instanceof Error ? err.message : String(err),
    });
    return problemResponse({
      status: 500,
      code: "secret_write_failed",
      detail:
        "The provider was saved, but its credential could not be stored. Re-send the `apiKey` to retry; the ref is derived from the provider id, so a retry overwrites rather than orphans.",
      instance: ctx.instance,
    });
  }
}

/** The metadata bag to store, with the secret ref folded in when there is one. */
function withSecretRef(
  metadata: Record<string, unknown> | undefined,
  ref: string | undefined,
): Record<string, unknown> {
  return {
    ...(metadata ?? {}),
    ...(ref === undefined ? {} : { [PROVIDER_SECRET_REF_KEY]: ref }),
  };
}

/**
 * The patch applied over the stored config.
 *
 * `metadata` REPLACES when the request carries one, and the secret ref is
 * re-folded in afterwards either way — otherwise a client sending
 * `{ metadata: {} }` to clear its own tags would silently unlink the
 * provider's credential, and the next turn would call the vendor unauthenticated.
 */
function mergeProvider(
  existing: AiProviderConfig,
  patch: UpdateProviderRequest,
  newRef: string | undefined,
): AiProviderConfig {
  const metadata =
    patch.metadata === undefined ? (existing.metadata ?? {}) : patch.metadata;
  const ref = newRef ?? existing.metadata?.[PROVIDER_SECRET_REF_KEY];
  return {
    ...existing,
    ...(patch.label === undefined ? {} : { label: patch.label }),
    ...(patch.kind === undefined ? {} : { kind: patch.kind }),
    ...(patch.baseUrl === undefined ? {} : { baseUrl: patch.baseUrl }),
    ...(patch.defaultModel === undefined
      ? {}
      : { defaultModel: patch.defaultModel }),
    ...(patch.enabled === undefined ? {} : { enabled: patch.enabled }),
    ...(patch.extraHeaders === undefined
      ? {}
      : { extraHeaders: patch.extraHeaders }),
    metadata: withSecretRef(
      metadata,
      typeof ref === "string" && ref !== "" ? ref : undefined,
    ),
  };
}
