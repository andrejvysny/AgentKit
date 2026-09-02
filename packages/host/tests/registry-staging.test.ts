import { describe, expect, it } from "bun:test";
import { resolveToolLimits, type AiTool } from "@agentkit/core";
import {
  type AgentKitHostError,
  stageRegistry,
  TOOL_GUARD_ERROR_MESSAGE,
  TOOL_GUARD_REFUSED_CODE,
  type ToolContributionContext,
  type ToolGuard,
  type ToolSetContributor,
} from "../src/index.js";

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

/** A contributor over a fixed tool set, in its own namespace. */
function contributor(
  namespace: string,
  tools: AiTool[],
  extra: Partial<ToolSetContributor> = {},
): ToolSetContributor {
  return { namespace, contribute: async () => tools, ...extra };
}

const CTX: ToolContributionContext = {
  chatId: "chat-1",
  runId: "run-1",
  bindings: [],
  limits: resolveToolLimits({ preference: "small" }),
};

describe("stageRegistry", () => {
  it("collects tools from every contributor", async () => {
    const staged = await stageRegistry({
      contributors: [
        contributor("alpha", [tool("a"), tool("b")]),
        contributor("beta", [tool("c")]),
      ],
      ctx: CTX,
      hasPrimaryBinding: true,
    });
    expect(staged.registry.size()).toBe(3);
    expect(staged.registry.has("c")).toBe(true);
  });

  it("records the owning namespace of every staged tool", async () => {
    const staged = await stageRegistry({
      contributors: [
        contributor("alpha", [tool("a")]),
        contributor("beta", [tool("c")]),
      ],
      ctx: CTX,
      hasPrimaryBinding: true,
    });
    expect([...staged.namespaces.entries()].sort()).toEqual([
      ["a", "alpha"],
      ["c", "beta"],
    ]);
  });

  it("prunes to the declared unbound set when there is no primary binding", async () => {
    const staged = await stageRegistry({
      contributors: [
        contributor("alpha", [tool("read_x"), tool("write_x")], {
          unboundToolNames: () => ["read_x"],
        }),
      ],
      ctx: CTX,
      hasPrimaryBinding: false,
    });
    expect(staged.registry.size()).toBe(1);
    expect(staged.registry.has("read_x")).toBe(true);
    expect(staged.registry.has("write_x")).toBe(false);
  });

  it("unions the declarations across contributors", async () => {
    const staged = await stageRegistry({
      contributors: [
        contributor("alpha", [tool("a"), tool("b")], {
          unboundToolNames: () => ["a"],
        }),
        contributor("beta", [tool("c"), tool("d")], {
          unboundToolNames: () => ["c"],
        }),
      ],
      ctx: CTX,
      hasPrimaryBinding: false,
    });
    expect(
      staged.registry
        .list()
        .map((t) => t.definition.name)
        .sort(),
    ).toEqual(["a", "c"]);
  });

  it("prunes nothing when no contributor declares an opinion", async () => {
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a"), tool("b")])],
      ctx: CTX,
      hasPrimaryBinding: false,
    });
    expect(staged.registry.size()).toBe(2);
  });

  it("keeps the healthy tools when one cannot register", async () => {
    const warnings: string[] = [];
    const staged = await stageRegistry({
      contributors: [
        contributor("alpha", [tool("ok"), tool("ok"), tool("also_ok")]),
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
    expect(staged.registry.size()).toBe(2);
    expect(warnings).toHaveLength(1);
  });
});

describe("stageRegistry — namespaces", () => {
  it.each(["agentkit", "chat", "mcp"])(
    "refuses the reserved namespace %s to an ordinary contributor",
    async (namespace) => {
      const staging = stageRegistry({
        contributors: [contributor(namespace, [tool("a")])],
        ctx: CTX,
        hasPrimaryBinding: true,
      });
      await expect(staging).rejects.toThrow(/reserved/i);
      const err = await staging.catch((e: unknown) => e as AgentKitHostError);
      expect((err as AgentKitHostError).code).toBe("tool_namespace_reserved");
    },
  );

  it("lets a privileged contributor claim mcp", async () => {
    const staged = await stageRegistry({
      contributors: [
        contributor("mcp", [tool("mcp__gh__search")], { privileged: true }),
      ],
      ctx: CTX,
      hasPrimaryBinding: true,
    });
    expect(staged.registry.has("mcp__gh__search")).toBe(true);
    expect(staged.namespaces.get("mcp__gh__search")).toBe("mcp");
  });

  it.each(["", "MCP", "1st", "has space", "dotted.ns"])(
    "rejects the malformed namespace %p",
    async (namespace) => {
      const staging = stageRegistry({
        contributors: [contributor(namespace, [tool("a")])],
        ctx: CTX,
        hasPrimaryBinding: true,
      });
      const err = await staging.catch((e: unknown) => e as AgentKitHostError);
      expect((err as AgentKitHostError).code).toBe("tool_namespace_invalid");
    },
  );

  it("checks namespaces BEFORE contributing anything", async () => {
    let contributed = false;
    const staging = stageRegistry({
      contributors: [
        {
          namespace: "alpha",
          contribute: async () => {
            contributed = true;
            return [tool("a")];
          },
        },
        contributor("chat", [tool("b")]),
      ],
      ctx: CTX,
      hasPrimaryBinding: true,
    });
    await expect(staging).rejects.toThrow(/reserved/i);
    expect(contributed).toBe(false);
  });

  it("fails the whole staging closed when two contributors offer one name", async () => {
    const staging = stageRegistry({
      contributors: [
        contributor("alpha", [tool("shared")]),
        contributor("beta", [tool("shared")]),
      ],
      ctx: CTX,
      hasPrimaryBinding: true,
    });
    const err = (await staging.catch((e: unknown) => e)) as AgentKitHostError;
    expect(err.code).toBe("tool_name_collision");
    // Both sides named, or whoever has to fix the wiring cannot tell which two.
    expect(err.message).toContain("alpha");
    expect(err.message).toContain("beta");
    expect(err.message).toContain("shared");
  });

  it("fails a collision even on a tool the unbound pruning would drop", async () => {
    const staging = stageRegistry({
      contributors: [
        contributor("alpha", [tool("shared")], {
          unboundToolNames: () => [],
        }),
        contributor("beta", [tool("shared")]),
      ],
      ctx: CTX,
      hasPrimaryBinding: false,
    });
    await expect(staging).rejects.toThrow(/tool name/i);
  });
});

describe("stageRegistry — guards", () => {
  const hideB: ToolGuard = {
    isVisible: (ctx) => ctx.tool.name !== "b",
  };

  it("does not stage a tool any guard hides", async () => {
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a"), tool("b")])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [hideB],
    });
    expect(staged.registry.list().map((t) => t.definition.name)).toEqual(["a"]);
    expect(staged.namespaces.has("b")).toBe(false);
  });

  it("composes visibility with AND across guards", async () => {
    const hideA: ToolGuard = { isVisible: (ctx) => ctx.tool.name !== "a" };
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a"), tool("b"), tool("c")])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [hideA, hideB],
    });
    expect(staged.registry.list().map((t) => t.definition.name)).toEqual(["c"]);
  });

  it("shows the guard the namespace, the tool and the chat's bindings", async () => {
    const seen: { namespace: string; name: string; chatId?: string }[] = [];
    await stageRegistry({
      contributors: [contributor("alpha", [tool("a")])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [
        {
          isVisible: (ctx) => {
            seen.push({
              namespace: ctx.namespace,
              name: ctx.tool.name,
              ...(ctx.chatId === undefined ? {} : { chatId: ctx.chatId }),
            });
            return true;
          },
        },
      ],
    });
    expect(seen).toEqual([{ namespace: "alpha", name: "a", chatId: "chat-1" }]);
  });

  it("stages everything when a guard has no visibility opinion", async () => {
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a"), tool("b")])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [{ canExecute: () => ({ allowed: true }) }],
    });
    expect(staged.registry.size()).toBe(2);
  });

  it("turns a canExecute refusal into a failed result, never a throw", async () => {
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a")])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [
        {
          canExecute: () => ({ allowed: false, reason: "read-only session" }),
        },
      ],
    });
    const result = await staged.registry.get("a")!.execute(
      {
        runId: "run-1",
        bindings: [],
        limits: CTX.limits,
      },
      {},
    );
    expect(result.ok).toBe(false);
    expect(result.summary).toBe("read-only session");
    expect(result.modelData).toEqual({
      errorCode: TOOL_GUARD_REFUSED_CODE,
      errorMessage: "read-only session",
      phase: "guard",
      retryable: false,
    });
  });

  it("does not run the tool body when a guard refuses", async () => {
    let ran = false;
    const spy: AiTool = {
      ...tool("a"),
      async execute(ctx) {
        ran = true;
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
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [spy])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [{ canExecute: () => ({ allowed: false, reason: "no" }) }],
    });
    await staged.registry
      .get("a")!
      .execute({ runId: "run-1", bindings: [], limits: CTX.limits }, {});
    expect(ran).toBe(false);
  });

  it("evaluates canExecute at CALL time, not at staging", async () => {
    let allowed = true;
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a")])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [
        {
          canExecute: () =>
            allowed ? { allowed: true } : { allowed: false, reason: "spent" },
        },
      ],
    });
    const call = () =>
      staged.registry
        .get("a")!
        .execute({ runId: "run-1", bindings: [], limits: CTX.limits }, {});
    expect((await call()).ok).toBe(true);
    allowed = false;
    expect((await call()).ok).toBe(false);
  });
});

