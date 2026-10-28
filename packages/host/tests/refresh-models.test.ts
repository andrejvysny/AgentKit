import { describe, expect, it } from "bun:test";
import type { AiProviderConfig, AiProviderModel } from "@agentkit/contracts";
import type { AiProviderClient } from "@agentkit/core";
import { refreshProviderModels } from "../src/context/refresh-models.js";
import type { ProviderStore } from "../src/ports/provider-store.js";

const config: AiProviderConfig = {
  id: "p1",
  label: "P1",
  kind: "openai-compatible",
  baseUrl: "http://localhost:8000/v1",
  defaultModel: "m1",
  enabled: true,
};

function fakeStore(): {
  store: { providers: ProviderStore };
  stored: () => AiProviderModel[] | undefined;
} {
  let stored: AiProviderModel[] | undefined;
  const noop = async () => {};
  const store: ProviderStore = {
    listProviders: async () => [config],
    getProvider: async (id) => (id === "p1" ? config : null),
    upsertProvider: async (c) => c,
    deleteProvider: noop,
    listModels: async () => stored ?? [],
    replaceModels: async (_id, models) => {
      stored = models;
    },
    getCapabilities: async () => null,
    saveCapabilities: noop,
  };
  return {
    store: { providers: store },
    stored: () => stored,
  };
}

function fakeClient(models: AiProviderModel[]): AiProviderClient {
  return {
    id: "p1",
    kind: "openai-compatible",
    capabilities: async () => ({
      streaming: true,
      toolCalling: true,
      modelList: true,
    }),
    listModels: async () => models,
    streamChat: () => {
      throw new Error("not used");
    },
  };
}

describe("refreshProviderModels metadata merge (B2)", () => {
  it("stores the provider catalogue as-is when no host metadata exists", async () => {
    const { store, stored } = fakeStore();
    const discovered: AiProviderModel[] = [
      {
        providerId: "p1",
        modelId: "m1",
        displayName: null,
        contextWindowTokens: 131072,
        metadataSource: "provider",
        fetchedAt: "2026-10-28T00:00:00.000Z",
      },
    ];
    const result = await refreshProviderModels(
      {
        store,
        providerFactory: () => fakeClient(discovered),
      },
      "p1",
    );
    expect(result).toEqual(discovered);
    expect(stored()).toEqual(discovered);
  });

  it("host-declared metadata wins field by field and is attributed", async () => {
    const { store, stored } = fakeStore();
    const discovered: AiProviderModel[] = [
      {
        providerId: "p1",
        modelId: "m1",
        displayName: "Server M",
        contextWindowTokens: 4096, // stale server-advertised window
        supportsToolCalling: true,
        metadataSource: "provider",
        fetchedAt: "2026-10-28T00:00:00.000Z",
      },
    ];
    const result = await refreshProviderModels(
      {
        store,
        providerFactory: () => fakeClient(discovered),
        modelMetadata: [
          {
            providerId: "p1",
            modelId: "m1",
            displayName: "Server M",
            contextWindowTokens: 131072,
            pricing: {
              currency: "USD",
              inputPerMillionTokens: 0.1,
              priceSource: "host",
            },
            fetchedAt: "2026-10-28T00:00:00.000Z",
          },
        ],
      },
      "p1",
    );
    const model = result[0];
    expect(model?.contextWindowTokens).toBe(131072); // host override wins
    expect(model?.supportsToolCalling).toBe(true); // provider fact kept
    expect(model?.pricing?.priceSource).toBe("host");
    expect(model?.metadataSource).toBe("host");
    expect(stored()).toEqual(result);
  });
});
