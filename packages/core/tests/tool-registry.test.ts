import { describe, expect, it } from "bun:test";
import { AiToolRegistry } from "../src/tools/registry.js";
import type { AiTool } from "../src/tools/types.js";

function makeTool(name: string): AiTool {
  return {
    definition: {
      name,
      version: "1",
      effect: "read",
      capability: "test",
      description: "test",
      inputSchema: { type: "object" },
    },
    async execute(ctx, _input) {
      return {
        ok: true,
        data: null,
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

describe("AiToolRegistry", () => {
  it("registers a tool with provider-safe name", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("library_search_components"));
    expect(r.size()).toBe(1);
    expect(r.has("library_search_components")).toBe(true);
  });

  it("rejects dotted names", () => {
    const r = new AiToolRegistry();
    expect(() => r.register(makeTool("library.search"))).toThrow(
      /Invalid tool name/,
    );
  });

  it("rejects spaces", () => {
    const r = new AiToolRegistry();
    expect(() => r.register(makeTool("library search"))).toThrow(
      /Invalid tool name/,
    );
  });

  it("rejects duplicate registration", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("foo"));
    expect(() => r.register(makeTool("foo"))).toThrow(/Duplicate/);
  });

  it("lists definitions", () => {
    const r = new AiToolRegistry();
    r.register(makeTool("a"));
    r.register(makeTool("b"));
    expect(
      r
        .listDefinitions()
        .map((d) => d.name)
        .sort(),
    ).toEqual(["a", "b"]);
  });
});
