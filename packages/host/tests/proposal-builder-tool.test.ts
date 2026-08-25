import { describe, expect, it } from "bun:test";
import type { AiToolDefinition } from "@agentkit/contracts";
import type { AiToolExecutionContext } from "@agentkit/core";
import { resolveToolLimits } from "@agentkit/core";
import {
  ProposalService,
  SessionWritePolicy,
  createProposalBuilderTool,
  type ProposalBuildResult,
  type ProposalToolData,
  type WriteToolModelData,
} from "../src/index.js";
import {
  createHarness,
  type FakeApplierOptions,
  type TestHarness,
} from "./fakes.js";

const DEFINITION: AiToolDefinition = {
  name: "write_items",
  version: "1.0.0",
  effect: "write",
  capability: "items.write",
  description: "Stage a write to the item list.",
  inputSchema: {
    type: "object",
    properties: { action_id: { type: "string" } },
  },
};

interface ToolInput {
  action_id?: unknown;
}

const CTX: AiToolExecutionContext = {
  runId: "run-1",
  chatId: "chat-1",
  bindings: [],
  limits: resolveToolLimits({ preference: "small" }),
};

interface Fixture extends TestHarness {
  service: ProposalService;
  policy: SessionWritePolicy;
  tool: ReturnType<typeof createProposalBuilderTool<ToolInput>>;
  builds: number;
}

function setup(
  options: {
    build?: Partial<ProposalBuildResult>;
    applier?: FakeApplierOptions;
    policyMode?:
      | "auto_readonly_confirm_writes"
      | "confirm_all_writes"
      | "auto_all";
    allow?: boolean;
    currentRevision?: (scopeKey: string) => Promise<string | null>;
  } = {},
): Fixture {
  const harness = createHarness(options.applier ?? {});
  const policy = new SessionWritePolicy({
    ...(options.policyMode === undefined ? {} : { mode: options.policyMode }),
    clock: harness.clock,
  });
  if (options.allow) {
    policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      maxRisk: "destructive",
    });
  }
  const service = new ProposalService({
    store: harness.store,
    applier: harness.applier,
    policy,
    clock: harness.clock,
    ids: harness.ids,
  });
  const fixture: Fixture = {
    ...harness,
    service,
    policy,
    builds: 0,
    tool: undefined as unknown as Fixture["tool"],
  };
  fixture.tool = createProposalBuilderTool<ToolInput>({
    definition: DEFINITION,
    service,
    store: harness.store,
    policy,
    ids: harness.ids,
    scopeKeyOf: () => "scope-1",
    ...(options.currentRevision === undefined
      ? {}
      : { currentRevision: options.currentRevision }),
    build: async () => {
      fixture.builds += 1;
      return {
        kind: "items.write",
        risk: "low",
        operations: [{ op: "add", value: 1 }],
        warnings: [],
        truncated: false,
        envelope: { title: "Add one item" },
        ...options.build,
      };
    },
  });
  return fixture;
}

function modelData(result: { modelData?: unknown }): WriteToolModelData {
  return result.modelData as WriteToolModelData;
}

describe("createProposalBuilderTool — wiring", () => {
  it("refuses to wrap a read tool", () => {
    const f = setup();
    expect(() =>
      createProposalBuilderTool<ToolInput>({
        definition: { ...DEFINITION, effect: "read" },
        service: f.service,
        store: f.store,
        policy: f.policy,
        ids: f.ids,
        scopeKeyOf: () => "scope-1",
        build: async () => ({
          kind: "k",
          risk: "low",
          operations: [],
          warnings: [],
          truncated: false,
        }),
      }),
    ).toThrow(/effect:"write"/);
  });
});

describe("createProposalBuilderTool — staging", () => {
  it("always stages a pending proposal when the policy will not auto-apply", async () => {
    const f = setup();
    const result = await f.tool.execute(CTX, {
      action_id: "create_a_scope-1",
    });

    expect(result.ok).toBe(true);
    expect(modelData(result).status).toBe("pending");
    expect(modelData(result).appliedCount).toBe(0);
    const staged = [...f.store.proposals.proposals.values()];
    expect(staged).toHaveLength(1);
    expect(staged[0]!.status).toBe("pending");
    expect(staged[0]!.actionId).toBe("create_a_scope-1");
    expect(staged[0]!.runId).toBe("run-1");
    expect(f.applier.calls).toHaveLength(0);
    expect((result.data as ProposalToolData).operationCount).toBe(1);
  });

  it("stamps the scope revision so the apply path can detect staleness", async () => {
    const f = setup({ currentRevision: async () => "rev-3" });
    await f.tool.execute(CTX, { action_id: "create_a_scope-1" });
    expect([...f.store.proposals.proposals.values()][0]!.revisionAtCreate).toBe(
      "rev-3",
    );
  });

  it("degrades a malformed action_id to a warning and stages anyway", async () => {
    const f = setup();
    const result = await f.tool.execute(CTX, { action_id: "not-an-action-id" });

    expect(result.ok).toBe(true);
    expect(result.warnings.some((w) => w.includes("not-an-action-id"))).toBe(
      true,
    );
    const staged = [...f.store.proposals.proposals.values()];
    expect(staged).toHaveLength(1);
    // No key ⇒ no dedup protection; the write still happens.
    expect(staged[0]!.actionId).toBeUndefined();
  });

  it("reports a missing chat context instead of throwing", async () => {
    const f = setup();
    const result = await f.tool.execute(
      { ...CTX, chatId: undefined },
      { action_id: "create_a_scope-1" },
    );
    expect(result.ok).toBe(false);
    expect(result.data).toBeNull();
    expect(f.builds).toBe(0);
  });
});

