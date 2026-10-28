import { describe, expect, it } from "bun:test";
import Ajv from "ajv";
import { AiProviderModelSchema } from "../src/index.js";

const asJson = (schema: object): object =>
  JSON.parse(JSON.stringify(schema)) as object;
const ajv = new Ajv({ strict: true, allErrors: true });
const validate = ajv.compile(asJson(AiProviderModelSchema));

const legacy = {
  providerId: "p1",
  modelId: "m1",
  displayName: null,
  fetchedAt: "2026-10-28T00:00:00.000Z",
};

describe("AiProviderModel metadata additions (B2)", () => {
  it("accepts the pre-B2 legacy shape unchanged (backward compatible)", () => {
    expect(validate(legacy)).toBe(true);
  });

  it("accepts full capability and pricing metadata", () => {
    expect(
      validate({
        ...legacy,
        contextWindowTokens: 131072,
        supportsToolCalling: true,
        supportsStreaming: true,
        supportsJsonMode: false,
        supportsVision: true,
        supportsAudio: false,
        supportsReasoning: true,
        reasoningEfforts: ["low", "medium", "high"],
        maxOutputTokens: 16384,
        pricing: {
          currency: "USD",
          inputPerMillionTokens: 2.5,
          outputPerMillionTokens: 10,
          cachedInputPerMillionTokens: 1.25,
          reasoningTokensBilledAsOutput: true,
          priceSource: "host",
          effectiveDate: "2026-01-01",
        },
        metadataSource: "host",
        metadataFetchedAt: "2026-10-01T00:00:00.000Z",
      }),
    ).toBe(true);
  });

  it("rejects a pricing block without a currency", () => {
    expect(validate({ ...legacy, pricing: { inputPerMillionTokens: 1 } })).toBe(
      false,
    );
  });

  it("rejects an unattributed price (priceSource is required)", () => {
    expect(
      validate({
        ...legacy,
        pricing: { currency: "USD", inputPerMillionTokens: 1 },
      }),
    ).toBe(false);
  });

  it("rejects unknown price sources", () => {
    expect(
      validate({
        ...legacy,
        pricing: {
          currency: "USD",
          inputPerMillionTokens: 1,
          priceSource: "scraper",
        },
      }),
    ).toBe(false);
  });

  it("rejects negative prices", () => {
    expect(
      validate({
        ...legacy,
        pricing: {
          currency: "USD",
          inputPerMillionTokens: -1,
          priceSource: "host",
        },
      }),
    ).toBe(false);
  });

  it("rejects a malformed effective date", () => {
    expect(
      validate({
        ...legacy,
        pricing: {
          currency: "USD",
          inputPerMillionTokens: 1,
          priceSource: "host",
          effectiveDate: "yesterday",
        },
      }),
    ).toBe(false);
  });

  it("rejects unknown metadata sources", () => {
    expect(validate({ ...legacy, metadataSource: "rumor" })).toBe(false);
  });
});
