import { getPresetByKind } from "@agentkit/core";
import type { AiProviderKind, AiProviderConfig } from "@agentkit/contracts";
import type { NativeStore } from "@agentkit/native";
import type { WiringEnv } from "./wiring.js";

const DEFAULT_PROVIDER_ID = "default";
const DEFAULT_PROVIDER_KIND = "openai-compatible";

/** Environment bootstrap input; native managed metadata is authoritative after creation. */
export interface ResolvedProvider {
  id: string;
  kind: AiProviderKind;
  label: string;
  baseUrl: string;
  defaultModel: string;
  apiKey?: string;
}

export function resolveProvider(env: WiringEnv): ResolvedProvider {
  const kind = (env.AGENTKIT_PROVIDER_KIND ??
    DEFAULT_PROVIDER_KIND) as AiProviderKind;
  const preset = getPresetByKind(kind);
  if (!preset) throw new Error(`Unknown provider kind "${kind}".`);
  return {
    id: DEFAULT_PROVIDER_ID,
    kind,
    label: preset?.label ?? kind,
    baseUrl:
      env.AGENTKIT_BASE_URL ??
      preset?.defaultBaseUrl ??
      "http://127.0.0.1:8000/v1",
    defaultModel: env.AGENTKIT_MODEL ?? preset?.defaultModel ?? "local-model",
    ...(env.AGENTKIT_API_KEY === undefined || env.AGENTKIT_API_KEY === ""
      ? {}
      : { apiKey: env.AGENTKIT_API_KEY }),
  };
}

/** Display of the explicitly configured fixed connection; never native metadata authority. */
interface ProviderSeed {
  id: string;
  label: string;
  kind: string;
  baseUrl: string;
  defaultModel: string;
  enabled: boolean;
  metadata: Record<string, unknown>;
}

export function legacyProviderRecord(provider: ResolvedProvider): ProviderSeed {
  const metadata: Record<string, unknown> = {};
  // The key itself never lands in the document — only its presence is noted;
  // the credential lives in the provider lane (Rust connection handle or the
  // JS client), exactly as the old `PROVIDER_SECRET_REF_KEY` metadata meant.
  if (provider.apiKey !== undefined) metadata.credential = "env";
  return {
    id: provider.id,
    label: provider.label,
    kind: provider.kind,
    baseUrl: provider.baseUrl,
    defaultModel: provider.defaultModel,
    enabled: true,
    metadata,
  };
}

export function providerConfigOf(provider: ResolvedProvider): AiProviderConfig {
  return {
    id: provider.id,
    label: provider.label,
    kind: provider.kind,
    baseUrl: provider.baseUrl,
    defaultModel: provider.defaultModel,
    enabled: true,
    ...(provider.apiKey === undefined ? {} : { apiKey: provider.apiKey }),
  };
}

/** Seed native metadata once; credentials cross only the explicit host vault port. */
export async function seedManagedProvider(
  store: NativeStore,
  baseUrl: string,
  bearerToken: string,
  provider: ResolvedProvider,
): Promise<void> {
  const headers = {
    authorization: `Bearer ${bearerToken}`,
    "content-type": "application/json",
  };
  const response = await fetch(`${baseUrl}/v1/providers`, { headers });
  if (!response.ok)
    throw new Error(`Native provider list failed (${response.status})`);
  const providers = (await response.json()) as { id: string }[];
  if (providers.length > 0) return;
  const created = await fetch(`${baseUrl}/v1/providers`, {
    method: "POST",
    headers,
    body: JSON.stringify({ ...provider, enabled: true }),
  });
  if (!created.ok)
    throw new Error(`Native provider seed failed (${created.status})`);
  await store.callStore("settings.updateSettings", {
    patch: {
      defaultProviderId: provider.id,
      defaultModel: provider.defaultModel,
    },
  });
}
