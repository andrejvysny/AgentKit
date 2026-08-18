import { describe, expect, it } from "bun:test";
import {
  INTERRUPTED_REASON,
  ProposalService,
  SessionWritePolicy,
  recoverOnBoot,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

interface Fixture extends TestHarness {
  service: ProposalService;
}

function setup(): Fixture {
  const harness = createHarness();
  const service = new ProposalService({
    store: harness.store,
    applier: harness.applier,
    policy: new SessionWritePolicy({ clock: harness.clock }),
    clock: harness.clock,
    ids: harness.ids,
  });
  return { ...harness, service };
}

/**
 * Leave a proposal exactly where a process that died mid-apply leaves one:
 * `applying`, with the operation id it claimed under already persisted.
 */
async function stageApproveAndClaim(
  f: Fixture,
  operationId: string,
): Promise<string> {
  const proposal = await f.service.stage({
    chatId: "chat-1",
    scopeKey: "scope-1",
    toolName: "write_items",
    kind: "items.write",
    risk: "low",
    operations: [{ op: "add" }],
  });
  await f.service.approve({ proposalId: proposal.id, actor: "user" });
  await f.store.proposals.transition(proposal.id, ["approved"], "applying", {
    operationId,
  });
  return proposal.id;
}

describe("recoverOnBoot", () => {
  it("recovers the queue and settles the proposals a crash left applying", async () => {
    const f = setup();
    const stuck = await stageApproveAndClaim(f, "op-boot-1");

    const result = await recoverOnBoot({
      taskRunner: f.taskRunner,
      proposals: f.service,
    });

    // The queue's own recovery ran — and ran first, before anything could
    // claim a run whose proposals were still unresolved.
    expect(f.taskRunner.recoverCalls).toBe(1);
    expect(result).toEqual({ proposalsReconciled: 1 });

    // Nothing could prove the write landed, so it is failed, not retried.
    const settled = await f.store.proposals.get(stuck);
    expect(settled?.status).toBe("failed");
    expect(settled?.reason).toBe(INTERRUPTED_REASON);
    expect(f.applier.calls).toHaveLength(0);
  });

  it("reports zero on a clean boot, and is safe to run twice", async () => {
    const f = setup();

    expect(
      await recoverOnBoot({ taskRunner: f.taskRunner, proposals: f.service }),
    ).toEqual({ proposalsReconciled: 0 });
    expect(
      await recoverOnBoot({ taskRunner: f.taskRunner, proposals: f.service }),
    ).toEqual({ proposalsReconciled: 0 });
    expect(f.taskRunner.recoverCalls).toBe(2);
  });

  it("logs the reconciliation split when a logger is wired", async () => {
    const f = setup();
    await stageApproveAndClaim(f, "op-boot-2");
    const lines: { message: string; fields?: Record<string, unknown> }[] = [];
    const logger = {
      debug: () => {},
      info: (message: string, fields?: Record<string, unknown>) => {
        lines.push({ message, ...(fields === undefined ? {} : { fields }) });
      },
      warn: () => {},
      error: () => {},
    };

    await recoverOnBoot({
      taskRunner: f.taskRunner,
      proposals: f.service,
      logger,
    });

    expect(lines).toHaveLength(1);
    expect(lines[0]?.fields).toEqual({
      proposalsReconciled: 1,
      proposalsApplied: 0,
      proposalsFailed: 1,
    });
  });
});
