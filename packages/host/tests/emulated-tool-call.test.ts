import { describe, expect, it } from "bun:test";
import { looksLikeEmulatedToolCall } from "../src/turn/emulated-tool-call.js";

describe("looksLikeEmulatedToolCall", () => {
  it("fires on a fenced json block carrying both a name and arguments", () => {
    expect(
      looksLikeEmulatedToolCall(
        'Calling the tool now:\n```json\n{"name": "write_items", "arguments": {"count": 2}}\n```',
      ),
    ).toBe(true);
  });

  it("accepts the other key spellings models use", () => {
    for (const [nameKey, argKey] of [
      ["tool", "parameters"],
      ["tool_name", "args"],
      ["function", "input"],
    ]) {
      expect(
        looksLikeEmulatedToolCall(
          `\`\`\`\n{"${nameKey}": "x", "${argKey}": {}}\n\`\`\``,
        ),
      ).toBe(true);
    }
  });

  it("fires on an array of emulated calls", () => {
    expect(
      looksLikeEmulatedToolCall(
        '```json\n[{"name": "a", "arguments": {}}, {"name": "b", "arguments": {}}]\n```',
      ),
    ).toBe(true);
  });

  it("finds the block even when it is not the first fence", () => {
    expect(
      looksLikeEmulatedToolCall(
        '```python\nprint("hi")\n```\nand then\n```json\n{"tool":"t","args":{}}\n```',
      ),
    ).toBe(true);
  });

  it("does not fire on JSON with only one of the two keys", () => {
    expect(
      looksLikeEmulatedToolCall('```json\n{"name": "config", "value": 3}\n```'),
    ).toBe(false);
    expect(
      looksLikeEmulatedToolCall('```json\n{"arguments": {"a": 1}}\n```'),
    ).toBe(false);
  });

  it("does not fire on prose that merely talks about tools", () => {
    expect(
      looksLikeEmulatedToolCall(
        "I will call the write_items tool with the arguments you gave me.",
      ),
    ).toBe(false);
  });

  it("does not fire on unfenced JSON", () => {
    expect(
      looksLikeEmulatedToolCall('{"name": "write_items", "arguments": {}}'),
    ).toBe(false);
  });

  it("ignores a fence that is not valid JSON", () => {
    expect(
      looksLikeEmulatedToolCall(
        '```json\n{"name": "x", "arguments": {,,,}\n```',
      ),
    ).toBe(false);
  });

  it("ignores empty input and empty fences", () => {
    expect(looksLikeEmulatedToolCall("")).toBe(false);
    expect(looksLikeEmulatedToolCall("```json\n\n```")).toBe(false);
    expect(looksLikeEmulatedToolCall('```json\n"just a string"\n```')).toBe(
      false,
    );
  });

  it("is not stateful across calls (the /g pattern is module-level)", () => {
    const emulated = '```json\n{"name": "t", "arguments": {}}\n```';
    expect(looksLikeEmulatedToolCall(emulated)).toBe(true);
    expect(looksLikeEmulatedToolCall(emulated)).toBe(true);
    expect(looksLikeEmulatedToolCall(emulated)).toBe(true);
  });
});
