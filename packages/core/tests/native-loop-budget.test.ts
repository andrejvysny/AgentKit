import { MockProviderClient } from "@agentkit/testing";
import type { AiProviderClient } from "../src/providers/client.js";
import { runAgentLoop } from "../src/runs/agent-loop.js";
import { ExecutionBudget } from "../src/runs/execution-budget.js";
import { nativeIt } from "./native-loop-helpers.js";
import { afterEach, describe, expect } from "bun:test";
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
  nativeIt("never dispatches a provider for a pre-aborted run", async () => {
    process.env.AGENTKIT_BACKEND = "native";
    const client = new MockProviderClient();
    const controller = new AbortController();
    controller.abort();
    const { result } = await collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [],
        limits: resolveToolLimits({ preference: "small" }),
        signal: controller.signal,
      }),
    );
    expect(result.terminal).toBe("cancelled");
    expect(client.callCount).toBe(0);
  });
  nativeIt.each(["random", "sleep"] as const)(
    "handles a throwing retry %s without silently repeating on random failure",
    async (failAt) => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      client.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
      const originalStream = client.streamChat.bind(client);
      let calls = 0;
      client.streamChat = (request) => {
        calls++;
        if (calls === 1)
          // biome-ignore lint/correctness/useYield: provider throws before producing events.
          return (async function* () {
            throw new Error("network");
          })();
        return originalStream(request);
      };
      const { result } = await collectRun(
        runChat({
          client,
          registry: new AiToolRegistry(),
          model: "m",
          messages: [],
          limits: resolveToolLimits({ preference: "small" }),
          providerRetry: {
            maxAttempts: 2,
            baseDelayMs: 1,
            maxDelayMs: 1,
            random: () => {
              if (failAt === "random") throw new Error("random failed");
              return 0.5;
            },
            sleep: async () => {
              if (failAt === "sleep") throw new Error("sleep failed");
            },
          },
        }),
      );
      expect(calls).toBe(failAt === "random" ? 1 : 2);
      expect(result.terminal).toBe(
        failAt === "random" ? "failed" : "completed",
      );
    },
  );
  nativeIt("resets the first-byte deadline after retry backoff", async () => {
    process.env.AGENTKIT_BACKEND = "native";
    const client = new MockProviderClient();
    client.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const original = client.streamChat.bind(client);
    let calls = 0;
    client.streamChat = (request) => {
      if (++calls === 1) {
        // biome-ignore lint/correctness/useYield: provider throws before producing events.
        return (async function* () {
          throw new Error("network");
        })();
      }
      return original(request);
    };
    const { result } = await collectRun(
      runChat({
        client,
        registry: new AiToolRegistry(),
        model: "m",
        messages: [],
        limits: resolveToolLimits({ preference: "small" }),
        budgets: { firstByteMs: 20, streamIdleMs: 20 },
        providerRetry: {
          maxAttempts: 2,
          baseDelayMs: 50,
          maxDelayMs: 50,
          random: () => 0.5,
        },
      }),
    );
    expect(calls).toBe(2);
    expect(result.terminal).toBe("completed");
  });
  nativeIt(
    "forwards tool metadata while preserving opaque values",
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
        { steps: [{ kind: "text", content: "ok" }] },
      ]);
      const registry = new AiToolRegistry();
      const opaque: { self?: unknown } = {};
      opaque.self = opaque;
      let seen: unknown;
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
          seen = context.metadata;
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
          limits: resolveToolLimits({ preference: "small" }),
          toolMetadata: { opaque, toolEventId: "forged" },
        }),
      );
      expect(seen).toMatchObject({ opaque, toolEventId: expect.any(String) });
      expect((seen as { toolEventId: string }).toolEventId).not.toBe("forged");
    },
  );
  nativeIt(
    "honors caller-owned budget checkpoints before provider effects",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      const budget = new ExecutionBudget(
        { providerRequests: 2 },
        {
          checkpoint: async () => {
            throw new Error("disk full");
          },
        },
      );
      try {
        const { result } = await collectRun(
          runChat({
            client,
            executionBudget: budget,
            registry: new AiToolRegistry(),
            model: "m",
            messages: [],
            limits: resolveToolLimits({ preference: "small" }),
          }),
        );
        expect(result).toMatchObject({
          terminal: "failed",
          iterations: 1,
          appendedMessages: [],
        });
        expect(client.callCount).toBe(0);
        expect(budget.snapshot().providerRequests).toBe(1);
      } finally {
        budget.dispose();
      }
    },
  );
  nativeIt(
    "honors the caller-owned budget timeout during a silent provider",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      client.streamChat = async function* () {
        await new Promise(() => {});
      };
      const budget = new ExecutionBudget({ overallMs: 15 });
      try {
        const { events, result } = await collectRun(
          runChat({
            client,
            executionBudget: budget,
            registry: new AiToolRegistry(),
            model: "m",
            messages: [],
            limits: resolveToolLimits({ preference: "small" }),
          }),
        );
        expect(result.terminal).toBe("failed");
        expect(events.at(-1)).toMatchObject({
          type: "run.failed",
          data: { errorCode: "execution_timeout" },
        });
      } finally {
        budget.dispose();
      }
    },
  );
  nativeIt("honors a caller-owned budget with a custom clock", async () => {
    process.env.AGENTKIT_BACKEND = "native";
    const client = new MockProviderClient();
    client.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const budget = new ExecutionBudget({ overallMs: 1000 }, { now: () => 100 });
    try {
      const { result } = await collectRun(
        runChat({
          client,
          executionBudget: budget,
          registry: new AiToolRegistry(),
          model: "m",
          messages: [],
          limits: resolveToolLimits({ preference: "small" }),
        }),
      );
      expect(result.terminal).toBe("completed");
      expect(budget.snapshot().providerRequests).toBe(1);
    } finally {
      budget.dispose();
    }
  });
  nativeIt(
    "does not dispatch a transport after cancellation during a budget checkpoint",
    async () => {
      process.env.AGENTKIT_BACKEND = "native";
      let entered = () => {};
      let release = () => {};
      const checkpointStarted = new Promise<void>((resolve) => {
        entered = resolve;
      });
      const checkpointRelease = new Promise<void>((resolve) => {
        release = resolve;
      });
      const controller = new AbortController();
      const budget = new ExecutionBudget(
        { providerRequests: 1 },
        {
          checkpoint: async () => {
            entered();
            await checkpointRelease;
          },
        },
      );
      let dispatches = 0;
      const client: AiProviderClient = {
        id: "tracked",
        kind: "openai-compatible",
        tracksTransportRequests: true,
        capabilities: async () => ({
          streaming: true,
          toolCalling: true,
          modelList: false,
        }),
        listModels: async () => [],
        // biome-ignore lint/correctness/useYield: dispatch is the observable action.
        async *streamChat(request) {
          await request.beforeRequest?.();
          dispatches++;
        },
      };
      try {
        const run = collectRun(
          runChat({
            client,
            executionBudget: budget,
            signal: controller.signal,
            registry: new AiToolRegistry(),
            model: "m",
            messages: [],
            limits: resolveToolLimits({ preference: "small" }),
          }),
        );
        await checkpointStarted;
        controller.abort();
        expect((await run).result.terminal).toBe("cancelled");
        release();
        await new Promise((resolve) => setTimeout(resolve, 10));
        expect(dispatches).toBe(0);
      } finally {
        release();
        budget.dispose();
      }
    },
  );
  nativeIt("preserves the external zero provider budget failure", async () => {
    process.env.AGENTKIT_BACKEND = "native";
    const client = new MockProviderClient();
    const budget = new ExecutionBudget({ providerRequests: 0 });
    try {
      const { events, result } = await collectRun(
        runChat({
          client,
          executionBudget: budget,
          registry: new AiToolRegistry(),
          model: "m",
          messages: [],
          limits: resolveToolLimits({ preference: "small" }),
        }),
      );
      expect(result).toMatchObject({
        terminal: "failed",
        iterations: 0,
        appendedMessages: [],
      });
      expect(events.at(-1)).toMatchObject({
        type: "run.failed",
        data: { errorCode: "execution_budget_exhausted" },
      });
      expect(client.callCount).toBe(0);
      expect(budget.snapshot().providerRequests).toBe(0);
    } finally {
      budget.dispose();
    }
  });
  nativeIt("preserves the external zero tool budget failure", async () => {
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
    ]);
    let effects = 0;
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
        effects++;
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
    const budget = new ExecutionBudget({ toolCalls: 0 });
    try {
      const { events, result } = await collectRun(
        runChat({
          client,
          executionBudget: budget,
          registry,
          model: "m",
          messages: [],
          limits: resolveToolLimits({ preference: "small" }),
        }),
      );
      expect(result).toMatchObject({
        terminal: "failed",
        iterations: 1,
        appendedMessages: [],
      });
      expect(events.at(-1)).toMatchObject({
        type: "run.failed",
        data: { errorCode: "execution_budget_exhausted" },
      });
      expect(effects).toBe(0);
      expect(budget.snapshot().toolCalls).toBe(0);
    } finally {
      budget.dispose();
    }
  });
  nativeIt.each([1, 4_294_967_297])(
    "preserves exhausted restored provider budget %s",
    async (count) => {
      process.env.AGENTKIT_BACKEND = "native";
      const client = new MockProviderClient();
      const budget = new ExecutionBudget(
        { providerRequests: count },
        {
          state: {
            version: 1,
            startedAt: Date.now(),
            providerRequests: count,
            toolCalls: 0,
            correctionPasses: 0,
            retries: 0,
          },
        },
      );
      try {
        const { events, result } = await collectRun(
          runChat({
            client,
            executionBudget: budget,
            registry: new AiToolRegistry(),
            model: "m",
            messages: [],
            limits: resolveToolLimits({ preference: "small" }),
          }),
        );
        expect(result).toMatchObject({
          terminal: "failed",
          iterations: count,
          appendedMessages: [],
        });
        expect(events.at(-1)).toMatchObject({
          type: "run.failed",
          data: { errorCode: "execution_budget_exhausted" },
        });
        expect(client.callCount).toBe(0);
        expect(budget.snapshot().providerRequests).toBe(count);
      } finally {
        budget.dispose();
      }
    },
  );
});
