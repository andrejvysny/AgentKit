import type {
  AiProviderCapabilities,
  AiProviderConfig,
  AiProviderModel,
} from "@agentkit/contracts";

/**
 * Persistence for configured providers, their model catalogues, and their
 * probed capabilities.
 *
 * Capabilities are stored rather than probed per run because probing costs a
 * request and its answer is stable for hours; a run that had to discover
 * "does this endpoint do tool calling?" every time would pay for it on every
 * turn.
 *
 * Secrets: `AiProviderConfig.apiKey` is part of the DTO, but a host that keeps
 * keys out of its main database stores a reference in `metadata` and resolves it
 * through {@link SecretStore} instead — see `secret-store.ts`.
 */
export interface ProviderStore {
  listProviders(): Promise<AiProviderConfig[]>;
  getProvider(providerId: string): Promise<AiProviderConfig | null>;
  upsertProvider(config: AiProviderConfig): Promise<AiProviderConfig>;
  deleteProvider(providerId: string): Promise<void>;

  listModels(providerId: string): Promise<AiProviderModel[]>;
  /** Replace the catalogue wholesale — a refresh is a snapshot, not a merge. */
  replaceModels(providerId: string, models: AiProviderModel[]): Promise<void>;

  getCapabilities(providerId: string): Promise<AiProviderCapabilities | null>;
  saveCapabilities(
    providerId: string,
    capabilities: AiProviderCapabilities,
  ): Promise<void>;
}
