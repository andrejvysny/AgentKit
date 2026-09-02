import { describe, expect, it } from "bun:test";
import type { AiContextBinding } from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import {
  defaultClock,
  defaultIds,
  type ContextProvider,
  type ToolGuard,
  type ToolSetContributor,
} from "@agentkit/host";
import { createStagedToolSource, EXEC_FAILED_TEXT } from "../src/index.js";
import { demoContributor, echoTool } from "./helpers.js";

const PRIMARY: AiContextBinding = {
  id: "bind-1",
  kind: "document",
  refId: "doc-1",
  role: "primary",
  status: "active",
  label: "Doc",
};

function source(
  contributors: ToolSetContributor[],
  extra: {
    context?: ContextProvider;
    guards?: ToolGuard[];
  } = {},
) {
  return createStagedToolSource({
    contributors,
    clock: defaultClock,
    ids: defaultIds,
    ...extra,
  });
}

describe("createStagedToolSource", () => {
  it("lists what the contributors stage, per scope", async () => {
    const tools = source([demoContributor()]);
    const unscoped = await tools.catalog.listTools();
    expect(unscoped.map((e) => e.definition.name)).toEqual([
      "demo_echo",
      "demo_fail",
      "demo_write",
    ]);
    const scoped = await tools.catalog.listTools({ chatId: "chat-a" });
    expect(scoped.map((e) => e.definition.name)).toContain("demo_only_chat-a");
  });

  it("executes through the registry and returns an envelope", async () => {
    const tools = source([demoContributor()]);
    const envelope = await tools.execute("demo_echo", { text: "hi" });
    expect(envelope).toEqual({
      ok: true,
      status: "ok",
      summary: "echoed 2 char(s)",
      warnings: [],
      truncated: false,
      // `modelData`, not the fuller `data` — same projection a chat turn gets.
      data: { echo: "hi" },
    });
  });

  it("validates arguments against the tool's own inputSchema", async () => {
    const tools = source([demoContributor()]);
    const envelope = await tools.execute("demo_echo", { text: 42 });
    expect(envelope.ok).toBe(false);
    expect(envelope.data).toMatchObject({
      errorCode: "schema_invalid",
      phase: "validation",
      retryable: true,
    });
  });

  it("refuses an unknown tool with a structured envelope", async () => {
    const tools = source([demoContributor()]);
    const envelope = await tools.execute("nope", {});
    expect(envelope.data).toMatchObject({ errorCode: "tool_not_found" });
  });

  it("turns a thrown tool into a failure envelope, not an exception", async () => {
    const throwing: AiTool = {
      definition: {
        ...echoTool().definition,
        name: "demo_throw",
        inputSchema: { type: "object", properties: {} },
      },
      async execute() {
        throw new Error("boom");
      },
    };
    const tools = source([
      { namespace: "demo", contribute: async () => [throwing] },
    ]);
    const envelope = await tools.execute("demo_throw", {});
    expect(envelope.data).toMatchObject({
      errorCode: "exec_failed",
      phase: "execution",
      retryable: false,
    });
    // The thrower's own sentence is NOT forwarded — it went to the logger,
    // under the correlation id this message carries. See F13.
    const { errorMessage } = envelope.data as { errorMessage: string };
    expect(errorMessage).not.toContain("boom");
    expect(errorMessage).toContain(EXEC_FAILED_TEXT);
  });

  it("runs the SAME guard chain the turn runner would", async () => {
    const guard: ToolGuard = {
      isVisible: (ctx) => ctx.tool.name !== "demo_write",
      canExecute: (ctx) =>
        ctx.tool.name === "demo_fail"
          ? { allowed: false, reason: "not today" }
          : { allowed: true },
    };
    const tools = source([demoContributor()], { guards: [guard] });

    // isVisible removed it from the catalogue...
    const listed = await tools.catalog.listTools();
    expect(listed.map((e) => e.definition.name)).not.toContain("demo_write");
    // ...and from the executable registry, so the call path agrees.
    expect((await tools.execute("demo_write", {})).data).toMatchObject({
      errorCode: "tool_not_found",
    });
    // canExecute refuses at call time, as a guard-phase failure.
    expect((await tools.execute("demo_fail", {})).data).toMatchObject({
      phase: "guard",
    });
  });

  it("resolves bindings for the scope's chat", async () => {
    const seen: string[] = [];
    const context: ContextProvider = {
      async listBindings(chatId) {
        seen.push(chatId);
        return [PRIMARY];
      },
    };
    const bindingTool: AiTool = {
      definition: {
        ...echoTool().definition,
        name: "demo_bindings",
        inputSchema: { type: "object", properties: {} },
      },
      async execute(ctx) {
        return {
          ok: true,
          data: { bindings: ctx.bindings.map((b) => b.id), chatId: ctx.chatId },
          sources: [],
          warnings: [],
          truncated: false,
          limits: ctx.limits,
        };
      },
    };
    const tools = source(
      [{ namespace: "demo", contribute: async () => [bindingTool] }],
      { context },
    );
    const envelope = await tools.execute(
      "demo_bindings",
      {},
      {
        chatId: "chat-a",
      },
    );
    expect(envelope.data).toEqual({ bindings: ["bind-1"], chatId: "chat-a" });
    expect(seen).toEqual(["chat-a"]);
  });
});

describe("createStagedToolSource call deadline", () => {
  /** A tool that never settles, however politely it is asked to stop. */
  function hangingTool(seen: { aborted: boolean }): AiTool {
    return {
      definition: {
        ...echoTool().definition,
        name: "demo_hang",
        inputSchema: { type: "object", properties: {} },
      },
      execute(ctx) {
        ctx.signal?.addEventListener("abort", () => {
          seen.aborted = true;
        });
        return new Promise(() => {});
      },
    };
  }

  it("answers with a timeout envelope instead of pinning the session", async () => {
    const seen = { aborted: false };
    const tools = createStagedToolSource({
      contributors: [
        { namespace: "demo", contribute: async () => [hangingTool(seen)] },
      ],
      clock: defaultClock,
      ids: defaultIds,
      maxCallMs: 20,
    });

    const envelope = await tools.execute("demo_hang", {});
    expect(envelope.ok).toBe(false);
    expect(envelope.data).toMatchObject({
      errorCode: "timeout",
      phase: "execution",
    });
    // The tool's signal is aborted too, so a tool that DOES watch it stops.
    expect(seen.aborted).toBe(true);
  });

  it("leaves a tool that answers within the deadline alone", async () => {
    const tools = createStagedToolSource({
      contributors: [demoContributor()],
      clock: defaultClock,
      ids: defaultIds,
      maxCallMs: 5_000,
    });
    const envelope = await tools.execute("demo_echo", { text: "hi" });
    expect(envelope.ok).toBe(true);
  });
});
