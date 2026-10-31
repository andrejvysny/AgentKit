import { createTestEventStamper, MockProviderClient } from "@agentkit/testing";
import type { AiProviderClient } from "../src/providers/client.js";
import { runAgentLoop } from "../src/runs/agent-loop.js";
import { nativeIt } from "./native-loop-helpers.js";
import { afterEach, describe, expect, it } from "bun:test";
import { AiToolRegistry } from "../src/tools/registry.js";
import { runChat } from "../src/runs/run-loop.js";
import { collectRun } from "./helpers.js";
import { resolveToolLimits } from "../src/tools/limits.js";
const original = process.env.AGENTKIT_BACKEND;
afterEach(() => {
  if (original === undefined) delete process.env.AGENTKIT_BACKEND;
  else process.env.AGENTKIT_BACKEND = original;
});

describe("native run facade", () => {
  nativeIt(
    "preserves the caller signal for hooks and tools without deadlines",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const controller = new AbortController();
      const client = new MockProviderClient();
      client.setScript([
        {
          steps: [
            {
              kind: "tool_call",
              toolCallId: "call",
              name: "lookup",
              argumentsJson: "{}",
            },
          ],
        },
        { steps: [{ kind: "text", content: "ok" }] },
      ]);
      const registry = new AiToolRegistry();
      const observed: Array<AbortSignal | undefined> = [];
      registry.register({
        definition: {
          name: "lookup",
          version: "1",
          effect: "read",
          capability: "test",
          description: "lookup",
          inputSchema: { type: "object" },
        },
        async execute(context) {
          observed.push(context.signal);
          return {
            ok: true,
            data: {},
            sources: [],
            warnings: [],
            truncated: false,
            limits: context.limits,
          };
        },
      });
      await collectRun(
        runAgentLoop({
          client,
          registry,
          model: "m",
          messages: [],
          signal: controller.signal,
          limits: resolveToolLimits({ preference: "small" }),
          hooks: [
            {
              onRunStart: (context) => {
                observed.push(context.signal);
              },
            },
          ],
        }),
      );
      expect(observed).toEqual([controller.signal, controller.signal]);
      expect(controller.signal.aborted).toBe(false);
    },
  );
  nativeIt(
    "does not abort a timed-out observer signal after a successful run",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      client.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
      let observed: AbortSignal | undefined;
      const { result } = await collectRun(
        runAgentLoop({
          client,
          registry: new AiToolRegistry(),
          model: "m",
          messages: [],
          limits: resolveToolLimits({ preference: "small" }),
          budgets: { providerRequests: 1 },
          hookTimeoutMs: 5,
          hooks: [
            {
              onRunStart: (context) => {
                observed = context.signal;
                return new Promise<void>(() => {});
              },
            },
          ],
        }),
      );
      expect(result.terminal).toBe("completed");
      expect(observed?.aborted).toBe(false);
    },
  );
  nativeIt("aborts a timed-out tool signal", async () => {
    process.env.AGENTKIT_BACKEND = "native";
    const client = new MockProviderClient();
    client.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "call",
            name: "lookup",
            argumentsJson: "{}",
          },
        ],
      },
      { steps: [{ kind: "text", content: "ok" }] },
    ]);
    const registry = new AiToolRegistry();
    let aborted = false;
    registry.register({
      definition: {
        name: "lookup",
        version: "1",
        effect: "read",
        capability: "test",
        description: "lookup",
        timeoutMs: 5,
        inputSchema: { type: "object" },
      },
      async execute(context) {
        context.signal?.addEventListener("abort", () => {
          aborted = true;
        });
        await new Promise(() => {});
        return {
          ok: true,
          data: {},
          sources: [],
          warnings: [],
          truncated: false,
          limits: context.limits,
        };
      },
    });
    const { result } = await collectRun(
      runChat({
        client,
        registry,
        model: "m",
        messages: [],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    );
    expect(result.terminal).toBe("completed");
    expect(aborted).toBe(true);
  });
  nativeIt(
    "preserves delayed tool completion after caller cancellation",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const controller = new AbortController();
      const client = new MockProviderClient();
      client.setScript([
        {
          steps: [
            {
              kind: "tool_call",
              toolCallId: "call",
              name: "lookup",
              argumentsJson: "{}",
            },
          ],
        },
      ]);
      const registry = new AiToolRegistry();
      let finished = false;
      registry.register({
        definition: {
          name: "lookup",
          version: "1",
          effect: "write",
          capability: "test",
          description: "lookup",
          inputSchema: { type: "object" },
        },
        async execute(context) {
          controller.abort();
          await new Promise((resolve) => setTimeout(resolve, 100));
          finished = true;
          return {
            ok: true,
            data: { applied: true },
            sources: [],
            warnings: [],
            truncated: false,
            limits: context.limits,
          };
        },
      });
      const { events, result } = await collectRun(
        runChat({
          client,
          registry,
          model: "m",
          messages: [],
          signal: controller.signal,
          limits: resolveToolLimits({ preference: "small" }),
        }),
      );
      expect(finished).toBe(true);
      expect(events.some((event) => event.type === "run.tool.succeeded")).toBe(
        true,
      );
      expect(result.terminal).toBe("cancelled");
    },
  );
  nativeIt(
    "bounds a cancelled uncooperative tool by its declared deadline",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const controller = new AbortController();
      const client = new MockProviderClient();
      client.setScript([
        {
          steps: [
            {
              kind: "tool_call",
              toolCallId: "call",
              name: "lookup",
              argumentsJson: "{}",
            },
          ],
        },
      ]);
      const registry = new AiToolRegistry();
      registry.register({
        definition: {
          name: "lookup",
          version: "1",
          effect: "write",
          capability: "test",
          description: "lookup",
          timeoutMs: 30,
          inputSchema: { type: "object" },
        },
        async execute(context) {
          controller.abort();
          await new Promise(() => {});
          return {
            ok: true,
            data: {},
            sources: [],
            warnings: [],
            truncated: false,
            limits: context.limits,
          };
        },
      });
      const { events, result } = await collectRun(
        runChat({
          client,
          registry,
          model: "m",
          messages: [],
          signal: controller.signal,
          limits: resolveToolLimits({ preference: "small" }),
        }),
      );
      expect(events.some((event) => event.type === "run.tool.failed")).toBe(
        true,
      );
      expect(result.terminal).toBe("cancelled");
    },
  );
  nativeIt.each([false, true])(
    "waits for consumer acknowledgement before provider dispatch (retry=%s)",
    async (retry) => {
      process.env.AGENTKIT_BACKEND = "native";
      const controller = new AbortController();
      let dispatches = 0;
      const client: AiProviderClient = {
        id: "started",
        kind: "openai-compatible",
        capabilities: async () => ({
          streaming: true,
          toolCalling: true,
          modelList: false,
        }),
        listModels: async () => [],
        async *streamChat(request) {
          const stamp = createTestEventStamper();
          yield stamp({
            type: "run.started",
            runId: request.runId,
            timestamp: new Date().toISOString(),
            data: { model: request.model, toolCount: 0 },
          });
          dispatches++;
          yield stamp({
            type: "run.message.completed",
            runId: request.runId,
            timestamp: new Date().toISOString(),
            data: { content: "ok", toolCallCount: 0 },
          });
        },
      };
      const run = runChat({
        client,
        signal: controller.signal,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [],
        limits: resolveToolLimits({ preference: "small" }),
        ...(retry
          ? { providerRetry: { maxAttempts: 2, baseDelayMs: 1, maxDelayMs: 1 } }
          : {}),
      });
      const first = await run.next();
      expect(first.done ? undefined : first.value.type).toBe("run.started");
      controller.abort();
      while (!(await run.next()).done) {}
      expect(dispatches).toBe(0);
    },
  );
  it("rejects invalid backend selection", () => {
    process.env.AGENTKIT_BACKEND = "invalid";
    expect(() =>
      runChat({
        client: new MockProviderClient(),
        registry: new AiToolRegistry(),
        model: "m",
        messages: [],
        limits: resolveToolLimits({ preference: "small" }),
      }),
    ).toThrow("Invalid AGENTKIT_BACKEND");
  });
});