describe("createProposalBuilderTool — dedup by action_id", () => {
  async function stagePrior(
    f: Fixture,
    status:
      | "applied"
      | "pending"
      | "approved"
      | "applying"
      | "failed"
      | "rejected"
      | "invalidated",
  ): Promise<void> {
    const proposal = await f.service.stage({
      chatId: "chat-1",
      scopeKey: "scope-1",
      actionId: "create_a_scope-1",
      toolName: "write_items",
      kind: "items.write",
      risk: "low",
      operations: [{ op: "add" }],
    });
    if (status === "pending") return;
    if (status === "rejected") {
      await f.store.proposals.transition(proposal.id, ["pending"], "rejected");
      return;
    }
    if (status === "invalidated") {
      await f.store.proposals.transition(
        proposal.id,
        ["pending"],
        "invalidated",
      );
      return;
    }
    await f.store.proposals.transition(proposal.id, ["pending"], "approved");
    if (status === "approved") return;
    await f.store.proposals.transition(proposal.id, ["approved"], "applying", {
      operationId: "op-prior",
    });
    if (status === "applying") return;
    if (status === "applied") {
      await f.store.proposals.recordOutcome("op-prior", {
        status: "applied",
        appliedOps: 4,
        failedOps: [],
      });
      await f.store.proposals.transition(proposal.id, ["applying"], "applied");
      return;
    }
    await f.store.proposals.transition(proposal.id, ["applying"], "failed");
  }

  it("answers an applied action_id from the record, without rebuilding", async () => {
    const f = setup();
    await stagePrior(f, "applied");
    const result = await f.tool.execute(CTX, {
      action_id: "create_a_scope-1",
    });

    expect(result.ok).toBe(true);
    expect(result.status).toBe("ok");
    expect(modelData(result)).toEqual({
      status: "already_applied",
      appliedCount: 4,
      skipped: [],
    });
    expect(f.builds).toBe(0);
    expect(f.store.proposals.proposals.size).toBe(1);
  });

  for (const status of ["pending", "approved", "applying"] as const) {
    it(`blocks an in-flight (${status}) action_id`, async () => {
      const f = setup();
      await stagePrior(f, status);
      const result = await f.tool.execute(CTX, {
        action_id: "create_a_scope-1",
      });

      expect(result.ok).toBe(false);
      expect(result.status).toBe("partial");
      expect(result.summary).toContain("duplicate_blocked");
      expect(modelData(result).skipped[0]!.reason).toContain("in-flight");
      expect(modelData(result).skipped[0]!.reason).toContain("do not re-issue");
      expect(f.builds).toBe(0);
      expect(f.store.proposals.proposals.size).toBe(1);
    });
  }

  it("blocks a failed action_id and asks for a new key", async () => {
    const f = setup();
    await stagePrior(f, "failed");
    const result = await f.tool.execute(CTX, {
      action_id: "create_a_scope-1",
    });

    expect(result.ok).toBe(false);
    expect(result.status).toBe("partial");
    expect(modelData(result).skipped[0]!.reason).toContain("new action_id");
    expect(f.builds).toBe(0);
  });

  for (const status of ["rejected", "invalidated"] as const) {
    it(`allows a fresh attempt after ${status}`, async () => {
      const f = setup();
      await stagePrior(f, status);
      const result = await f.tool.execute(CTX, {
        action_id: "create_a_scope-1",
      });

      expect(result.ok).toBe(true);
      expect(modelData(result).status).toBe("pending");
      expect(f.builds).toBe(1);
      // A second record for the same key: the prior one is terminal, and the
      // store's uniqueness guard covers only live ids in this fake — the real
      // adapter scopes uniqueness the same way the service asks it to.
      expect(f.store.proposals.proposals.size).toBe(2);
    });
  }
});

