import { describe, expect, it } from "bun:test";
import {
  DuplicateActionIdError,
  INTERRUPTED_REASON,
  ProposalService,
  REVISION_CONFLICT_REASON,
  RevisionConflictError,
  SessionWritePolicy,
  type ApplyOutcome,
  type ProposalRecord,
  type StageProposalInput,
} from "../src/index.js";
import { createHarness, type FakeApplierOptions, type TestHarness } from "./fakes.js";

interface Fixture extends TestHarness {
  service: ProposalService;
}

function setup(applierOptions: FakeApplierOptions = {}): Fixture {
  const harness = createHarness(applierOptions);
  const service = new ProposalService({
    store: harness.store,
    applier: harness.applier,
    policy: new SessionWritePolicy({ clock: harness.clock }),
    clock: harness.clock,
    ids: harness.ids,
  });
  return { ...harness, service };
}

function stageInput(
  overrides: Partial<StageProposalInput> = {},
): StageProposalInput {
  return {
    chatId: "chat-1",
    runId: "run-1",
    scopeKey: "scope-1",
    toolName: "write_items",
    kind: "items.write",
    risk: "low",
    operations: [{ op: "add" }],
    warnings: [],
    truncated: false,
    ...overrides,
  };
}

describe("ProposalService — stage → approve → apply", () => {
  it("runs the happy path and records who approved it", async () => {
    const f = setup();
    const proposal = await f.service.stage(stageInput());
    expect(proposal.status).toBe("pending");
    expect(proposal.decision).toBeUndefined();

    const approved = await f.service.approve({
      proposalId: proposal.id,
      actor: "user",
      decidedBy: "ada",
      reason: "looks right",
    });
    expect(approved.status).toBe("approved");
    expect(approved.decision).toEqual({
      actor: "user",
      decidedBy: "ada",
      reason: "looks right",
      decidedAt: f.clock.nowIso(),
    });

    const outcome = await f.service.apply({
      proposalId: proposal.id,
      operationId: "op-A",
    });
    expect(outcome).toEqual({ status: "applied", appliedOps: 1, failedOps: [] });

    const stored = (await f.store.proposals.get(proposal.id)) as ProposalRecord;
    expect(stored.status).toBe("applied");
    expect(stored.appliedAt).toBe(f.clock.nowIso());
    // The operation id is written at claim time, before any side effect, so
    // recovery can find it.
    expect(stored.operationId).toBe("op-A");
    expect(await f.store.proposals.getOutcome("op-A")).toEqual(outcome);
  });

  it("lands a partial outcome as `applied` and keeps the partiality on the outcome", async () => {
    const f = setup({
      outcome: {
        status: "partial",
        appliedOps: 2,
        failedOps: [{ opIndex: 2, error: "target missing" }],
      },
    });
    const proposal = await f.service.stage(stageInput());
    await f.service.approve({ proposalId: proposal.id, actor: "user" });
    const outcome = await f.service.apply({
      proposalId: proposal.id,
      operationId: "op-P",
    });

    expect(outcome.status).toBe("partial");
    const stored = (await f.store.proposals.get(proposal.id)) as ProposalRecord;
    expect(stored.status).toBe("applied");
  });

  it("rejects a policy approval with no policyId, and a user approval carrying one", async () => {
    const f = setup();
    const proposal = await f.service.stage(stageInput());
    await expect(
      f.service.approve({ proposalId: proposal.id, actor: "policy" }),
    ).rejects.toThrow(/policyId/);
    await expect(
      f.service.approve({
        proposalId: proposal.id,
        actor: "user",
        policyId: "session-write-policy",
      }),
    ).rejects.toThrow(/must not carry a policyId/);
    // Neither attempt moved the record.
    expect((await f.store.proposals.get(proposal.id))?.status).toBe("pending");
  });

  it("rejects a pending proposal on the user's behalf", async () => {
    const f = setup();
    const proposal = await f.service.stage(stageInput());
    const rejected = await f.service.reject({
      proposalId: proposal.id,
      decidedBy: "ada",
      reason: "not what I meant",
    });
    expect(rejected.status).toBe("rejected");
    expect(rejected.decision?.actor).toBe("user");
  });
});

