import { describe, expect, it } from "bun:test";
import { resolveToolLimits, type AiToolExecutionContext } from "@agentkit/core";
import {
  createProposalBuilderTool,
  ProposalService,
  SessionWritePolicy,
  writePayloadFingerprint,
  type WriteToolModelData,
  type WritePolicyMode,
} from "../src/index.js";
import { createHarness } from "./fakes.js";

interface ToolInput {
  action_id?: unknown;
  value?: number;
}
const CTX: AiToolExecutionContext = {
  runId: "run-1",
  chatId: "chat-1",
  bindings: [],
  limits: resolveToolLimits({ preference: "small" }),
};
function setup(
  options: {
    allow?: boolean;
    policyMode?: WritePolicyMode;
    currentRevision?: (scopeKey: string) => Promise<string | null>;
    applierOptions?: Parameters<typeof createHarness>[0];
  } = {},
) {
  const harness = createHarness(options.applierOptions ?? {});
  const policy = new SessionWritePolicy({ mode: options.policyMode });
  if (options.allow)
    policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      maxRisk: "destructive",
    });
  const service = new ProposalService({ ...harness, policy });
  const tool = createProposalBuilderTool<ToolInput>({
    definition: {
      name: "write_items",
      version: "1.0.0",
      effect: "write",
      capability: "items.write",
      description: "Stage an item write.",
      inputSchema: { type: "object" },
    },
    service,
    store: harness.store,
    policy,
    ids: harness.ids,
    scopeKeyOf: () => "scope-1",
    currentRevision: options.currentRevision,
    build: async () => ({
      kind: "items.write",
      risk: "low",
      operations: [{ op: "add", value: 1 }],
      warnings: [],
      truncated: false,
    }),
  });
  return { ...harness, service, policy, tool };
}
function modelData(result: { modelData?: unknown }): WriteToolModelData {
  return result.modelData as WriteToolModelData;
}

describe("createProposalBuilderTool — inbound invocation identity", () => {
  const input = { action_id: "create_a_scope-1" };
  const actorContext = (actorId: string): AiToolExecutionContext => ({
    ...CTX,
    metadata: { source: "mcp-server", actorId, principal: "same-user" },
  });

  it("isolates action keys and records trusted actor audit metadata", async () => {
    const f = setup({ allow: true });
    await f.tool.execute(actorContext("session-a"), input);
    await f.tool.execute(actorContext("session-b"), input);
    const proposals = [...f.store.proposals.proposals.values()];
    expect(proposals).toHaveLength(2);
    expect(proposals[0]!.scopeKey).toBe("scope-1");
    expect(proposals[0]!.actionId).not.toBe(proposals[1]!.actionId);
    expect(proposals[0]!.envelope["__agentkitInvocation"]).toMatchObject({
      actorId: "session-a",
      principal: "same-user",
    });
    expect(f.applier.calls).toHaveLength(0);
  });

  it("deduplicates an authorized actor write without recording or applying it twice", async () => {
    const f = setup();
    f.policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      actorId: "session-a",
      scopeKey: "scope-1",
      payloadFingerprint: await writePayloadFingerprint(input),
      revision: null,
      maxRisk: "destructive",
    });
    expect(
      modelData(await f.tool.execute(actorContext("session-a"), input)).status,
    ).toBe("ok");
    expect(
      modelData(await f.tool.execute(actorContext("session-a"), input)).status,
    ).toBe("already_applied");
    expect(f.store.proposals.proposals.size).toBe(1);
    expect(f.applier.calls).toHaveLength(1);
    const record = [...f.store.proposals.proposals.values()][0]!;
    expect(record.decision?.reason).toContain("actor=session-a");
  });

  it("changed payload or revision cannot reuse approval or action identity", async () => {
    let revision = "rev-1";
    const f = setup({ currentRevision: async () => revision });
    f.policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      actorId: "session-a",
      scopeKey: "scope-1",
      payloadFingerprint: await writePayloadFingerprint(input),
      revision,
      maxRisk: "destructive",
    });
    expect(
      modelData(await f.tool.execute(actorContext("session-a"), input)).status,
    ).toBe("ok");
    expect(
      modelData(
        await f.tool.execute(actorContext("session-a"), {
          ...input,
          value: 2,
        } as ToolInput),
      ).status,
    ).toBe("pending");
    revision = "rev-2";
    expect(
      modelData(await f.tool.execute(actorContext("session-a"), input)).status,
    ).toBe("pending");
    expect(f.store.proposals.proposals.size).toBe(3);
    expect(f.applier.calls).toHaveLength(1);
  });

  it("fails closed without trusted actor or after disconnect", async () => {
    const f = setup({ policyMode: "auto_all" });
    await expect(
      f.tool.execute({ ...CTX, metadata: { source: "mcp-server" } }, input),
    ).rejects.toThrow(
      "MCP proposal execution requires a trusted actor identity",
    );
    const controller = new AbortController();
    controller.abort();
    const result = await f.tool.execute(
      { ...actorContext("session-a"), signal: controller.signal },
      input,
    );
    expect(modelData(result).status).toBe("pending");
    expect(f.applier.calls).toHaveLength(0);
  });

  it("rechecks actor consent after an asynchronous apply claim and releases denied claims", async () => {
    const f = setup();
    f.policy.allow({
      chatId: "chat-1",
      toolName: "write_items",
      proposalKind: "items.write",
      actorId: "session-a",
      scopeKey: "scope-1",
      payloadFingerprint: await writePayloadFingerprint(input),
      revision: null,
      maxRisk: "destructive",
    });
    const transition = f.store.proposals.transition.bind(f.store.proposals);
    f.store.proposals.transition = async (
      ...args: Parameters<typeof transition>
    ) => {
      const record = await transition(...args);
      if (args[2] === "applying") f.policy.revokeActor("session-a");
      return record;
    };
    const result = await f.tool.execute(actorContext("session-a"), input);
    expect(result.ok).toBe(false);
    expect(f.applier.calls).toHaveLength(0);
    expect([...f.store.proposals.proposals.values()][0]!.status).toBe("failed");
    expect(f.store.proposals.outcomes.size).toBe(1);
  });

  it("reports already_applied with failed-op detail when a partial apply is repeated", async () => {
    // Port of mcp-server/tests/inbound-actors.test.ts "stages, then honors
    // host approval; partial mutation reports failure and is never
    // repeated", at the host lane the MCP handler delegates to.
    const f = setup({
      applierOptions: {
        outcome: {
          status: "partial",
          appliedOps: 1,
          failedOps: [{ opIndex: 1, error: "missing target" }],
        },
      },
    });
    const input = { action_id: "remove_item_scope-1" };
    const staged = await f.tool.execute(actorContext("session-a"), input);
    expect(modelData(staged).status).toBe("pending");
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

    const repeat = await f.tool.execute(actorContext("session-a"), input);
    expect(JSON.stringify(repeat)).toContain("already_applied");
    expect(repeat.ok).toBe(false);
    expect(repeat.status).toBe("partial");
    expect(modelData(repeat).status).toBe("already_applied");
    expect(modelData(repeat).appliedCount).toBe(1);
    expect(modelData(repeat).skipped).toEqual([
      { id: "op:1", reason: "missing target" },
    ]);
    // Never repeated: the partial apply is the only applier invocation.
    expect(f.applier.calls).toHaveLength(1);
  });
});
