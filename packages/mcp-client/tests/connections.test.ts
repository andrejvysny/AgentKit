import { expect, test } from "bun:test";
import {
  ConnectionService,
  customMcpConnector,
  planeConnector,
  type ConnectionGrant,
  type OperationReceipt,
  type ConnectorInvocation,
  type ConnectionServiceDeps,
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
  overrides: Partial<ConnectionServiceDeps> = {},
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
    ...overrides,
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
          env: {
            TOKEN: "literal-credential",
            PLANE_WORKSPACE_SLUG: "workspace",
          },
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
    expect(Object.isFrozen(invocation)).toBe(true);
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

test("staged tools cannot revive after disconnect and reconnect", async () => {
  const f = await setup(async () => true);
  try {
    const [tool] = await f.contributor.contribute(context);
    await f.service.disconnect(f.record.id);
    await f.service.connect(f.record.id);
    expect((await tool!.execute(toolContext(), { action: "create" })).ok).toBe(
      false,
    );
    expect(f.writes()).toBe(0);
    const [fresh] = await f.contributor.contribute(context);
    expect((await fresh!.execute(toolContext(), { action: "create" })).ok).toBe(
      true,
    );
  } finally {
    await f.service.dispose();
  }
});

test("duplicate aliases cannot route a second account through the first OAuth provider", async () => {
  const f = await setup();
  try {
    await expect(
      f.service.save(
        {
          ...f.record,
          id: "other",
          accountId: "other-account",
          revision: 1,
        },
        null,
      ),
    ).rejects.toMatchObject({ code: "mcp_invalid_config" });
    expect(await f.service.get("other")).toBeNull();
  } finally {
    await f.service.dispose();
  }
});

test("pending receipts receive a terminal refusal when disconnect occurs during persistence", async () => {
  const receipts: OperationReceipt[] = [];
  let disconnect: (() => Promise<void>) | undefined;
  const f = await setup(async () => true, {
    recordOperation: async (receipt) => {
      receipts.push(receipt);
      if (receipt.outcome === "pending") await disconnect?.();
    },
  });
  disconnect = () => f.service.disconnect(f.record.id);
  try {
    const [tool] = await f.contributor.contribute(context);
    expect((await tool!.execute(toolContext(), { action: "create" })).ok).toBe(
      false,
    );
    expect(f.writes()).toBe(0);
    expect(receipts.map((r) => r.outcome)).toEqual(["pending", "failed"]);
    expect(receipts[1]?.errorCode).toBe("mcp_stale_connection");
  } finally {
    await f.service.dispose();
  }
});

test("managed contributions forward selected-model media capabilities and retain unsupported warnings", async () => {
  const harness = new InMemoryHarness({
    test: () =>
      buildFakeServer("test", [
        {
          name: "workitem",
          inputSchema: {
            type: "object",
            properties: { action: { type: "string" } },
          },
          handler: async () => ({
            content: [
              { type: "image", data: "aGVsbG8=", mimeType: "image/png" },
              { type: "audio", data: "aGVsbG8=", mimeType: "audio/wav" },
              { type: "audio", data: "aGVsbG8=", mimeType: "audio/ogg" },
            ],
          }),
        },
      ]),
  });
  const seenRuns: (string | undefined)[] = [];
  const f = await setup(undefined, {
    transportFactory: harness.factory,
    modelContentCapabilities: async (ctx) => {
      seenRuns.push(ctx.runId);
      return ctx.runId === "media" ? { image: true, audio: true } : {};
    },
  });
  try {
    const [media] = await f.contributor.contribute({
      ...context,
      runId: "media",
    });
    const result = await media!.execute(toolContext(), { action: "list" });
    expect(result.modelContent?.map((part) => part.type)).toEqual([
      "image",
      "audio",
    ]);
    expect(result.warnings?.join(" ")).toContain("audio");
    expect((result.data as { content: unknown[] }).content).toHaveLength(3);
    const [text] = await f.contributor.contribute(context);
    const textResult = await text!.execute(toolContext(), { action: "list" });
    expect(textResult.modelContent).toBeUndefined();
    expect(textResult.warnings?.join(" ")).toContain("image, audio");
    expect(seenRuns).toEqual(["media", "run"]);
  } finally {
    await f.service.dispose();
  }
});

test("remote context cannot follow a disconnected/reconnected session", async () => {
  const f = await setup();
  try {
    const remote = f.service.context({
      ...f.grant,
      resources: ["test://resource"],
    });
    await f.service.disconnect(f.record.id);
    await f.service.connect(f.record.id);
    await expect(remote.listResources()).rejects.toMatchObject({
      code: "mcp_stale_connection",
    });
  } finally {
    await f.service.dispose();
  }
});

test("disable and failed remote revocation refuse staged calls and clear owned credentials", async () => {
  let revocations = 0;
  const f = await setup(async () => true, {
    revoke: async () => {
      revocations++;
      throw new Error("remote revocation unavailable");
    },
  });
  try {
    const [tool] = await f.contributor.contribute(context);
    await f.service.save(
      {
        ...f.record,
        revision: 2,
        config: { ...f.record.config, enabled: false },
      },
      1,
    );
    expect((await tool!.execute(toolContext(), { action: "create" })).ok).toBe(
      false,
    );
    await expect(f.service.connect(f.record.id)).rejects.toMatchObject({
      code: "mcp_stale_connection",
    });
    await expect(f.service.remove(f.record.id)).rejects.toThrow(
      "remote revocation unavailable",
    );
    expect(revocations).toBe(1);
    expect(await f.service.get(f.record.id)).toBeNull();
    expect(await f.secrets.listRefs()).toHaveLength(0);
    expect((await tool!.execute(toolContext(), { action: "create" })).ok).toBe(
      false,
    );
    expect(f.writes()).toBe(0);
  } finally {
    await f.service.dispose();
  }
});

test("out-of-band conflicting aliases are rejected before opening another account session", async () => {
  const f = await setup();
  try {
    await f.store.put(
      { ...f.record, id: "other", accountId: "other-account" },
      null,
    );
    await expect(f.service.connect("other")).rejects.toMatchObject({
      code: "mcp_invalid_config",
    });
  } finally {
    await f.service.dispose();
  }
});
