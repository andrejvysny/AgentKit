import { afterEach, expect, it } from "bun:test";
import { defaultClock, defaultIds } from "@agentkit/host";
import { MockProviderClient } from "@agentkit/testing";
import {
  createMcpServerHandler,
  createStagedToolSource,
  type McpServerHandler,
  type McpSessionScope,
} from "../src/index.js";
import {
  authHeaders,
  connectClient,
  demoContributor,
  serveHandler,
  TEST_TOKEN,
  type ServedHandler,
} from "./helpers.js";

const opened: { handler: McpServerHandler; served: ServedHandler }[] = [];
afterEach(async () => {
  for (const { handler, served } of opened.splice(0)) {
    await handler.dispose();
    await served.stop();
  }
});

it("advertises and serves only host-supplied resources and prompts, without a provider call", async () => {
  const provider = new MockProviderClient();
  const seen: McpSessionScope[] = [];
  const handler = createMcpServerHandler({
    auth: { bearerToken: TEST_TOKEN },
    tools: createStagedToolSource({
      contributors: [demoContributor()],
      clock: defaultClock,
      ids: defaultIds,
    }),
    sessionScope: () => ({ chatId: "chat-1", principal: "user-1" }),
    resources: {
      list: async (scope) => {
        seen.push(scope);
        return [{ uri: "host://status", name: "status" }];
      },
      read: async (uri, scope) => {
        seen.push(scope);
        return { contents: [{ uri, text: "ready" }] };
      },
    },
    prompts: {
      list: async (scope) => {
        seen.push(scope);
        return [
          { name: "review", arguments: [{ name: "subject", required: true }] },
        ];
      },
      get: async (_name, args, scope) => {
        seen.push(scope);
        return {
          messages: [
            {
              role: "user",
              content: { type: "text", text: `Review ${args.subject}` },
            },
          ],
        };
      },
    },
  });
  const served = serveHandler(handler);
  opened.push({ handler, served });
  const { client, transport } = await connectClient(served.url, authHeaders());
  expect(client.getServerCapabilities()).toMatchObject({
    tools: {},
    resources: {},
    prompts: {},
  });
  expect(
    (await client.listResources()).resources.map((resource) => resource.uri),
  ).toEqual(["host://status"]);
  expect(
    (await client.readResource({ uri: "host://status" })).contents,
  ).toEqual([{ uri: "host://status", text: "ready" }]);
  await expect(client.readResource({ uri: "host://hidden" })).rejects.toThrow(
    "Unknown resource",
  );
  expect(
    (await client.listPrompts()).prompts.map((prompt) => prompt.name),
  ).toEqual(["review"]);
  const prompt = await client.getPrompt({
    name: "review",
    arguments: { subject: "board" },
  });
  expect(prompt.messages[0]!.content).toEqual({
    type: "text",
    text: "Review board",
  });
  await expect(client.getPrompt({ name: "hidden" })).rejects.toThrow(
    "Unknown prompt",
  );
  await expect(client.getPrompt({ name: "review" })).rejects.toThrow(
    "Invalid prompt arguments",
  );
  await expect(
    client.getPrompt({
      name: "review",
      arguments: { subject: "board", chatId: "other" },
    }),
  ).rejects.toThrow("Invalid prompt arguments");
  await client.listTools();
  await client.callTool({ name: "demo_echo", arguments: { text: "hello" } });
  expect(provider.callCount).toBe(0);
  expect(
    seen.every(
      (scope) =>
        scope.actorId === transport.sessionId &&
        scope.chatId === "chat-1" &&
        scope.principal === "user-1",
    ),
  ).toBe(true);
  await client.close();
});

it("keeps extension faults private and does not advertise absent extension seams", async () => {
  const tools = createStagedToolSource({
    contributors: [],
    clock: defaultClock,
    ids: defaultIds,
  });
  const handler = createMcpServerHandler({
    tools,
    auth: { bearerToken: TEST_TOKEN },
    resources: {
      list: async () => {
        throw new Error("secret database path");
      },
      read: async () => ({ contents: [] }),
    },
  });
  const served = serveHandler(handler);
  opened.push({ handler, served });
  const { client } = await connectClient(served.url, authHeaders());
  expect(client.getServerCapabilities()?.prompts).toBeUndefined();
  await expect(client.listPrompts()).rejects.toThrow();
  await expect(client.listResources()).rejects.toThrow(
    "Host MCP extension failed",
  );
  expect((await client.listTools()).tools).toEqual([]);
  await client.close();
});