describe("ProposalService — idempotent apply", () => {
  it("replays the recorded outcome and never calls the applier twice", async () => {
    const f = setup();
    const proposal = await f.service.stage(stageInput());
    await f.service.approve({ proposalId: proposal.id, actor: "user" });

    const first = await f.service.apply({
      proposalId: proposal.id,
      operationId: "op-1",
    });
    const second = await f.service.apply({
      proposalId: proposal.id,
      operationId: "op-1",
    });

    expect(second).toEqual(first);
    expect(f.applier.calls).toHaveLength(1);
    expect((await f.store.proposals.get(proposal.id))?.status).toBe("applied");
  });

  it("finalizes a proposal left in `applying` when the outcome is replayed", async () => {
    const f = setup();
    const proposal = await f.service.stage(stageInput());
    await f.service.approve({ proposalId: proposal.id, actor: "user" });
    // Simulate the crash window: claimed + outcome recorded, status never moved.
    await f.store.proposals.transition(proposal.id, ["approved"], "applying", {
      operationId: "op-crash",
    });
    const outcome: ApplyOutcome = {
      status: "applied",
      appliedOps: 3,
      failedOps: [],
    };
    await f.store.proposals.recordOutcome("op-crash", outcome);

    const replayed = await f.service.apply({
      proposalId: proposal.id,
      operationId: "op-crash",
    });
    expect(replayed).toEqual(outcome);
    expect(f.applier.calls).toHaveLength(0);
    expect((await f.store.proposals.get(proposal.id))?.status).toBe("applied");
  });

  it("adopts an outcome the applier remembers but we never recorded", async () => {
    const f = setup();
    const proposal = await f.service.stage(stageInput());
    await f.service.approve({ proposalId: proposal.id, actor: "user" });
    // The write landed; we died before writing it down. The applier remembers.
    f.applier.remembered.set("op-lost", {
      status: "applied",
      appliedOps: 5,
      failedOps: [],
    });

    const outcome = await f.service.apply({
      proposalId: proposal.id,
      operationId: "op-lost",
    });
    expect(outcome.appliedOps).toBe(5);
    expect(f.applier.calls).toHaveLength(0);
    // Persisted on the way through, so the store is authoritative from now on.
    expect(await f.store.proposals.getOutcome("op-lost")).toEqual(outcome);
  });
});

describe("ProposalService — apply failures", () => {
  it("records a failed outcome and finalizes when the applier throws", async () => {
    const f = setup({ throws: new Error("backend exploded") });
    const proposal = await f.service.stage(stageInput());
    await f.service.approve({ proposalId: proposal.id, actor: "user" });

    await expect(
      f.service.apply({ proposalId: proposal.id, operationId: "op-boom" }),
    ).rejects.toThrow("backend exploded");

    const stored = (await f.store.proposals.get(proposal.id)) as ProposalRecord;
    expect(stored.status).toBe("failed");
    expect(stored.reason).toContain("backend exploded");
    // Never left `applying` without an outcome — that is reconcile's contract.
    expect(await f.store.proposals.getOutcome("op-boom")).toEqual({
      status: "failed",
      appliedOps: 0,
      failedOps: [{ opIndex: 0, error: "backend exploded" }],
    });
  });

  it("finalizes a reported failure without throwing", async () => {
    const f = setup({
      outcome: {
        status: "failed",
        appliedOps: 0,
        failedOps: [{ opIndex: 0, error: "rejected by host" }],
      },
    });
    const proposal = await f.service.stage(stageInput());
    await f.service.approve({ proposalId: proposal.id, actor: "user" });
    const outcome = await f.service.apply({
      proposalId: proposal.id,
      operationId: "op-f",
    });
    expect(outcome.status).toBe("failed");
    const stored = (await f.store.proposals.get(proposal.id)) as ProposalRecord;
    expect(stored.status).toBe("failed");
    expect(stored.reason).toBe("op 0: rejected by host");
  });
});

