import { describe, expect, it } from "bun:test";
import { projectMcpCallResult } from "../src/index.js";

describe("MCP result metadata", () => {
  it("preserves standard nested result metadata beside content and structured content", () => {
    const metadata = { cursor: "opaque", nested: { values: [1, false, null] } };
    const result = projectMcpCallResult("notes", "echo", "echo", {
      content: [{ type: "text", text: "hello" }],
      structuredContent: { value: 1 },
      _meta: metadata,
    });
    expect(result.resultMetadata).toEqual(metadata);
    expect(result.structuredContent).toEqual({ value: 1 });
    expect(result.text).toBe("hello");
  });

  it("omits absent or invalid metadata rather than widening the outcome", () => {
    for (const value of [undefined, null, [], "invalid"]) {
      const result = projectMcpCallResult("notes", "echo", "echo", {
        content: [],
        _meta: value,
      });
      expect(result).not.toHaveProperty("resultMetadata");
    }
  });
});
