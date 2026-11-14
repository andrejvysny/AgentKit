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
  ).toBeUndefined();
});

const responses = (usage: object) =>
  normalizedUsage(
    usage,
    "input_tokens",
    "output_tokens",
    "input_tokens_details",
    "output_tokens_details",
  );
it("maps cache-write lifetime splits and preserves raw usage", () => {
  const raw = {
    input_tokens: 100,
    output_tokens: 10,
    input_tokens_details: {
      cached_tokens: 20,
      cache_write_tokens: 30,
      cache_write_tokens_details: { ttl_30m_tokens: 10, ttl_12h_tokens: 20 },
    },
    output_tokens_details: { reasoning_tokens: 3 },
  };
  expect(responses(raw)).toMatchObject({
    input_uncached: 50,
    cache_read: 20,
    cache_write_short: 10,
    cache_write_long: 20,
    output: 7,
    reasoning: 3,
    provider_raw: raw,
  });
});

it("maps zero, aggregate and complete cache-write counts", () => {
  expect(
    responses({
      input_tokens: 100,
      input_tokens_details: { cached_tokens: 20, cache_write_tokens: 0 },
    }),
  ).toMatchObject({
    input_uncached: 80,
    cache_write_short: 0,
    cache_write_long: 0,
  });
  expect(
    responses({
      input_tokens: 100,
      input_tokens_details: { cached_tokens: 20, cache_write_tokens: 30 },
    }),
  ).toMatchObject({ input_uncached: 50 });
  expect(
    responses({
      input_tokens: 100,
      input_tokens_details: {
        cached_tokens: 20,
        cache_write_tokens_details: { ttl_30m_tokens: 10, ttl_12h_tokens: 20 },
      },
    }),
  ).toMatchObject({
    input_uncached: 50,
    cache_write_short: 10,
    cache_write_long: 20,
  });
});

it("keeps missing cache-write counts unknown", () => {
  for (const details of [
    { cached_tokens: 20 },
    { cached_tokens: 20, cache_write_tokens: null },
    { cached_tokens: 20, cache_write_tokens_details: { ttl_30m_tokens: 10 } },
  ])
    expect(
      responses({ input_tokens: 100, input_tokens_details: details }),
    ).not.toHaveProperty("input_uncached");
  expect(
    map({
      prompt_tokens: 100,
      prompt_tokens_details: { cached_tokens: 20, cache_write_tokens: 30 },
    }),
  ).toMatchObject({ input_uncached: 50, cache_read: 20 });
});
it("rejects contradictory or malformed cache writes", () => {
  for (const detail of [
    { cached_tokens: 80, cache_write_tokens: 30 },
    {
      cached_tokens: 20,
      cache_write_tokens: 29,
      cache_write_tokens_details: { ttl_30m_tokens: 10, ttl_12h_tokens: 20 },
    },
    { cache_write_tokens: -1 },
    { cache_write_tokens: 1.5 },
    { cache_write_tokens: Number.MAX_SAFE_INTEGER + 1 },
    {
      cache_write_tokens_details: {
        ttl_30m_tokens: Number.MAX_SAFE_INTEGER,
        ttl_12h_tokens: 1,
      },
    },
  ])
    expect(() =>
      responses({ input_tokens: 100, input_tokens_details: detail }),
    ).toThrow("malformed_usage");
});

it("keeps unreported output and chat lifetime classes unknown", () => {
  const usage = map({
    prompt_tokens: 100,
    completion_tokens: 10,
    prompt_tokens_details: {
      cached_tokens: 20,
      cache_write_tokens: 30,
      cache_write_tokens_details: { ttl_30m_tokens: 10, ttl_12h_tokens: 20 },
    },
  });
  expect(usage.input_uncached).toBe(50);
  expect(usage).not.toHaveProperty("output");
  expect(usage).not.toHaveProperty("cache_write_short");
  expect(usage).not.toHaveProperty("cache_write_long");
});

it("rejects partial writes that already exceed input", () => {
  for (const details of [
    { cached_tokens: 60, cache_write_tokens_details: { ttl_30m_tokens: 60 } },
    { cache_write_tokens_details: { ttl_30m_tokens: 120 } },
  ])
    expect(() =>
      responses({ input_tokens: 100, input_tokens_details: details }),
    ).toThrow("malformed_usage");
});