describe("ProposalService — idempotency keys and staleness", () => {
  it("refuses a duplicate (scopeKey, actionId)", async () => {
    const f = setup();
    await f.service.stage(stageInput({ actionId: "create_a_scope-1" }));
    await expect(
      f.service.stage(stageInput({ actionId: "create_a_scope-1" })),
    ).rejects.toThrow(DuplicateActionIdError);
    // A different scope is a different namespace.
    await expect(
      f.service.stage(
        stageInput({ actionId: "create_a_scope-1", scopeKey: "scope-2" }),
      ),
    ).resolves.toBeDefined();
  });

  it("invalidates a still-pending proposal whose scope moved on", async () => {
    const f = setup({ revisions: { "scope-1": "rev-1" } });
    const proposal = await f.service.stage(
      stageInput({ revisionAtCreate: "rev-1" }),
    );
    f.applier.revisions.set("scope-1", "rev-2");

    await expect(
      f.service.apply({ proposalId: proposal.id, operationId: "op-stale" }),
    ).rejects.toThrow(RevisionConflictError);

    const stored = (await f.store.proposals.get(proposal.id)) as ProposalRecord;
    expect(stored.status).toBe("invalidated");
    expect(stored.reason).toBe(REVISION_CONFLICT_REASON);
    expect(f.applier.calls).toHaveLength(0);
  });

  it("fails an already-approved proposal whose scope moved on", async () => {
    const f = setup({ revisions: { "scope-1": "rev-1" } });
    const proposal = await f.service.stage(
      stageInput({ revisionAtCreate: "rev-1" }),
    );
    await f.service.approve({ proposalId: proposal.id, actor: "user" });
    f.applier.revisions.set("scope-1", "rev-2");

    await expect(
      f.service.apply({ proposalId: proposal.id, operationId: "op-stale2" }),
    ).rejects.toThrow(RevisionConflictError);

    const stored = (await f.store.proposals.get(proposal.id)) as ProposalRecord;
    expect(stored.status).toBe("failed");
    expect(stored.reason).toBe(REVISION_CONFLICT_REASON);
    expect(f.applier.calls).toHaveLength(0);
  });

  it("applies when the scope is unchanged, and when it has no revision at all", async () => {
    const fresh = setup({ revisions: { "scope-1": "rev-1" } });
    const p1 = await fresh.service.stage(stageInput({ revisionAtCreate: "rev-1" }));
    await fresh.service.approve({ proposalId: p1.id, actor: "user" });
    await expect(
      fresh.service.apply({ proposalId: p1.id, operationId: "op-ok" }),
    ).resolves.toMatchObject({ status: "applied" });

    const unversioned = setup();
    const p2 = await unversioned.service.stage(
      stageInput({ revisionAtCreate: "rev-9" }),
    );
    await unversioned.service.approve({ proposalId: p2.id, actor: "user" });
    await expect(
      unversioned.service.apply({ proposalId: p2.id, operationId: "op-ok2" }),
    ).resolves.toMatchObject({ status: "applied" });
  });

  it("invalidateForRevision sweeps every pending proposal in the scope", async () => {
    const f = setup();
    const a = await f.service.stage(stageInput({ actionId: "create_a_scope-1" }));
    const b = await f.service.stage(stageInput({ actionId: "create_b_scope-1" }));
    const other = await f.service.stage(stageInput({ scopeKey: "scope-2" }));
    await f.service.approve({ proposalId: b.id, actor: "user" });

    const count = await f.service.invalidateForRevision({
      scopeKey: "scope-1",
      newRevision: "rev-7",
    });
    expect(count).toBe(1);
    expect((await f.store.proposals.get(a.id))?.status).toBe("invalidated");
    // Approved proposals are NOT swept — an approval is a fact, and the apply
    // path fails them with revision_conflict instead.
    expect((await f.store.proposals.get(b.id))?.status).toBe("approved");
    expect((await f.store.proposals.get(other.id))?.status).toBe("pending");
  });
});

describe("ProposalService — reconcileInterrupted", () => {
  it("finalizes to applied when the applier can prove the write landed", async () => {
    const f = setup();
    const proposal = await f.service.stage(stageInput());
    await f.service.approve({ proposalId: proposal.id, actor: "user" });
    await f.store.proposals.transition(proposal.id, ["approved"], "applying", {
      operationId: "op-crash",
    });
    f.applier.remembered.set("op-crash", {
      status: "partial",
      appliedOps: 2,
      failedOps: [{ opIndex: 3, error: "boom" }],
    });

    const report = await f.service.reconcileInterrupted();
    expect(report).toEqual({ reconciled: 1, applied: 1, failed: 0 });
    expect((await f.store.proposals.get(proposal.id))?.status).toBe("applied");
  });

  it("fails with reason 'interrupted' when nothing can prove it landed", async () => {
    const f = setup();
    const proposal = await f.service.stage(stageInput());
    await f.service.approve({ proposalId: proposal.id, actor: "user" });
    await f.store.proposals.transition(proposal.id, ["approved"], "applying", {
      operationId: "op-gone",
    });

    const report = await f.service.reconcileInterrupted();
    expect(report).toEqual({ reconciled: 1, applied: 0, failed: 1 });
    const stored = (await f.store.proposals.get(proposal.id)) as ProposalRecord;
    expect(stored.status).toBe("failed");
    expect(stored.reason).toBe(INTERRUPTED_REASON);
    // Recorded, so a redelivered apply for that operation replays the failure
    // instead of running the write a second time.
    expect(await f.store.proposals.getOutcome("op-gone")).toEqual({
      status: "failed",
      appliedOps: 0,
      failedOps: [],
    });
  });

  it("is a no-op when nothing is stuck", async () => {
    const f = setup();
    await f.service.stage(stageInput());
    expect(await f.service.reconcileInterrupted()).toEqual({
      reconciled: 0,
      applied: 0,
      failed: 0,
    });
  });
});
