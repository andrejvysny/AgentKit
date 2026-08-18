import { describe, expect, it } from "bun:test";
import {
  resolveToolLimits,
  truncateArray,
  truncateString,
} from "../src/tools/limits.js";

describe("resolveToolLimits", () => {
  it("returns profile defaults", () => {
    expect(resolveToolLimits({ preference: "small" }).maxBytes).toBe(16 * 1024);
    expect(resolveToolLimits({ preference: "medium" }).maxBytes).toBe(
      64 * 1024,
    );
    expect(resolveToolLimits({ preference: "large" }).maxBytes).toBe(
      128 * 1024,
    );
  });

  it("caps to model context budget when smaller", () => {
    // 4096 tokens * 4 chars * 0.25 = 4096 bytes < 16KB small profile
    const limits = resolveToolLimits({
      preference: "small",
      modelContextTokens: 4096,
    });
    expect(limits.maxBytes).toBe(4096);
  });

  it("respects requestedMaxBytes override", () => {
    const limits = resolveToolLimits({
      preference: "medium",
      requestedMaxBytes: 2048,
    });
    expect(limits.maxBytes).toBe(2048);
  });

  it("enforces floor of 1024 bytes", () => {
    const limits = resolveToolLimits({
      preference: "small",
      requestedMaxBytes: 100,
    });
    expect(limits.maxBytes).toBe(1024);
  });
});

describe("truncateString", () => {
  it("returns unchanged when within budget", () => {
    expect(truncateString("hi", 10)).toEqual({ value: "hi", truncated: false });
  });
  it("truncates and marks", () => {
    const out = truncateString("0123456789012345678901234567890", 20);
    expect(out.truncated).toBe(true);
    expect(out.value.length).toBeLessThanOrEqual(20);
    expect(out.value).toContain("[...truncated]");
  });
});

describe("truncateArray", () => {
  it("returns unchanged when no cap", () => {
    expect(truncateArray([1, 2, 3], undefined)).toEqual({
      items: [1, 2, 3],
      truncated: false,
    });
  });
  it("caps to maxItems", () => {
    expect(truncateArray([1, 2, 3, 4], 2)).toEqual({
      items: [1, 2],
      truncated: true,
    });
  });
});
