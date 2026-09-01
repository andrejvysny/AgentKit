import { describe, expect, it } from "bun:test";
import type { AiContextBinding } from "@agentkit/contracts";
import { resolveToolLimits, type AiTool } from "@agentkit/core";
import {
  createContributorToolCatalog,
  stageRegistry,
  type ContextProvider,
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

const PRIMARY: AiContextBinding = {
  id: "bind-1",
  kind: "document",
  refId: "doc-1",
  role: "primary",
  status: "active",
  label: "Doc",
};

/** Both tools, one of which survives with no primary binding. */
function notesContributor(): ToolSetContributor {
  return {
    namespace: "notes",
    contribute: async () => [tool("notes_read"), tool("notes_write")],
    unboundToolNames: () => ["notes_read"],
  };
}

function contextWith(bindings: AiContextBinding[]): ContextProvider {
  return { listBindings: async () => bindings };
}

describe("createContributorToolCatalog", () => {
  it("reports the tool a bound chat's next turn would get", async () => {
    const catalog = createContributorToolCatalog({
      contributors: [notesContributor()],
      context: contextWith([PRIMARY]),
    });
    expect(await catalog.listTools({ chatId: "chat-1" })).toEqual([
      { namespace: "notes", definition: tool("notes_read").definition },
      { namespace: "notes", definition: tool("notes_write").definition },
    ]);
  });

  it("agrees with stageRegistry for that chat, minus the executables", async () => {
    const contributors = [notesContributor()];
    const catalog = createContributorToolCatalog({
      contributors,
      context: contextWith([PRIMARY]),
    });
    const staged = await stageRegistry({
      contributors,
      ctx: {
        chatId: "chat-1",
        bindings: [PRIMARY],
        limits: resolveToolLimits({ preference: "small" }),
      },
      hasPrimaryBinding: true,
    });
    const entries = await catalog.listTools({ chatId: "chat-1" });
    expect(entries.map((e) => e.definition)).toEqual(
      staged.registry.listDefinitions(),
    );
    // Definitions only: a catalogue entry is never a way to CALL a tool.
    for (const entry of entries) {
      expect(Object.keys(entry).sort()).toEqual(["definition", "namespace"]);
    }
  });

  it("applies the unbound rules for an unbound chat", async () => {
    const catalog = createContributorToolCatalog({
      contributors: [notesContributor()],
      context: contextWith([]),
    });
    const entries = await catalog.listTools({ chatId: "chat-1" });
    expect(entries.map((e) => e.definition.name)).toEqual(["notes_read"]);
  });

  it("uses the unbound rules — and no bindings — with no chatId at all", async () => {
    let asked = 0;
    const catalog = createContributorToolCatalog({
      contributors: [notesContributor()],
      context: {
        listBindings: async () => {
          asked += 1;
          return [PRIMARY];
        },
      },
    });
    const entries = await catalog.listTools();
    expect(entries.map((e) => e.definition.name)).toEqual(["notes_read"]);
    // No chat named means no chat's bindings — not "some chat's", and not a
    // fabricated one.
    expect(asked).toBe(0);
  });

  it("passes no chatId to the contributor when none was given", async () => {
    const seen: (string | undefined)[] = [];
    const catalog = createContributorToolCatalog({
      contributors: [
        {
          namespace: "notes",
          contribute: async (ctx) => {
            seen.push(ctx.chatId);
            return [tool("notes_read")];
          },
        },
      ],
    });
    await catalog.listTools();
    await catalog.listTools({ chatId: "chat-1" });
    expect(seen).toEqual([undefined, "chat-1"]);
  });

  it("works with no ContextProvider wired", async () => {
    const catalog = createContributorToolCatalog({
      contributors: [notesContributor()],
    });
    const entries = await catalog.listTools({ chatId: "chat-1" });
    expect(entries.map((e) => e.definition.name)).toEqual(["notes_read"]);
  });

  it("hides from the catalogue exactly what the guards hide from a run", async () => {
    const catalog = createContributorToolCatalog({
      contributors: [notesContributor()],
      context: contextWith([PRIMARY]),
      guards: [{ isVisible: (ctx) => ctx.tool.name !== "notes_write" }],
    });
    const entries = await catalog.listTools({ chatId: "chat-1" });
    expect(entries.map((e) => e.definition.name)).toEqual(["notes_read"]);
  });

  it("fails closed on the same wiring faults staging does", async () => {
    const catalog = createContributorToolCatalog({
      contributors: [
        { namespace: "chat", contribute: async () => [tool("a")] },
      ],
    });
    await expect(catalog.listTools()).rejects.toThrow(/reserved/i);
  });
});
