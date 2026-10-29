import { expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import { MockProviderClient, createTestEventStamper } from "@agentkit/testing";
import type {
  AiChatRequest,
  AiProviderClient,
} from "../src/providers/client.js";
import { runChat } from "../src/runs/run-loop.js";
import { resolveToolLimits } from "../src/tools/limits.js";
import { AiToolRegistry } from "../src/tools/registry.js";
import { collectRun } from "./helpers.js";

it("retries only the provider request after a recorded tool effect, preserving model and tool result", async () => {
  const mock = new MockProviderClient();
  mock.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "count-once",
          name: "count",
          argumentsJson: "{}",
        },
      ],
      finishReason: "tool_calls",
    },
    { steps: [{ kind: "text", content: "done" }] },
  ]);
  const requests: AiChatRequest[] = [];
  const client: AiProviderClient = {
    id: "pinned-provider",
    kind: mock.kind,
    capabilities: () => mock.capabilities(),
    listModels: () => mock.listModels(),
    async *streamChat(input): AsyncIterable<AiRunEvent> {
      requests.push(input);
      if (requests.length === 2) {
        yield createTestEventStamper()({
          type: "run.failed",
          runId: input.runId,
          timestamp: new Date().toISOString(),
          data: { errorCode: "503", errorMessage: "injected after effect" },
        });
        return;
      }
      yield* mock.streamChat(input);
    },
  };
  let effects = 0;
  const registry = new AiToolRegistry();
  registry.register({
    definition: {
      name: "count",
      version: "1",
      effect: "write",
      capability: "test",
      description: "Count one effect",
      inputSchema: { type: "object", properties: {} },
    },
    async execute(ctx) {
      effects += 1;
      return {
        ok: true,
        data: { effects },
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  });
  const { events } = await collectRun(
    runChat({
      client,
      registry,
      model: "pinned-model",
      messages: [{ role: "user", content: "count once" }],
      limits: resolveToolLimits({ preference: "small" }),
      providerRetry: {
        maxAttempts: 2,
        baseDelayMs: 1,
        maxDelayMs: 1,
        random: () => 0.5,
        sleep: async () => {},
      },
    }),
  );
  expect(effects).toBe(1);
  expect(requests).toHaveLength(3);
  expect(requests.map((request) => request.model)).toEqual([
    "pinned-model",
    "pinned-model",
    "pinned-model",
  ]);
  expect(requests[2]!.messages).toEqual(requests[1]!.messages);
  expect(
    requests[2]!.messages.filter((message) => message.role === "tool"),
  ).toHaveLength(1);
  expect(
    events.filter((event) => event.type === "run.tool.succeeded"),
  ).toHaveLength(1);
  expect(events.at(-1)?.type).toBe("run.completed");
});
