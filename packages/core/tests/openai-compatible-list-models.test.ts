import { describe, expect, it } from "bun:test";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";

function clientListing(data: unknown[]): OpenAiCompatibleClient {
  return new OpenAiCompatibleClient({
    id: "test",
    kind: "openai-compatible",
    baseUrl: "http://localhost:9/v1",
    fetchImpl: (async () =>
      new Response(JSON.stringify({ data }), {
        status: 200,
        headers: { "content-type": "application/json" },
      })) as unknown as typeof fetch,
  });
}

describe("listModels context window", () => {
  it("maps vLLM max_model_len and LM Studio context_length", async () => {
    const models = await clientListing([
      { id: "vllm", max_model_len: 131072 },
      { id: "lms", context_length: 8192 },
      { id: "win", context_window: 4096 },
      { id: "max", max_context_length: 2048 },
    ]).listModels();
    expect(models.map((m) => [m.modelId, m.contextWindowTokens])).toEqual([
      ["vllm", 131072],
      ["lms", 8192],
      ["win", 4096],
      ["max", 2048],
    ]);
  });

  it("takes the first usable key and ignores invalid values", async () => {
    const [first, second, third] = await clientListing([
      { id: "a", max_model_len: 0, context_length: 4096 },
      { id: "b", max_model_len: "8192", context_length: 1.5 },
      { id: "c" },
    ]).listModels();
    expect(first?.contextWindowTokens).toBe(4096);
    expect(second).not.toHaveProperty("contextWindowTokens");
    expect(third).not.toHaveProperty("contextWindowTokens");
  });
});