describe("stageRegistry — a guard that throws fails closed", () => {
  it("hides only the tool whose isVisible threw, and warns", async () => {
    const warnings: { message: string; fields?: Record<string, unknown> }[] =
      [];
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a"), tool("b")])],
      ctx: {
        ...CTX,
        logger: {
          debug: () => {},
          info: () => {},
          warn: (message, fields) => warnings.push({ message, fields }),
          error: () => {},
        },
      },
      hasPrimaryBinding: true,
      guards: [
        {
          isVisible: (ctx) => {
            if (ctx.tool.name === "a") throw new Error("policy store is down");
            return true;
          },
        },
      ],
    });

    // Fail-open here would advertise a tool the policy never approved.
    expect(staged.registry.listDefinitions().map((t) => t.name)).toEqual(["b"]);
    expect(warnings).toHaveLength(1);
    expect(warnings[0]?.fields).toMatchObject({
      tool: "a",
      namespace: "alpha",
      error: "policy store is down",
    });
  });

  it("hides a tool whose isVisible rejects, with no logger wired", async () => {
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a")])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [
        { isVisible: async () => Promise.reject(new Error("async boom")) },
      ],
    });
    expect(staged.registry.size()).toBe(0);
  });

  it("turns a canExecute that throws into a guard refusal, not an allow", async () => {
    let ran = false;
    const spy: AiTool = {
      ...tool("a"),
      async execute(ctx) {
        ran = true;
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
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [spy])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [
        {
          canExecute: () => {
            throw new Error("connection string: postgres://secret@host/db");
          },
        },
      ],
    });
    const result = await staged.registry
      .get("a")!
      .execute({ runId: "run-1", bindings: [], limits: CTX.limits }, {});

    expect(ran).toBe(false);
    expect(result.ok).toBe(false);
    expect(result.modelData).toEqual({
      errorCode: TOOL_GUARD_REFUSED_CODE,
      errorMessage: TOOL_GUARD_ERROR_MESSAGE,
      phase: "guard",
      retryable: false,
    });
    // The thrown text never reaches the model: a guard reason is fed verbatim.
    expect(JSON.stringify(result)).not.toContain("postgres://");
  });

  it("keeps the run going: another tool still executes after a guard threw", async () => {
    const staged = await stageRegistry({
      contributors: [contributor("alpha", [tool("a"), tool("b")])],
      ctx: CTX,
      hasPrimaryBinding: true,
      guards: [
        {
          canExecute: (ctx) => {
            if (ctx.tool.name === "a") throw new Error("boom");
            return { allowed: true };
          },
        },
      ],
    });
    const call = (name: string) =>
      staged.registry
        .get(name)!
        .execute({ runId: "run-1", bindings: [], limits: CTX.limits }, {});
    expect((await call("a")).ok).toBe(false);
    expect((await call("b")).ok).toBe(true);
  });
});

