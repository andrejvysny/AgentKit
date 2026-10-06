import { afterEach, expect, it } from "bun:test";
import type { AiTool } from "@agentkit/core";
import {
  createProposalBuilderTool,
  ProposalService,
  SessionWritePolicy,
  writePayloadFingerprint,
} from "@agentkit/host";
import { createHarness } from "../../host/tests/fakes.js";
import {
  createMcpServerHandler,
  createStagedToolSource,
  type McpServerHandler,
  type McpSessionScope,
} from "../src/index.js";
import {
  authHeaders,
  connectClient,
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

function fixture(outcome?: {
  status: "partial";
  appliedOps: number;
  failedOps: { opIndex: number; error: string }[];
}) {
  const harness = createHarness(outcome === undefined ? {} : { outcome });
  const policy = new SessionWritePolicy({ mode: "auto_all" });
  let revision = "rev-1";
  const seen: McpSessionScope[] = [];
  const service = new ProposalService({ ...harness, policy });
  const tool: AiTool = createProposalBuilderTool({
    definition: {
      name: "demo_remove",
      effect: "write",
      capability: "demo.remove",
      version: "1.0.0",
      description: "Stage item removal.",
      inputSchema: {
        type: "object",
        properties: {
          action_id: { type: "string" },
          item: { type: "string" },
        },
        required: ["action_id", "item"],
        additionalProperties: false,
      },
    },
    service,
    store: harness.store,
    policy,
    ids: harness.ids,
    scopeKeyOf: () => "design-1",
    currentRevision: async () => revision,
    build: async (_ctx, input: unknown) => ({
      kind: "items.remove",
      risk: "destructive",
      operations: [input],
      warnings: [],
      truncated: false,
    }),
  });
  const tools = createStagedToolSource({
    contributors: [{ namespace: "demo", contribute: async () => [tool] }],
    guards: [
      {
        canExecute: (ctx) => {
          seen.push({
            actorId: ctx.actorId,
            principal: ctx.principal,
            chatId: ctx.chatId,
          });
          return { allowed: true };
        },
      },
    ],
    writePolicy: policy,
    clock: harness.clock,
    ids: harness.ids,
  });
  const handler = createMcpServerHandler({
    tools,
    auth: { bearerToken: TEST_TOKEN },
    writesEnabled: true,
    sessionScope: () => ({
      chatId: "chat-1",
      principal: "same-user",
      actorId: "client-forged",
    }),
  });
  const served = serveHandler(handler);
  opened.push({ handler, served });
  return {
    ...harness,
    policy,
    service,
    handler,
    served,
    seen,
    setRevision: (value: string) => {
      revision = value;
    },
  };
}

it("same-name clients have distinct actors, grants and action keys; disconnect revokes only own grant", async () => {
  const f = fixture();
  const a = await connectClient(f.served.url, authHeaders());
  const b = await connectClient(f.served.url, authHeaders());
  const input = { action_id: "remove_item_design-1", item: "item-1" };
  const grant = {
    chatId: "chat-1",
    toolName: "demo_remove",
    proposalKind: "items.remove",
    scopeKey: "design-1",
    payloadFingerprint: await writePayloadFingerprint(input),
    revision: "rev-1",
    maxRisk: "destructive",
  } as const;
  f.policy.allow({ ...grant, actorId: a.transport.sessionId! });
  const authorized = await a.client.callTool({
    name: "demo_remove",
    arguments: input,
  });
  expect(authorized.isError).toBeUndefined();
  const repeat = await a.client.callTool({
    name: "demo_remove",
    arguments: input,
  });
  expect(JSON.stringify(repeat)).toContain("already_applied");
  await b.client.callTool({ name: "demo_remove", arguments: input });
  const records = [...f.store.proposals.proposals.values()];
  expect(records).toHaveLength(2);
  expect(records[0]!.actionId).not.toBe(records[1]!.actionId);
  expect(records[1]!.status).toBe("pending");
  expect(f.applier.calls).toHaveLength(1);
  expect(f.seen.map((scope) => scope.actorId)).toEqual([
    a.transport.sessionId,
    a.transport.sessionId,
    b.transport.sessionId,
  ]);
  expect(
    f.seen.every(
      (scope) =>
        scope.actorId !== "client-forged" && scope.principal === "same-user",
    ),
  ).toBe(true);
  f.policy.allow({ ...grant, actorId: b.transport.sessionId! });
  await a.transport.terminateSession();
  expect(f.policy.list("chat-1").map((allowance) => allowance.actorId)).toEqual(
    [b.transport.sessionId],
  );
  await a.client.close();
  await b.client.close();
});

it("changed arguments or revision cannot reuse an actor approval", async () => {
  const f = fixture();
  const { client, transport } = await connectClient(
    f.served.url,
    authHeaders(),
  );
  const input = { action_id: "remove_item_design-1", item: "item-1" };
  f.policy.allow({
    chatId: "chat-1",
    toolName: "demo_remove",
    proposalKind: "items.remove",
    scopeKey: "design-1",
    actorId: transport.sessionId!,
    payloadFingerprint: await writePayloadFingerprint(input),
    revision: "rev-1",
    maxRisk: "destructive",
  });
  await client.callTool({ name: "demo_remove", arguments: input });
  await client.callTool({
    name: "demo_remove",
    arguments: { ...input, item: "item-2" },
  });
  f.setRevision("rev-2");
  await client.callTool({ name: "demo_remove", arguments: input });
  expect(
    [...f.store.proposals.proposals.values()].map(
      (proposal) => proposal.status,
    ),
  ).toEqual(["applied", "pending", "pending"]);
  expect(f.applier.calls).toHaveLength(1);
  await client.close();
});

it("concurrent repeated writes apply and record once", async () => {
  const f = fixture();
  const { client, transport } = await connectClient(
    f.served.url,
    authHeaders(),
  );
  const input = { action_id: "remove_item_design-1", item: "item-1" };
  f.policy.allow({
    chatId: "chat-1",
    toolName: "demo_remove",
    proposalKind: "items.remove",
    scopeKey: "design-1",
    actorId: transport.sessionId!,
    payloadFingerprint: await writePayloadFingerprint(input),
    revision: "rev-1",
    maxRisk: "destructive",
  });
  await Promise.all(
    Array.from({ length: 4 }, () =>
      client.callTool({ name: "demo_remove", arguments: input }),
    ),
  );
  expect(f.store.proposals.proposals.size).toBe(1);
  expect(f.applier.calls).toHaveLength(1);
  await client.close();
});

it("stages, then honors host approval; partial mutation reports failure and is never repeated", async () => {
  const f = fixture({
    status: "partial",
    appliedOps: 1,
    failedOps: [{ opIndex: 1, error: "missing target" }],
  });
  const { client } = await connectClient(f.served.url, authHeaders());
  const input = { action_id: "remove_item_design-1", item: "item-1" };
  const staged = await client.callTool({
    name: "demo_remove",
    arguments: input,
  });
  expect(JSON.stringify(staged)).toContain("pending");
  expect(f.applier.calls).toHaveLength(0);
  const proposal = [...f.store.proposals.proposals.values()][0]!;
  await f.service.approve({
    proposalId: proposal.id,
    actor: "user",
    decidedBy: "host-user",
  });
  const outcome = await f.service.apply({
    proposalId: proposal.id,
    operationId: f.ids.operationId(),
  });
  expect(outcome.status).toBe("partial");
  const repeat = await client.callTool({
    name: "demo_remove",
    arguments: input,
  });
  expect(JSON.stringify(repeat)).toContain("already_applied");
  expect(repeat.isError).toBe(true);
  expect(JSON.stringify(repeat)).toContain("missing target");
  expect(f.applier.calls).toHaveLength(1);
  await client.close();
});

it("projects partial auto-apply as isError with exact skipped operations", async () => {
  const f = fixture({
    status: "partial",
    appliedOps: 1,
    failedOps: [{ opIndex: 1, error: "missing target" }],
  });
  const { client, transport } = await connectClient(
    f.served.url,
    authHeaders(),
  );
  const input = { action_id: "remove_item_design-1", item: "item-1" };
  f.policy.allow({
    chatId: "chat-1",
    toolName: "demo_remove",
    proposalKind: "items.remove",
    scopeKey: "design-1",
    actorId: transport.sessionId!,
    payloadFingerprint: await writePayloadFingerprint(input),
    revision: "rev-1",
    maxRisk: "destructive",
  });
  const result = await client.callTool({
    name: "demo_remove",
    arguments: input,
  });
  expect(result.isError).toBe(true);
  expect(JSON.stringify(result)).toContain("missing target");
  expect(f.applier.calls).toHaveLength(1);
  await client.close();
});
