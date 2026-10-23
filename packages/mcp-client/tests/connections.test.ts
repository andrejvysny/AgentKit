import { expect, test } from "bun:test";
import {
  ConnectionService,
  customMcpConnector,
  planeConnector,
  type ConnectionGrant,
  type OperationReceipt,
  type ConnectorInvocation,
} from "../src/index.js";
import { MemoryConnectionStore } from "../../adapters-memory/src/memory-connection-store.js";
import {
  InMemoryHarness,
  buildFakeServer,
  createSecretStore,
  toolContext,
} from "./helpers.js";
const context = { runId: "run", bindings: [], limits: toolContext().limits };
async function setup(
  authorize?: (args: ConnectorInvocation) => Promise<boolean>,
) {
  let writes = 0;
  const receipts: OperationReceipt[] = [];
  const harness = new InMemoryHarness({
    test: () =>
      buildFakeServer("test", [
        {
          name: "workitem",
          annotations: { readOnlyHint: true },
          inputSchema: {
            type: "object",
            properties: {
              action: { type: "string" },
              title: { type: "string" },
            },
            required: ["action"],
            additionalProperties: false,
          },
          handler: async (args) => {
            if (args?.action === "create") writes++;
            return {
              content: [{ type: "text", text: "ok" }],
              structuredContent: { id: "external-1" },
            };
          },
        },
      ]),
  });
  const store = new MemoryConnectionStore();
  const secrets = createSecretStore();
  const service = new ConnectionService({
    store,
    secrets,
    connectors: [customMcpConnector, planeConnector],
    transportFactory: harness.factory,
    authorize,
    recordOperation: async (receipt) => {
      receipts.push(receipt);
    },
  });
  const record = await service.save(
    {
      id: "connection",
      connectorId: "plane",
      accountId: "account",
      workspaceId: "workspace",
      revision: 1,
      config: {
        alias: "test",
        transport: {
          kind: "stdio",
          command: "test",
          env: { TOKEN: "literal-credential" },
        },
      },
    },
    null,
  );
  const grant: ConnectionGrant = {
    connectionId: record.id,
    accountId: record.accountId,
    workspaceId: record.workspaceId,
    revision: record.revision,
    tools: ["workitem"],
    effects: ["read", "write"],
  };
  const contributor = service.contributor(async () => [grant]);
  return {
    service,
    store,
    secrets,
    record,
    grant,
    contributor,
    receipts,
    writes: () => writes,
  };
}
test("Plane argument-dependent writes require exact immutable approval and retain external result", async () => {
  const seen: unknown[] = [];
  const f = await setup(async (invocation) => {
    seen.push(invocation);
    expect(Object.isFrozen(invocation.arguments)).toBe(true);
    return true;
  });
  try {
    const tools = await f.contributor.contribute(context);
    const input = { action: "create", title: "issue" };
    expect((await tools[0]!.execute(toolContext(), input)).ok).toBe(true);
    expect(seen).toHaveLength(1);
    expect(seen[0]).toMatchObject({
      arguments: input,
      connection: { accountId: "account", revision: 1 },
      effect: "write",
    });
    expect(f.writes()).toBe(1);
    expect(
      f.receipts.find((receipt) => receipt.outcome === "succeeded"),
    ).toMatchObject({
      outcome: "succeeded",
      externalResult: { structuredContent: { id: "external-1" } },
    });
    expect(JSON.stringify(await f.store.list())).not.toContain(
      "literal-credential",
    );
  } finally {
    await f.service.dispose();
  }
});
test("reads follow grants and writes are refused without approval despite readOnlyHint", async () => {
  const f = await setup();
  try {
    const [tool] = await f.contributor.contribute(context);
    expect((await tool!.execute(toolContext(), { action: "list" })).ok).toBe(
      true,
    );
    expect((await tool!.execute(toolContext(), { action: "create" })).ok).toBe(
      false,
    );
    expect(f.writes()).toBe(0);
  } finally {
    await f.service.dispose();
  }
});
test("disconnect and configuration revision changes invalidate a previously staged tool", async () => {
  const f = await setup(async () => true);
  try {
    const [tool] = await f.contributor.contribute(context);
    await f.service.disconnect(f.record.id);
    expect((await tool!.execute(toolContext(), { action: "create" })).ok).toBe(
      false,
    );
    await f.service.save({ ...f.record, revision: 2 }, 1);
    expect((await tool!.execute(toolContext(), { action: "create" })).ok).toBe(
      false,
    );
    expect(f.writes()).toBe(0);
  } finally {
    await f.service.dispose();
  }
});
test("scope constraints and invalid arguments are refused before authorization", async () => {
  let approvals = 0;
  const f = await setup(async () => {
    approvals++;
    return true;
  });
  try {
    const [tool] = await f.service
      .contributor(async () => [
        { ...f.grant, arguments: { title: "allowed" } },
      ])
      .contribute(context);
    expect(
      (await tool!.execute(toolContext(), { action: "create", title: "other" }))
        .ok,
    ).toBe(false);
    expect(
      (
        await tool!.execute(toolContext(), {
          action: "create",
          unexpected: true,
        })
      ).ok,
    ).toBe(false);
    expect(approvals).toBe(0);
    expect(f.writes()).toBe(0);
  } finally {
    await f.service.dispose();
  }
});
test("removal invalidates calls even after an authorizer has been waiting", async () => {
  let release!: () => void;
  let entered!: () => void;
  const ready = new Promise<void>((resolve) => {
    entered = resolve;
  });
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const f = await setup(async () => {
    entered();
    await gate;
    return true;
  });
  const [tool] = await f.contributor.contribute(context);
  const call = tool!.execute(toolContext(), { action: "create" });
  await ready;
  await f.service.remove(f.record.id);
  release();
  expect((await call).ok).toBe(false);
  expect(f.writes()).toBe(0);
  await f.service.dispose();
});
