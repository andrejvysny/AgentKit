import { expect, test } from "bun:test";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import { createProviderCallback } from "../src/provider-callback.js";

test("custom provider adapter carries native effort, content, cancellation, usage and dispatch check", async () => {
  const mock = new MockProviderClient();
  mock.emitUsage = true;
  mock.setScript([{ steps: [{ kind: "text", content: "answer" }] }]);
  let seen: AiChatRequest | undefined;
  let checks = 0;
  const client: AiProviderClient = {
    id: mock.id,
    kind: mock.kind,
    capabilities: () => mock.capabilities(),
    listModels: () => mock.listModels(),
    async *streamChat(input) {
      seen = input;
      await input.beforeRequest?.();
      yield* mock.streamChat(input);
    },
  };
  const signal = new AbortController().signal;
  const runContent = {
    messages: [
      { role: "system" as const, content: "native selected instruction" },
    ],
  };
  const result = await createProviderCallback(client)({
    runId: "run",
    model: "model",
    messages: [{ role: "user", content: "hello" }],
    tools: [],
    signal,
    runContent,
    reasoningEffort: "high",
    checkAuthority: async () => {
      checks++;
    },
  });
  expect(seen?.signal).toBe(signal);
  expect(seen?.reasoningEffort).toBe("high");
  expect(seen?.runContent).toBe(runContent);
  expect(checks).toBe(2);
  expect(result).toMatchObject({
    content: "answer",
    usage: { totalTokens: 15 },
  });
});

test("custom provider adapter refuses revoked authority before invoking the client", async () => {
  const mock = new MockProviderClient();
  await expect(
    createProviderCallback(mock)({
      runId: "run",
      model: "model",
      messages: [],
      tools: [],
      checkAuthority: async () => {
        throw new Error("selected_content_revoked");
      },
    }),
  ).rejects.toThrow("selected_content_revoked");
  expect(mock.callCount).toBe(0);
});