// C10 + 3.5: contributors fail closed PER CONTRIBUTOR, not per run. One
// unreachable MCP server used to take every turn in every chat down with it.
describe("stageRegistry — a contributor that fails contributes nothing", () => {
  it("keeps the other contributors' tools when one throws, and reports it", async () => {
    const warnings: { message: string; fields?: Record<string, unknown> }[] =
      [];
    const staged = await stageRegistry({
      contributors: [
        contributor("broken", [], {
          contribute: async () => {
            throw new Error("mcp server is down");
          },
        }),
        contributor("alpha", [tool("a"), tool("b")]),
      ],
      ctx: {
        ...CTX,
        logger: {
          debug: () => {},
          info: () => {},
          warn: (message, fields) => warnings.push({ message, fields }),
          error: () => {},
        },
      },
      hasPrimaryBinding: true,
    });

    expect(staged.registry.size()).toBe(2);
    expect(staged.registry.has("a")).toBe(true);
    expect(staged.failed).toEqual([
      { namespace: "broken", reason: "mcp server is down", timedOut: false },
    ]);
    expect(warnings[0]?.message).toBe(
      "tool contributor failed; contributing nothing",
    );
  });

  it("bounds a contributor that never answers, and marks it timed out", async () => {
    const staged = await stageRegistry({
      contributors: [
        contributor("hanging", [], { contribute: () => new Promise(() => {}) }),
        contributor("alpha", [tool("a")]),
      ],
      ctx: CTX,
      hasPrimaryBinding: true,
      contributeTimeoutMs: 15,
    });

    expect(staged.registry.size()).toBe(1);
    // `timedOut` is what lets the caller put `hook_timeout` on the durable log
    // for a slow dependency, while a thrown contributor stays a log line.
    expect(staged.failed).toEqual([
      {
        namespace: "hanging",
        reason: 'Host hook "contribute(hanging)" did not answer within 15 ms.',
        timedOut: true,
      },
    ]);
  });

  it("awaits a slow contributor unbounded when no deadline is configured", async () => {
    const staged = await stageRegistry({
      contributors: [
        contributor("slow", [], {
          contribute: async () => {
            await new Promise((resolve) => setTimeout(resolve, 5));
            return [tool("a")];
          },
        }),
      ],
      ctx: CTX,
      hasPrimaryBinding: true,
    });
    expect(staged.registry.has("a")).toBe(true);
    expect(staged.failed).toEqual([]);
  });
});
