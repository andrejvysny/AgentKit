import { MockProviderClient } from "@agentkit/testing";
import { nativeIt, drainRaw } from "./native-loop-helpers.js";
import { afterEach, describe, expect } from "bun:test";
import { AiToolRegistry } from "../src/tools/registry.js";
import { runAgentLoop } from "../src/runs/agent-loop.js";
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
    "streams provider events and returns the Rust run result",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      client.setScript([{ steps: [{ kind: "text", content: "hello" }] }]);
      const iterator = runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [],
        limits: resolveToolLimits({ preference: "small" }),
      });
      const events = [];
      let next = await iterator.next();
      while (!next.done) {
        events.push(next.value);
        next = await iterator.next();
      }
      expect(events.some((event) => event.type === "run.message.delta")).toBe(
        true,
      );
      expect(next.value).toMatchObject({
        terminal: "completed",
        appendedMessages: [{ role: "assistant", content: "hello" }],
        iterations: 1,
      });
    },
  );
  nativeIt(
    "executes a registered JavaScript tool through the native loop",
    async () => {
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
        { steps: [{ kind: "text", content: "done" }] },
      ]);
      const registry = new AiToolRegistry();
      let calls = 0;
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
          calls++;
          return {
            ok: true,
            data: { found: true },
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
          limits: resolveToolLimits({ preference: "small" }),
        }),
      );
      expect(calls).toBe(1);
      expect(events.some((event) => event.type === "run.tool.succeeded")).toBe(
        true,
      );
      expect(result.terminal).toBe("completed");
      expect(result.iterations).toBe(2);
    },
  );
  nativeIt("cancels a silent provider", async () => {
    process.env.AGENTKIT_BACKEND = "native";
    const controller = new AbortController();
    const client = new MockProviderClient();
    client.streamChat = async function* () {
      await new Promise(() => {});
    };
    const run = collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [],
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
    );
    setTimeout(() => controller.abort(), 20);
    expect((await run).result.terminal).toBe("cancelled");
  });
  nativeIt("enforces first-byte budget on a silent provider", async () => {
    process.env.AGENTKIT_BACKEND = "native";
    const client = new MockProviderClient();
    client.streamChat = async function* () {
      await new Promise(() => {});
    };
    const { events, result } = await collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [],
        limits: resolveToolLimits({ preference: "small" }),
        budgets: { firstByteMs: 10 },
      }),
    );
    expect(result.terminal).toBe("failed");
    expect(events.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "execution_timeout" },
    });
  });
  nativeIt(
    "forwards selected run content and reasoning effort to the provider",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      client.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
      const originalStream = client.streamChat.bind(client);
      let observed: Parameters<typeof client.streamChat>[0] | undefined;
      client.streamChat = (request) => {
        observed = request;
        return originalStream(request);
      };
      await collectRun(
        runChat({
          client,
          registry: new AiToolRegistry(),
          model: "m",
          messages: [],
          limits: resolveToolLimits({ preference: "small" }),
          reasoningEffort: "high",
          runContent: { messages: [{ role: "user", content: "selected" }] },
        }),
      );
      expect(observed?.reasoningEffort).toBe("high");
      expect(observed?.runContent).toEqual({
        messages: [{ role: "user", content: "selected" }],
      });
    },
  );
  nativeIt(
    "does not add transport callbacks to an unbounded tool-less request",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      client.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
      const original = client.streamChat.bind(client);
      let request: Parameters<typeof client.streamChat>[0] | undefined;
      client.streamChat = (value) => {
        request = structuredClone(value);
        return original(value);
      };
      const { result } = await collectRun(
        runChat({
          client,
          registry: new AiToolRegistry(),
          model: "m",
          messages: [],
          limits: { maxItems: 10, maxBytes: 1000 } as never,
        }),
      );
      expect(result.terminal).toBe("completed");
      expect(request?.onActivity).toBeUndefined();
      expect(request?.beforeRequest).toBeUndefined();
      expect(request?.tools).toBeUndefined();
      expect(request?.signal).toBeUndefined();
      expect(Object.keys(request ?? {}).sort()).toEqual(
        [
          "continuation",
          "continuationRequired",
          "continuationScope",
          "maxOutputTokens",
          "messages",
          "model",
          "onContinuation",
          "reasoningEffort",
          "runContent",
          "runId",
          "signal",
          "temperature",
          "tools",
        ].sort(),
      );
    },
  );
  nativeIt(
    "does not announce an already-declared resumed tool call",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      client.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
      const registry = new AiToolRegistry();
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
      const call = { id: "call", name: "lookup", argumentsJson: "{}" };
      const { events, result } = await collectRun(
        runAgentLoop({
          client,
          registry,
          model: "m",
          messages: [{ role: "assistant", content: "", toolCalls: [call] }],
          resumeCalls: [call],
          limits: resolveToolLimits({ preference: "small" }),
        }),
      );
      expect(result.terminal).toBe("completed");
      expect(events.some((event) => event.type === "run.tool.requested")).toBe(
        false,
      );
    },
  );
  nativeIt(
    "closes pending tool callbacks without accepting late resolution",
    async () => {
      const native = await import("@agentkit/native");
      for (let i = 0; i < 10; i++) {
        let deliverId: (id: string) => void = () => {};
        const toolId = new Promise<string>((resolve) => {
          deliverId = resolve;
        });
        const handle = await native.NativeRunHandle.start(
          JSON.stringify({
            model: "m",
            messages: [],
            limits: resolveToolLimits({ preference: "small" }),
            tools: [
              {
                name: "lookup",
                version: "1",
                effect: "read",
                capability: "test",
                description: "lookup",
                inputSchema: { type: "object" },
              },
            ],
          }),
          (error, raw) => {
            if (error) throw error;
            const call = JSON.parse(raw) as { kind: string; callId: string };
            if (call.kind === "tool") deliverId(call.callId);
            if (call.kind === "provider")
              void (async () => {
                await native.pushRunProviderEvent(
                  call.callId,
                  JSON.stringify({ type: "run.started", data: {} }),
                );
                await native.pushRunProviderEvent(
                  call.callId,
                  JSON.stringify({
                    type: "run.tool.requested",
                    data: {
                      toolCallId: "call",
                      toolName: "lookup",
                      argumentsJson: "{}",
                    },
                  }),
                );
                await native.pushRunProviderEvent(
                  call.callId,
                  JSON.stringify({
                    type: "run.message.completed",
                    data: { content: "", toolCallCount: 1 },
                  }),
                );
                native.endRunProvider(call.callId);
              })();
          },
        );
        const pump = drainRaw(handle);
        const id = await toolId;
        handle.close();
        expect(native.resolveCall(id, "{}", null)).toBe(false);
        await pump;
      }
    },
  );
  nativeIt(
    "marks a tool callback inactive when its deadline expires before delivery",
    async () => {
      const native = await import("@agentkit/native");
      let deliver: (active: boolean) => void = () => {};
      const activity = new Promise<boolean>((resolve) => {
        deliver = resolve;
      });
      const handle = await native.NativeRunHandle.start(
        JSON.stringify({
          model: "m",
          messages: [],
          limits: resolveToolLimits({ preference: "small" }),
          tools: [
            {
              name: "lookup",
              version: "1",
              effect: "read",
              capability: "test",
              description: "lookup",
              timeoutMs: 5,
              inputSchema: { type: "object" },
            },
          ],
        }),
        (error, raw) => {
          if (error) throw error;
          const call = JSON.parse(raw) as { kind: string; callId: string };
          if (call.kind === "tool")
            deliver(native.isRunCallbackActive(call.callId));
          if (call.kind === "provider")
            void (async () => {
              await native.pushRunProviderEvent(
                call.callId,
                JSON.stringify({ type: "run.started", data: {} }),
              );
              await native.pushRunProviderEvent(
                call.callId,
                JSON.stringify({
                  type: "run.tool.requested",
                  data: {
                    toolCallId: "call",
                    toolName: "lookup",
                    argumentsJson: "{}",
                  },
                }),
              );
              await native.pushRunProviderEvent(
                call.callId,
                JSON.stringify({
                  type: "run.message.completed",
                  data: { content: "", toolCallCount: 1 },
                }),
              );
              native.endRunProvider(call.callId);
            })();
        },
      );
      const pump = drainRaw(handle, true);
      expect(await activity).toBe(false);
      handle.close();
      await pump;
    },
  );
});
