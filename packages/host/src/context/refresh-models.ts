import type { AiProviderConfig, AiProviderModel } from "@agentkit/contracts";
import { type AiProviderClient, applyHostModelMetadata } from "@agentkit/core";
import { RecordNotFoundError } from "../errors.js";
import type { ProviderStore } from "../ports/provider-store.js";
import type { SecretStore } from "../ports/secret-store.js";
import { PROVIDER_SECRET_REF_KEY } from "../turn/turn-runner.js";

export interface RefreshProviderModelsDeps {
  store: { providers: ProviderStore };
  providerFactory(config: AiProviderConfig): AiProviderClient;
  secrets?: SecretStore;
  /**
   * Explicit host-declared model metadata. Merged over the provider-
   * advertised catalogue field by field with host precedence (see
   * `mergeModelMetadata`): a host override wins for the fields it declares;
   * everything else the provider advertised is kept; unknown stays unknown.
   */
  modelMetadata?: AiProviderModel[];
}

/**
 * Probe the provider's model catalogue and REPLACE the stored one.
 *
 * This is what makes a server-advertised context window (`max_model_len` and
 * friends) reach the session budget: `resolveContextWindow` reads the stored
 * models, and nothing else persists them. It is an explicit call rather than a
 * per-turn refresh because it costs a request to someone else's server; a host
 * wires it into `ProviderOperations.refreshModels` or calls it at setup.
 */
export async function refreshProviderModels(
  deps: RefreshProviderModelsDeps,
  providerId: string,
  signal?: AbortSignal,
): Promise<AiProviderModel[]> {
  const provider = await deps.store.providers.getProvider(providerId);
  if (provider === null) {
    throw new RecordNotFoundError(`Provider not found: ${providerId}`, {
      providerId,
    });
  }
  // Same secret resolution as a turn: the ref in metadata, never a stored key.
  const ref = provider.metadata?.[PROVIDER_SECRET_REF_KEY];
  const apiKey =
    deps.secrets !== undefined && typeof ref === "string"
      ? await deps.secrets.get(ref)
      : null;
  const client = deps.providerFactory(
    apiKey === null ? provider : { ...provider, apiKey },
  );
  const models = applyHostModelMetadata(
    await client.listModels(signal),
    deps.modelMetadata,
  );
  await deps.store.providers.replaceModels(providerId, models);
  return models;
}
