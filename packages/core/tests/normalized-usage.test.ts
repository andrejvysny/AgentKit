import { describe, expect, it } from "bun:test";
import { normalizedUsage } from "../src/providers/usage.js";

const map = (usage: object) =>
  normalizedUsage(
    usage,
    "prompt_tokens",
    "completion_tokens",
    "prompt_tokens_details",
    "completion_tokens_details",
  );

describe("normalized usage", () => {
  it("leaves unknown counts absent", () => {
    expect(map({ prompt_tokens: 10, completion_tokens: 5 })).toEqual({
      provider_raw: { prompt_tokens: 10, completion_tokens: 5 },
    });
  });

  it("rejects contradictory provider counts", () => {
    expect(() =>
      map({ prompt_tokens: 1, prompt_tokens_details: { cached_tokens: 2 } }),
    ).toThrow("malformed_usage");
    expect(() =>
      map({
        completion_tokens: 1,
        completion_tokens_details: { reasoning_tokens: 2 },
      }),
    ).toThrow("malformed_usage");
  });
});

it("rejects numeric counters outside the safe integer range", () => {
  for (const n of [-1, 1.5, Number.MAX_SAFE_INTEGER + 1]) {
    expect(() => map({ prompt_tokens: n })).toThrow("malformed_usage");
    expect(() => map({ total_tokens: n })).toThrow("malformed_usage");
    expect(() => map({ prompt_tokens_details: { cached_tokens: n } })).toThrow(
      "malformed_usage",
    );
  }
  expect(
    map({
      prompt_tokens: Number.MAX_SAFE_INTEGER,
      prompt_tokens_details: { cached_tokens: 0 },
    }).input_uncached,
  ).toBe(Number.MAX_SAFE_INTEGER);
});
