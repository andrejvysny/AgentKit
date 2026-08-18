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

const utf8 = (s: string): number => new TextEncoder().encode(s).length;
const MARKER = "\n[...truncated]";

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

  it("passes through an ASCII value that exactly fills the budget", () => {
    const value = "x".repeat(64);
    expect(truncateString(value, 64)).toEqual({ value, truncated: false });
  });

  it("truncates an ASCII value one byte over the budget", () => {
    const out = truncateString("x".repeat(65), 64);
    expect(out.truncated).toBe(true);
    expect(utf8(out.value)).toBeLessThanOrEqual(64);
    expect(out.value.endsWith(MARKER)).toBe(true);
  });

  // Below: byte count and char count diverge, so a UTF-16-length check would
  // let ~2-4x the budget through.
  it("counts 2-byte code points (é) as bytes, not characters", () => {
    // 40 chars but 80 UTF-8 bytes: a length-based check would call this "fits".
    const value = "é".repeat(40);
    expect(value.length).toBe(40);
    expect(utf8(value)).toBe(80);
    const out = truncateString(value, 64);
    expect(out.truncated).toBe(true);
    expect(utf8(out.value)).toBeLessThanOrEqual(64);
    expect(out.value).not.toContain("�");
    // budget = 64 - 15 marker bytes = 49 → 24 whole "é" (48 bytes) fit.
    expect(out.value).toBe("é".repeat(24) + MARKER);
  });

  it("passes through a 2-byte value whose bytes exactly fill the budget", () => {
    const value = "é".repeat(16); // 32 bytes
    expect(truncateString(value, 32)).toEqual({ value, truncated: false });
    expect(truncateString(value, 31).truncated).toBe(true);
  });

  it("counts 3-byte code points (中) as bytes and never splits one", () => {
    const value = "中".repeat(30); // 30 chars, 90 bytes
    expect(utf8(value)).toBe(90);
    const out = truncateString(value, 40);
    expect(out.truncated).toBe(true);
    expect(utf8(out.value)).toBeLessThanOrEqual(40);
    expect(out.value).not.toContain("�");
    // budget = 25 bytes → 8 whole "中" (24 bytes); the 25th byte is left unused
    // rather than cutting a code point in half.
    expect(out.value).toBe("中".repeat(8) + MARKER);
    expect(utf8(out.value)).toBe(39);
  });

  it("never splits a surrogate pair (4-byte 😀)", () => {
    const value = "😀".repeat(10); // 20 code units, 40 bytes
    expect(value.length).toBe(20);
    expect(utf8(value)).toBe(40);
    const out = truncateString(value, 30);
    expect(out.truncated).toBe(true);
    expect(utf8(out.value)).toBeLessThanOrEqual(30);
    expect(out.value).not.toContain("�");
    // budget = 15 bytes → 3 whole emoji (12 bytes).
    expect(out.value).toBe("😀".repeat(3) + MARKER);
    // Re-encoding the result must round-trip cleanly (no lone surrogate).
    expect(new TextDecoder().decode(new TextEncoder().encode(out.value))).toBe(
      out.value,
    );
  });

  it("keeps the byte cap on mixed-width text and emits no U+FFFD", () => {
    const value = "a-é-中-😀-".repeat(20);
    for (const maxBytes of [16, 17, 31, 32, 33, 64, 100, 101]) {
      const out = truncateString(value, maxBytes);
      expect(out.truncated).toBe(true);
      expect(utf8(out.value)).toBeLessThanOrEqual(maxBytes);
      expect(out.value).not.toContain("�");
      expect(out.value.endsWith(MARKER)).toBe(true);
    }
  });

  it("stays within the cap when there is no room for the marker", () => {
    const out = truncateString("中".repeat(10), 8);
    expect(out.truncated).toBe(true);
    expect(utf8(out.value)).toBeLessThanOrEqual(8);
    expect(out.value).not.toContain("�");
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
