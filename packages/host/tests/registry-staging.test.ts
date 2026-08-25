import { describe, expect, it } from "bun:test";
import { resolveToolLimits, type AiTool } from "@agentkit/core";
import { stageRegistry, type ToolContributionContext } from "../src/index.js";

function tool(name: string): AiTool {
  return {
    definition: {
      name,
      version: "1.0.0",
      effect: "read",
      capability: name,
      description: name,
      inputSchema: { type: "object" },
    },
    async execute(ctx) {
      return {
        ok: true,
        data: {},
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
}

const CTX: ToolContributionContext = {
  chatId: "chat-1",
  runId: "run-1",
  bindings: [],
  limits: resolveToolLimits({ preference: "small" }),
};

describe("stageRegistry", () => {
  it("collects tools from every contributor", async () => {
    const registry = await stageRegistry({
      contributors: [
        { contribute: async () => [tool("a"), tool("b")] },
        { contribute: async () => [tool("c")] },
      ],
      ctx: CTX,
      hasPrimaryBinding: true,
    });
    expect(registry.size()).toBe(3);
    expect(registry.has("c")).toBe(true);
  });

  it("prunes to the declared unbound set when there is no primary binding", async () => {
    const registry = await stageRegistry({
      contributors: [
        {
          contribute: async () => [tool("read_x"), tool("write_x")],
          unboundToolNames: () => ["read_x"],
        },
      ],
      ctx: CTX,
      hasPrimaryBinding: false,
    });
    expect(registry.size()).toBe(1);
    expect(registry.has("read_x")).toBe(true);
    expect(registry.has("write_x")).toBe(false);
  });

  it("unions the declarations across contributors", async () => {
    const registry = await stageRegistry({
      contributors: [
        {
          contribute: async () => [tool("a"), tool("b")],
          unboundToolNames: () => ["a"],
        },
        {
          contribute: async () => [tool("c"), tool("d")],
          unboundToolNames: () => ["c"],
        },
      ],
      ctx: CTX,
      hasPrimaryBinding: false,
    });
    expect(
      registry
        .list()
        .map((t) => t.definition.name)
        .sort(),
    ).toEqual(["a", "c"]);
  });

  it("prunes nothing when no contributor declares an opinion", async () => {
    const registry = await stageRegistry({
      contributors: [{ contribute: async () => [tool("a"), tool("b")] }],
      ctx: CTX,
      hasPrimaryBinding: false,
    });
    expect(registry.size()).toBe(2);
  });

  it("keeps the healthy tools when one cannot register", async () => {
    const warnings: string[] = [];
    const registry = await stageRegistry({
      contributors: [
        { contribute: async () => [tool("ok"), tool("ok"), tool("also_ok")] },
      ],
      ctx: {
        ...CTX,
        logger: {
          debug: () => {},
          info: () => {},
          warn: (message) => warnings.push(message),
          error: () => {},
        },
      },
      hasPrimaryBinding: true,
    });
    expect(registry.size()).toBe(2);
    expect(warnings).toHaveLength(1);
  });
});