describe("createProposalBuilderTool — auto-apply gate", () => {
  it("applies when the policy allows it, recording a POLICY decision", async () => {
    const f = setup({ allow: true });
    const result = await f.tool.execute(CTX, {
      action_id: "create_a_scope-1",
    });

    expect(result.ok).toBe(true);
    expect(result.status).toBe("ok");
    expect(modelData(result)).toEqual({
      status: "ok",
      appliedCount: 1,
      skipped: [],
    });
    const proposal = [...f.store.proposals.proposals.values()][0]!;
    expect(proposal.status).toBe("applied");
    expect(proposal.decision?.actor).toBe("policy");
    expect(proposal.decision?.policyId).toBe("session-write-policy");
    expect(f.applier.calls).toHaveLength(1);
  });

  it("waits when there are no operations to apply", async () => {
    const f = setup({ allow: true, build: { operations: [] } });
    const result = await f.tool.execute(CTX, { action_id: "create_a_scope-1" });
    expect(modelData(result).status).toBe("pending");
    expect(f.applier.calls).toHaveLength(0);
  });

  it("waits when the build was truncated", async () => {
    const f = setup({ allow: true, build: { truncated: true } });
    const result = await f.tool.execute(CTX, { action_id: "create_a_scope-1" });
    expect(modelData(result).status).toBe("pending");
    expect(result.truncated).toBe(true);
    expect(f.applier.calls).toHaveLength(0);
  });

  it("waits when a destructive write carries warnings", async () => {
    const f = setup({
      allow: true,
      build: { risk: "destructive", warnings: ["2 targets unresolved"] },
    });
    const result = await f.tool.execute(CTX, { action_id: "create_a_scope-1" });
    expect(modelData(result).status).toBe("partial");
    expect(modelData(result).skipped).toEqual([
      { id: "build", reason: "2 targets unresolved" },
    ]);
    expect(f.applier.calls).toHaveLength(0);
  });

  it("applies a destructive write with no warnings when allowed", async () => {
    const f = setup({ allow: true, build: { risk: "destructive" } });
    const result = await f.tool.execute(CTX, { action_id: "create_a_scope-1" });
    expect(modelData(result).status).toBe("ok");
    expect(f.applier.calls).toHaveLength(1);
  });

  it("waits when the policy denies", async () => {
    const f = setup({ policyMode: "confirm_all_writes", allow: true });
    const result = await f.tool.execute(CTX, { action_id: "create_a_scope-1" });
    expect(modelData(result).status).toBe("pending");
    expect(f.applier.calls).toHaveLength(0);
  });
});

describe("createProposalBuilderTool — apply results", () => {
  it("reports a partial apply as ok:false with the failed ops listed", async () => {
    const f = setup({
      allow: true,
      applier: {
        outcome: {
          status: "partial",
          appliedOps: 2,
          failedOps: [{ opIndex: 1, error: "target missing" }],
        },
      },
    });
    const result = await f.tool.execute(CTX, { action_id: "create_a_scope-1" });

    expect(result.ok).toBe(false);
    expect(result.status).toBe("partial");
    expect(modelData(result)).toEqual({
      status: "partial",
      appliedCount: 2,
      skipped: [{ id: "op:1", reason: "target missing" }],
    });
    expect([...f.store.proposals.proposals.values()][0]!.status).toBe(
      "applied",
    );
  });

  it("turns an apply failure into a result, never a throw", async () => {
    const f = setup({
      allow: true,
      applier: { throws: new Error("host refused the write") },
    });
    const result = await f.tool.execute(CTX, { action_id: "create_a_scope-1" });

    expect(result.ok).toBe(false);
    expect(result.status).toBe("partial");
    expect(result.summary).toContain("host refused the write");
    expect(modelData(result).skipped).toContainEqual({
      id: "apply",
      reason: "host refused the write",
    });
    const proposal = [...f.store.proposals.proposals.values()][0]!;
    expect(proposal.status).toBe("failed");
  });

  it("leaves the proposal failed when the scope moved since staging", async () => {
    // What the builder believes the revision is when it stages…
    const staged = { revision: "rev-1" };
    const f = setup({
      allow: true,
      currentRevision: async () => staged.revision,
    });
    // …versus where the world actually is when the apply is attempted.
    f.applier.revisions.set("scope-1", "rev-2");

    const result = await f.tool.execute(CTX, { action_id: "create_a_scope-1" });
    expect(result.ok).toBe(false);
    expect(result.summary).toContain("changed since");
    // The write was refused BEFORE the applier was ever called.
    expect(f.applier.calls).toHaveLength(0);
    const proposal = [...f.store.proposals.proposals.values()][0]!;
    expect(proposal.status).toBe("failed");

    // …and a fresh attempt, built against the current revision, proceeds.
    staged.revision = "rev-2";
    const retry = await f.tool.execute(CTX, { action_id: "create_b_scope-1" });
    expect(retry.ok).toBe(true);
    expect(modelData(retry).status).toBe("ok");
  });
});
