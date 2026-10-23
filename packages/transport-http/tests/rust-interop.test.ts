import { afterEach, beforeEach, expect, test } from "bun:test";
import { Value } from "@sinclair/typebox/value";
import {
  AiRunEventSchema,
  ChatDtoSchema,
  MessagePageDtoSchema,
  ModelDtoSchema,
  ProposalDtoSchema,
  ProviderDtoSchema,
  REST_ROUTES,
  SettingsDtoSchema,
  VersionDtoSchema,
  WriteAllowanceListResponseSchema,
  type RestOperation,
} from "@agentkit/contracts";
import {
  startNativeServer,
  type NativeServer,
} from "../../../rust/tests/support/http-server.js";
let server: NativeServer;
beforeEach(async () => {
  server = await startNativeServer();
}, 60_000);
afterEach(async () => {
  await server?.stop();
});

test("all 39 REST operations exercise real native host, SQLite, configured domain ports", async () => {
  const client = server.client;
  const covered = new Set<RestOperation>();
  const call = async <T>(
    name: RestOperation,
    promise: Promise<T>,
  ): Promise<T> => {
    covered.add(name);
    return promise;
  };
  const version = await call("getVersion", client.getVersion());
  expect(Value.Check(VersionDtoSchema, version)).toBe(true);
  expect(version.restApiVersion).toBe("v1");
  const chat = await call(
    "createChat",
    client.createChat({ title: "Native routes" }),
  );
  expect(Value.Check(ChatDtoSchema, chat)).toBe(true);
  expect(
    (await call("listChats", client.listChats())).some(
      (item) => item.id === chat.id,
    ),
  ).toBe(true);
  expect(
    (await call("getChat", client.getChat({ chatId: chat.id }))).activeRunId,
  ).toBeNull();
  expect(
    (
      await call(
        "updateChat",
        client.updateChat({ chatId: chat.id }, { title: "Renamed" }),
      )
    ).title,
  ).toBe("Renamed");
  const turn = await call(
    "submitMessage",
    client.submitMessage({ chatId: chat.id }, { content: "question" }),
  );
  covered.add("streamRun");
  const events = [];
  for await (const event of client.streamRun(turn.result.runId)) {
    expect(Value.Check(AiRunEventSchema, event)).toBe(true);
    events.push(event);
  }
  expect(events.map((event) => event.seq)).toEqual(
    events.map((_event, index) => index),
  );
  expect(
    (await call("getRun", client.getRun({ runId: turn.result.runId }))).status,
  ).toBe("completed");
  const page = await call(
    "listMessages",
    client.listMessages({ chatId: chat.id }),
  );
  expect(Value.Check(MessagePageDtoSchema, page)).toBe(true);
  expect(page.items[1]?.content).toBe("Hello, client.");
  const regenerated = await call(
    "regenerateMessage",
    client.regenerateMessage({
      chatId: chat.id,
      messageId: turn.result.assistantMessageId,
    }),
  );
  for await (const _event of client.streamRun(regenerated.result.runId)) {
    /* drain */
  }
  expect(
    await call(
      "listSiblings",
      client.listSiblings({ messageId: turn.result.assistantMessageId }),
    ),
  ).toHaveLength(2);
  const active = await call(
    "activateBranch",
    client.activateBranch({ messageId: turn.result.assistantMessageId }),
  );
  expect(active.items.at(-1)?.id).toBe(turn.result.assistantMessageId);
  const fork = await call(
    "forkChat",
    client.forkChat(
      { chatId: chat.id },
      { fromMessageId: turn.result.assistantMessageId },
    ),
  );
  expect(
    (await client.listMessages({ chatId: fork.id })).items.map(
      (item) => item.content,
    ),
  ).toEqual(["question", "Hello, client."]);
  expect(
    (
      await call(
        "searchMessages",
        client.searchMessages({ q: "question", chatId: chat.id }),
      )
    ).hits.length,
  ).toBeGreaterThan(0);
  expect(
    (await client.searchMessages({ q: "question", chatId: chat.id })).hits[0]
      ?.snippet,
  ).toContain("[question]");
  expect(
    await call("listToolEvents", client.listToolEvents({ chatId: chat.id })),
  ).toEqual([]);
  await call("deleteChat", client.deleteChat({ chatId: fork.id }));
  const parkedChat = await client.createChat({});
  const parked = await client.submitMessage(
    { chatId: parkedChat.id },
    { content: "park" },
  );
  await call("cancelRun", client.cancelRun({ runId: parked.result.runId }));
  const resumed = await call(
    "resumeRun",
    client.resumeRun({ runId: "resume-me" }),
  );
  expect(["queued", "running", "completed"]).toContain(resumed.status);
  const proposals = await call(
    "listProposals",
    client.listProposals({ chatId: "chat-client" }),
  );
  expect(proposals.every((item) => Value.Check(ProposalDtoSchema, item))).toBe(
    true,
  );
  expect(
    (
      await call(
        "approveProposal",
        client.approveProposal(
          { proposalId: "approve-me" },
          { reason: "reviewed" },
        ),
      )
    ).status,
  ).toBe("approved");
  const applied = await call(
    "applyProposal",
    client.applyProposal(
      { proposalId: "approve-me" },
      { operationId: "native-operation" },
    ),
  );
  expect(applied.status).toBe("applied");
  expect(applied.outcome?.appliedOps).toBe(1);
  expect(
    await client.applyProposal(
      { proposalId: "approve-me" },
      { operationId: "native-operation" },
    ),
  ).toEqual(applied);
  expect(
    (
      await call(
        "rejectProposal",
        client.rejectProposal(
          { proposalId: "reject-me" },
          { reason: "declined" },
        ),
      )
    ).status,
  ).toBe("rejected");
  const provider = await call(
    "createProvider",
    client.createProvider({
      id: "native-provider",
      label: "Native",
      kind: "openai-compatible",
      baseUrl: "http://127.0.0.1:9999",
      defaultModel: "m1",
      apiKey: "never-publish",
      extraHeaders: { "x-secret": "never-publish-header" },
    }),
  );
  expect(Value.Check(ProviderDtoSchema, provider)).toBe(true);
  expect(JSON.stringify(provider)).not.toContain("never-publish");
  expect(await call("listProviders", client.listProviders())).toHaveLength(1);
  expect(
    (
      await call(
        "updateProvider",
        client.updateProvider(
          { providerId: provider.id },
          { label: "Updated" },
        ),
      )
    ).label,
  ).toBe("Updated");
  const models = await call(
    "listModels",
    client.listModels({ providerId: provider.id }),
  );
  expect(models).toEqual([]);
  const refreshed = await call(
    "refreshProviderModels",
    client.refreshProviderModels({ providerId: provider.id }),
  );
  expect(refreshed).toHaveLength(1);
  expect(refreshed.every((model) => Value.Check(ModelDtoSchema, model))).toBe(
    true,
  );
  expect(await client.listModels({ providerId: provider.id })).toEqual(
    refreshed,
  );
  expect(
    (
      await call(
        "testProvider",
        client.testProvider({ providerId: provider.id }),
      )
    ).ok,
  ).toBe(true);
  await call(
    "deleteProvider",
    client.deleteProvider({ providerId: provider.id }),
  );
  const settings = await call("getSettings", client.getSettings());
  expect(Value.Check(SettingsDtoSchema, settings)).toBe(true);
  expect(settings.contextSizePreference).toBe("small");
  const floatPatch = await fetch(`${server.baseUrl}/v1/settings`, {
    method: "PATCH",
    headers: {
      authorization: `Bearer ${server.token}`,
      "content-type": "application/json",
    },
    body: '{"maxToolIterations":2.0}',
  });
  expect(floatPatch.status).toBe(200);
  expect(
    (
      await call(
        "updateSettings",
        client.updateSettings({ maxToolIterations: 4 }),
      )
    ).maxToolIterations,
  ).toBe(4);
  const allowances = await call(
    "listAllowances",
    client.listAllowances({ chatId: chat.id }),
  );
  expect(Value.Check(WriteAllowanceListResponseSchema, allowances)).toBe(true);
  expect(allowances.allowances).toEqual([]);
  const granted = await call(
    "grantAllowance",
    client.grantAllowance(
      { chatId: chat.id },
      { toolName: "edit", proposalKind: "edit", maxRisk: "low" },
    ),
  );
  await call(
    "revokeAllowance",
    client.revokeAllowance({ chatId: chat.id, allowanceId: granted.key }),
  );
  const mcp = await call(
    "createMcpServer",
    client.createMcpServer({
      alias: "native",
      transport: {
        kind: "stdio",
        command: "fixture",
        env: { TOKEN: "never-publish" },
      },
    }),
  );
  expect(mcp.transport.kind === "stdio" && mcp.transport.env?.["TOKEN"]).toBe(
    "***",
  );
  expect(await call("listMcpServers", client.listMcpServers())).toHaveLength(1);
  expect(
    (
      await call(
        "updateMcpServer",
        client.updateMcpServer({ serverId: mcp.id }, { enabled: false }),
      )
    ).enabled,
  ).toBe(false);
  await call("deleteMcpServer", client.deleteMcpServer({ serverId: mcp.id }));
  expect(await call("listTools", client.listTools())).toEqual([]);
  expect([...covered].sort()).toEqual(
    (Object.keys(REST_ROUTES) as RestOperation[]).sort(),
  );
}, 60_000);
