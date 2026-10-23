import { afterEach, expect, test } from "bun:test";
import { MemoryConnectionStore } from "@agentkit/adapters-memory";
import {
  githubConfig,
  planeConfig,
  type McpToolCallOutcome,
  type OperationReceipt,
} from "@agentkit/mcp-client";
import {
  InMemoryHarness,
  buildFakeServer,
  createSecretStore,
  toolContext,
} from "../../../packages/mcp-client/tests/helpers.js";
import {
  createManagedConnectorAcceptance,
  type AcceptanceEvidence,
  type AcceptanceOptions,
  type AcceptanceTarget,
} from "../src/managed-connector-acceptance.js";

const context = {
  runId: "acceptance-run",
  bindings: [],
  limits: toolContext().limits,
};
const cleanups: (() => Promise<void>)[] = [];
afterEach(async () => {
  for (const close of cleanups.splice(0)) await close();
});

async function setup(
  options: {
    approve?: AcceptanceOptions["approveExactAction"];
    disposable?: true;
    loseAck?: true;
    wrongIdentity?: true;
    afterIdentity?(): Promise<void>;
  } = {},
) {
  const calls: string[] = [];
  const evidence: AcceptanceEvidence[] = [];
  const receipts: OperationReceipt[] = [];
  const secrets = createSecretStore({
    "github/a": "github-a-private-token",
    "github/b": "github-b-private-token",
    "plane/a": "plane-a-private-token",
    "plane/b": "plane-b-private-token",
  });
  const store = new MemoryConnectionStore();
  const aliases = ["github-a", "github-b", "plane-a", "plane-b"];
  const harness = new InMemoryHarness(
    Object.fromEntries(
      aliases.map((alias) => [
        alias,
        () =>
          buildFakeServer(
            alias,
            [
              "get_me",
              "get_file_contents",
              "issue_write",
              "member",
              "workitem",
            ].map((name) => ({
              name,
              inputSchema: {
                type: "object",
                properties: {
                  action: { type: "string" },
                  owner: { type: "string" },
                  repo: { type: "string" },
                  project_id: { type: "string" },
                  name: { type: "string" },
                },
                additionalProperties: false,
              },
              handler: async (args: Record<string, unknown> | undefined) => {
                calls.push(`${alias}:${name}:${args?.action ?? ""}`);
                if (
                  options.loseAck &&
                  name === "workitem" &&
                  args?.action === "create"
                )
                  await harness.dropCurrent(alias);
                return {
                  content: [
                    {
                      type: "text",
                      text: "private remote content must not enter evidence",
                    },
                  ],
                  structuredContent: {
                    id:
                      name === "get_me" || name === "member"
                        ? alias
                        : "external-123",
                    workspace: `workspace-${alias.at(-1)}`,
                  },
                };
              },
            })),
          ),
      ]),
    ),
  );
  const targets: AcceptanceTarget[] = aliases.map((alias): AcceptanceTarget => {
    const plane = alias.startsWith("plane");
    const workspaceId = `workspace-${alias.at(-1)}`;
    return {
      connection: {
        id: alias,
        accountId: alias,
        connectorId: plane ? "plane" : "github",
        revision: 1,
        ...(plane ? { workspaceId } : {}),
        config: plane
          ? planeConfig(alias, workspaceId, `plane/${alias.at(-1)}`)
          : githubConfig(alias, `github/${alias.at(-1)}`),
      },
      tools: plane ? ["workitem"] : ["get_file_contents", "issue_write"],
      scope: plane
        ? { project_id: `project-${alias.at(-1)}` }
        : { owner: "disposable-owner", repo: `repo-${alias.at(-1)}` },
      identityProbe: {
        toolName: plane ? "member" : "get_me",
        arguments: plane ? { action: "me" } : {},
      },
      verifyIdentity: async (result) => {
        await options.afterIdentity?.();
        const remote = (result.data as McpToolCallOutcome).structuredContent;
        return (
          !options.wrongIdentity &&
          remote?.id === alias &&
          (!plane || remote.workspace === workspaceId)
        );
      },
      ...(options.disposable ? { disposable: true } : {}),
    };
  });
  const acceptance = await createManagedConnectorAcceptance({
    deps: {
      store,
      secrets,
      transportFactory: async (request) => {
        expect(request.transport.kind).toBe("http");
        if (request.transport.kind === "http")
          expect(request.transport.headers?.Authorization).toBe(
            `Bearer ${request.alias}-private-token`,
          );
        return harness.factory(request);
      },
    },
    targets,
    execution: "deterministic",
    evidence: async (event) => {
      evidence.push(event);
    },
    recordOperation: async (receipt) => {
      receipts.push(receipt);
    },
    approveExactAction: options.approve,
    externalIds: (result) => [String(result.structuredContent?.id)],
  });
  cleanups.push(acceptance.dispose);
  return { acceptance, calls, evidence, receipts, secrets, store, harness };
}

