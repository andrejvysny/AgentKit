import { describe, expect, it } from "bun:test";
import type { AiProviderModel } from "@agentkit/contracts";
import { applyHostModelMetadata, mergeModelMetadata } from "../src/index.js";

const base = (over: Partial<AiProviderModel> = {}): AiProviderModel => ({
  providerId: "p1",
  modelId: "m1",
  displayName: null,
  fetchedAt: "2026-01-01T00:00:00.000Z",
  ...over,
});

describe("mergeModelMetadata (ADR 0022)", () => {
  it("host wins field by field, provider fills the gaps, preset fills the rest", () => {
    const merged = mergeModelMetadata({
      host: [base({ contextWindowTokens: 200000, pricing: undefined })],
      provider: [
        base({
          contextWindowTokens: 131072,
          supportsToolCalling: true,
          supportsReasoning: true,
        }),
      ],
      preset: [
        base({
          contextWindowTokens: 4096,
          supportsToolCalling: false,
          supportsStreaming: true,
          supportsVision: false,
        }),
      ],
    });
    expect(merged).toHaveLength(1);
    const model = merged[0];
    expect(model?.contextWindowTokens).toBe(200000); // host
    expect(model?.supportsToolCalling).toBe(true); // provider (preset's false loses)
    expect(model?.supportsReasoning).toBe(true); // provider
    expect(model?.supportsStreaming).toBe(true); // preset only
    expect(model?.supportsVision).toBe(false); // preset (false is a fact, kept)
    expect(model?.metadataSource).toBe("host");
  });

  it("never fabricates: fields absent everywhere stay absent", () => {
    const merged = mergeModelMetadata({
      provider: [base()],
    });
    const model = merged[0];
    expect(model?.contextWindowTokens).toBeUndefined();
    expect(model?.supportsToolCalling).toBeUndefined();
    expect(model?.supportsVision).toBeUndefined();
    expect(model?.pricing).toBeUndefined();
    expect(model?.metadataSource).toBe("provider");
  });

  it("unions models across sources and keys by providerId+modelId", () => {
    const merged = mergeModelMetadata({
      provider: [base({ modelId: "a" })],
      preset: [
        base({ modelId: "a", supportsStreaming: true }),
        base({ modelId: "b", supportsVision: true }),
      ],
    });
    expect(merged.map((m) => m.modelId).sort()).toEqual(["a", "b"]);
    const a = merged.find((m) => m.modelId === "a");
    expect(a?.supportsStreaming).toBe(true);
  });

  it("identity comes from the highest source that lists the model", () => {
    const merged = mergeModelMetadata({
      host: [
        base({
          displayName: "Hosted M",
          fetchedAt: "2026-02-01T00:00:00.000Z",
        }),
      ],
      provider: [
        base({
          displayName: "Provider M",
          fetchedAt: "2026-01-01T00:00:00.000Z",
        }),
      ],
    });
    const model = merged[0];
    expect(model?.displayName).toBe("Hosted M");
    expect(model?.fetchedAt).toBe("2026-02-01T00:00:00.000Z");
  });

  it("an explicit metadataSource on the winning record is honored", () => {
    const merged = mergeModelMetadata({
      host: [base({ supportsVision: true, metadataSource: "preset" })],
    });
    expect(merged[0]?.metadataSource).toBe("preset");
  });

  it("does not mutate inputs", () => {
    const provider = [base()];
    const snapshot = JSON.stringify(provider);
    mergeModelMetadata({ host: [base({ contextWindowTokens: 1 })], provider });
    expect(JSON.stringify(provider)).toBe(snapshot);
  });
});

describe("applyHostModelMetadata", () => {
  it("returns copies when no host metadata exists", () => {
    const discovered = [base({ contextWindowTokens: 8 })];
    const applied = applyHostModelMetadata(discovered, undefined);
    expect(applied).toEqual(discovered);
    expect(applied[0]).not.toBe(discovered[0]);
  });

  it("host overrides win for declared fields only", () => {
    const discovered = [
      base({ contextWindowTokens: 131072, supportsToolCalling: true }),
    ];
    const applied = applyHostModelMetadata(discovered, [
      base({ contextWindowTokens: 32768 }),
    ]);
    const model = applied[0];
    expect(model?.contextWindowTokens).toBe(32768);
    expect(model?.supportsToolCalling).toBe(true);
  });
});