test("two account/workspace/project targets discover only selection and enforce exact scopes", async () => {
  const f = await setup();
  const run = await f.acceptance.select(["github-a", "plane-a"], context);
  expect(run.tools).toHaveLength(3);
  expect(f.calls).toEqual(["github-a:get_me:", "plane-a:member:me"]);
  expect(
    (
      await run.call("plane-a", "workitem", {
        action: "list",
        project_id: "project-a",
      })
    ).ok,
  ).toBe(true);
  expect(
    (
      await run.call("plane-a", "workitem", {
        action: "list",
        project_id: "project-b",
      })
    ).ok,
  ).toBe(false);
  await expect(
    run.call("plane-b", "workitem", {
      action: "list",
      project_id: "project-b",
    }),
  ).rejects.toMatchObject({ code: "mcp_invocation_refused" });
  const second = await f.acceptance.select(["github-b", "plane-b"], {
    ...context,
    runId: "second",
  });
  expect(
    (
      await second.call("plane-b", "workitem", {
        action: "list",
        project_id: "project-b",
      })
    ).ok,
  ).toBe(true);
  expect(
    (
      await second.call("github-b", "get_file_contents", {
        owner: "disposable-owner",
        repo: "repo-b",
      })
    ).ok,
  ).toBe(true);
  const serialized = JSON.stringify({
    evidence: f.evidence,
    config: await f.store.list(),
  });
  expect(serialized).not.toContain("private-token");
  expect(serialized).not.toContain("private remote content");
  expect(f.evidence.every((event) => event.execution === "deterministic")).toBe(
    true,
  );
});

test("read-only default and non-disposable target refuse writes before approval", async () => {
  let approvals = 0;
  const f = await setup({
    approve: async () => {
      approvals++;
      return true;
    },
  });
  const run = await f.acceptance.select(["plane-a"], context);
  expect(
    (
      await run.call("plane-a", "workitem", {
        action: "create",
        project_id: "project-a",
        name: "test",
      })
    ).ok,
  ).toBe(false);
  expect(approvals).toBe(0);
  expect(f.calls.filter((call) => call.endsWith(":create"))).toHaveLength(0);
});

test("disposable target still needs exact immutable approval and records pending/terminal IDs", async () => {
  const actions: unknown[] = [];
  const f = await setup({
    disposable: true,
    approve: async (action) => {
      expect(Object.isFrozen(action)).toBe(true);
      actions.push(action.arguments);
      return action.arguments.name === "approved";
    },
  });
  const run = await f.acceptance.select(["plane-a"], context);
  expect(
    (
      await run.call("plane-a", "workitem", {
        action: "create",
        project_id: "project-b",
        name: "approved",
      })
    ).ok,
  ).toBe(false);
  expect(
    (
      await run.call("plane-a", "workitem", {
        action: "create",
        project_id: "project-a",
        name: "denied",
      })
    ).ok,
  ).toBe(false);
  expect(
    (
      await run.call("plane-a", "workitem", {
        action: "create",
        project_id: "project-a",
        name: "approved",
      })
    ).ok,
  ).toBe(true);
  expect(actions).toHaveLength(2);
  expect(
    f.receipts
      .filter((receipt) => receipt.effect === "write")
      .map((receipt) => receipt.outcome),
  ).toEqual(["pending", "succeeded"]);
  expect(
    f.evidence.find(
      (event) => event.effect === "write" && event.outcome === "succeeded",
    )?.externalIds,
  ).toEqual(["external-123"]);
});

test("lost acknowledgement produces uncertain receipt without replay", async () => {
  const f = await setup({
    disposable: true,
    approve: async () => true,
    loseAck: true,
  });
  const run = await f.acceptance.select(["plane-a"], context);
  const result = await run.call("plane-a", "workitem", {
    action: "create",
    project_id: "project-a",
    name: "test",
  });
  expect(result.ok).toBe(false);
  expect(result.modelData).toMatchObject({
    errorCode: "mcp_outcome_uncertain",
    retryable: false,
  });
  expect(f.calls.filter((call) => call.endsWith(":create"))).toHaveLength(1);
  expect(f.receipts.at(-1)).toMatchObject({
    outcome: "uncertain",
    errorCode: "mcp_outcome_uncertain",
  });
});

test("remote account/workspace mismatch refuses selection before ordinary reads", async () => {
  const f = await setup({ wrongIdentity: true });
  await expect(f.acceptance.select(["plane-a"], context)).rejects.toMatchObject(
    { code: "mcp_invocation_refused" },
  );
  expect(f.calls).toEqual(["plane-a:member:me"]);
});

test("disconnect/reconnect invalidates old run; fresh discovery verifies identity again", async () => {
  const f = await setup();
  const old = await f.acceptance.select(["plane-a"], context);
  await f.acceptance.service.disconnect("plane-a");
  await f.acceptance.service.connect("plane-a");
  expect(
    (
      await old.call("plane-a", "workitem", {
        action: "list",
        project_id: "project-a",
      })
    ).ok,
  ).toBe(false);
  const fresh = await f.acceptance.select(["plane-a"], {
    ...context,
    runId: "fresh",
  });
  expect(
    (
      await fresh.call("plane-a", "workitem", {
        action: "list",
        project_id: "project-a",
      })
    ).ok,
  ).toBe(true);
  expect(f.calls.filter((call) => call === "plane-a:member:me")).toHaveLength(
    2,
  );
});

test("an awaited identity verifier cannot switch the staged run onto a new session", async () => {
  let reconnect: (() => Promise<void>) | undefined;
  const f = await setup({
    afterIdentity: async () => {
      await reconnect?.();
    },
  });
  reconnect = async () => {
    await f.acceptance.service.disconnect("plane-a");
    await f.acceptance.service.connect("plane-a");
  };
  const run = await f.acceptance.select(["plane-a"], context);
  expect(
    (
      await run.call("plane-a", "workitem", {
        action: "list",
        project_id: "project-a",
      })
    ).ok,
  ).toBe(false);
  expect(f.calls).toEqual(["plane-a:member:me"]);
});
