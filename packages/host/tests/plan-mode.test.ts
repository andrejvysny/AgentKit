/**
 * Plan mode and the durable execution grant (ADR 0019 part 2): the guard blocks
 * writes, a plan approval the host records moves the session into an authorized
 * mode, and nothing about that decision lives in process memory.
 */
import { describe, expect, it } from "bun:test";
import {
  AgentSessionService,
  TaskService,
  TurnRunner,
  createUserInputContributor,
  executionStateForGrant,
  planRequestOf,
  readSessionExecution,
  withSessionExecution,
  type PlanApprovalPolicy,
} from "../src/index.js";
import { FakeAgentInputStore, FakeTaskRunner, createHarness } from "./fakes.js";

const PLAN_DIGEST = "a".repeat(64);
const POLICY: PlanApprovalPolicy = {
  toolNames: ["submit_plan"],
  grantedMode: "auto_edit",
};

function fixture(planApproval: PlanApprovalPolicy | null = POLICY) {
  const harness = createHarness();
  const taskRunner = new FakeTaskRunner();
  const inputs = new FakeAgentInputStore(harness.clock);
  harness.store.inputs = inputs;
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner,
    providerFactory: () => {
      throw new Error("no provider in this fixture");
    },
    contributors: [createUserInputContributor({ clock: harness.clock })],
    clock: harness.clock,
    ids: harness.ids,
  });
  const tasks = new TaskService({
    store: harness.store,
    taskRunner,
    ids: harness.ids,
    clock: harness.clock,
  });
  const service = new AgentSessionService({
    store: harness.store,
    turns: runner,
    tasks,
    taskRunner,
    clock: harness.clock,
    ids: harness.ids,
    ...(planApproval === null ? {} : { planApproval }),
  });
  return { harness, inputs, service, taskRunner, runner, tasks };
}

/** Park a turn on a plan-approval request, as a suspended run would have. */
async function parkPlanRequest(
  f: ReturnType<typeof fixture>,
  sessionId: string,
  input: {
    requestId?: string;
    toolName?: string;
    planDigest?: string;
    prompt?: string;
  } = {},
): Promise<{ taskId: string; requestId: string }> {
  const staged = await f.service.send(sessionId, { content: "start" });
  if (staged.kind !== "turn") throw new Error("expected a turn");
  const taskId = staged.turnId;
  await f.harness.store.tasks.transitionTask(taskId, ["queued"], "running");
  const requestId = input.requestId ?? `hreq:${taskId}:call-1`;
  await f.inputs.createInput({
    inputId: requestId,
    sessionId,
    kind: "human_request",
    content: {
      prompt: input.prompt ?? "Rewrite clamp(). Run bun test after.",
      toolName: input.toolName ?? "submit_plan",
      kind: "approval",
      argsSha256: input.planDigest ?? PLAN_DIGEST,
    },
    contentHash: "h",
    targetTaskId: taskId,
    attemptId: "att-1",
    toolCallId: "call-1",
  });
  await f.harness.store.tasks.transitionTask(
    taskId,
    ["running"],
    "waiting_approval",
  );
  return { taskId, requestId };
}

async function createSession(
  f: ReturnType<typeof fixture>,
  sessionId = "s1",
  executionMode?: string,
): Promise<string> {
  const state = await f.service.createSession({
    sessionId,
    profile: { id: "coding" },
    ...(executionMode === undefined ? {} : { executionMode }),
  });
  return state.sessionId;
}

describe("plan-mode metadata", () => {
  it("reads only a well-formed state and survives a round trip", () => {
    expect(readSessionExecution(undefined)).toBeUndefined();
    expect(
      readSessionExecution({ "agentkit.session.execution": { version: 2 } }),
    ).toBeUndefined();
    const state = executionStateForGrant(
      {
        mode: "auto_edit",
        planDigest: PLAN_DIGEST,
        requestId: "r1",
        turnId: "t1",
        approvedAt: "2026-01-01T00:00:00.000Z",
      },
      "2026-01-01T00:00:00.000Z",
    );
    const metadata = withSessionExecution({ other: 1 }, state);
    expect(readSessionExecution(metadata)).toEqual(state);
    // A malformed history entry invalidates the whole state, not just the entry.
    const broken = withSessionExecution(
      {},
      {
        ...state,
        history: [{ mode: "full_auto" } as never],
      },
    );
    expect(readSessionExecution(broken)).toBeUndefined();
    expect(withSessionExecution(metadata, undefined)["other"]).toBe(1);
    expect(
      "agentkit.session.execution" in withSessionExecution(metadata, undefined),
    ).toBe(false);
  });

  it("recognizes only the configured plan tool as a plan approval", () => {
    const record = {
      content: {
        kind: "approval",
        toolName: "submit_plan",
        argsSha256: PLAN_DIGEST,
        prompt: "the plan",
      },
    } as never;
    expect(planRequestOf(record, POLICY)).toEqual({
      planDigest: PLAN_DIGEST,
      plan: "the plan",
    });
    expect(planRequestOf(record, undefined)).toBeUndefined();
    expect(
      planRequestOf(
        {
          content: {
            kind: "approval",
            toolName: "edit_file",
            argsSha256: PLAN_DIGEST,
          },
        } as never,
        POLICY,
      ),
    ).toBeUndefined();
    // A per-call approval with no digest cannot be a grant.
    expect(
      planRequestOf(
        { content: { kind: "approval", toolName: "submit_plan" } } as never,
        POLICY,
      ),
    ).toBeUndefined();
  });
});

describe("plan approval", () => {
  it("writes the grant in the same answer that resumes the turn", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { taskId, requestId } = await parkPlanRequest(f, s);
    expect((await f.service.getExecution(s))?.mode).toBe("plan");

    const receipt = await f.service.approve(requestId, "ans-1");
    expect(receipt.duplicate).toBe(false);

    const state = await f.service.getExecution(s);
    expect(state?.mode).toBe("auto_edit");
    expect(state?.grantedPlan).toMatchObject({
      mode: "auto_edit",
      planDigest: PLAN_DIGEST,
      requestId,
      turnId: taskId,
    });
    expect(state?.history[0]?.planDigest).toBe(PLAN_DIGEST);
    // The turn is re-admitted, and the plan call was NOT answered with a tool
    // record: it still owes its result, which the approved re-run produces.
    const task = await f.harness.store.tasks.getTask(taskId);
    expect(task?.status).toBe("queued");
    expect(f.taskRunner.enqueued.some((e) => e.taskId === taskId)).toBe(true);
    expect(
      await f.harness.store.conversations.getMessage(receipt.messageId),
    ).toBeNull();
  });

  it("survives a restart: a new service over the same store sees the grant", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { requestId } = await parkPlanRequest(f, s);
    await f.service.approve(requestId, "ans-1");
    // "Restart": a second service, a second runner, the same durable store.
    const reopened = new AgentSessionService({
      store: f.harness.store,
      turns: f.runner,
      tasks: f.tasks,
      taskRunner: new FakeTaskRunner(),
      clock: f.harness.clock,
      ids: f.harness.ids,
      planApproval: POLICY,
    });
    expect((await reopened.getExecution(s))?.mode).toBe("auto_edit");
    expect(
      (await reopened.getState(s)).execution?.grantedPlan?.planDigest,
    ).toBe(PLAN_DIGEST);
  });

  it("a denial authorizes nothing", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { requestId } = await parkPlanRequest(f, s);
    await f.service.deny(requestId, "ans-1", "not this way");
    expect((await f.service.getExecution(s))?.mode).toBe("plan");
    expect((await f.service.getExecution(s))?.grantedPlan).toBeUndefined();
  });

  it("rejects a duplicated or modified approval", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { requestId } = await parkPlanRequest(f, s);
    await f.service.approve(requestId, "ans-1");
    // Same answerId, same response: an idempotent replay.
    const again = await f.service.approve(requestId, "ans-1");
    expect(again.duplicate).toBe(true);
    // Same answerId, different response: a modified approval.
    await expect(
      f.service.answer({
        requestId,
        answerId: "ans-1",
        response: { decision: "deny", note: "actually no" },
      }),
    ).rejects.toThrow(/already answered differently/);
    // A different answerId is a second human, not a retry.
    await expect(f.service.approve(requestId, "ans-2")).rejects.toThrow(
      /already answered differently/,
    );
    // The grant was not rewritten by any of that.
    expect((await f.service.getExecution(s))?.grantedPlan?.requestId).toBe(
      requestId,
    );
  });

  it("refuses to approve a request whose turn is no longer parked", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { taskId, requestId } = await parkPlanRequest(f, s);
    await f.harness.store.tasks.transitionTask(
      taskId,
      ["waiting_approval"],
      "cancelled",
      { finishedAt: f.harness.clock.nowIso() },
    );
    await expect(f.service.approve(requestId, "ans-1")).rejects.toThrow(
      /can no longer be answered/,
    );
    expect((await f.service.getExecution(s))?.mode).toBe("plan");
  });

  it("the host veto keeps the session in plan mode and is reported to the model", async () => {
    const f = fixture({
      ...POLICY,
      authorize: ({ planDigest }) =>
        planDigest === PLAN_DIGEST
          ? { allowed: false, reason: "reviewer roles may not plan edits" }
          : true,
    });
    const s = await createSession(f, "s1", "plan");
    const { requestId } = await parkPlanRequest(f, s);
    await f.service.approve(requestId, "ans-1");
    expect((await f.service.getExecution(s))?.mode).toBe("plan");
    expect((await f.service.getExecution(s))?.grantedPlan).toBeUndefined();
    const record = await f.inputs.getInput(requestId);
    expect(record?.status).toBe("answered");
    const message = await f.harness.store.conversations.getMessage(
      record?.appliedMessageId ?? "",
    );
    expect(message?.content).toContain("plan_not_authorized");
  });

  it("resolves a function grantedMode per session", async () => {
    const f = fixture({
      toolNames: ["submit_plan"],
      grantedMode: ({ sessionId }) =>
        sessionId === "s1" ? "auto_edit" : "full_auto",
    });
    const a = await createSession(f, "s1", "plan");
    const b = await createSession(f, "s2", "plan");
    const ra = await parkPlanRequest(f, a);
    const rb = await parkPlanRequest(f, b);
    await f.service.approve(ra.requestId, "ans-1");
    await f.service.approve(rb.requestId, "ans-2");
    expect((await f.service.getExecution(a))?.mode).toBe("auto_edit");
    expect((await f.service.getExecution(b))?.mode).toBe("full_auto");
    expect((await f.service.getExecution(b))?.grantedPlan?.mode).toBe(
      "full_auto",
    );
  });

  it("denies when the resolved grantedMode is plan or empty", async () => {
    for (const mode of ["plan", ""]) {
      const f = fixture({
        toolNames: ["submit_plan"],
        grantedMode: async () => mode,
      });
      const s = await createSession(f, "s1", "plan");
      const { requestId } = await parkPlanRequest(f, s);
      await f.service.approve(requestId, "ans-1");
      expect((await f.service.getExecution(s))?.mode).toBe("plan");
      expect((await f.service.getExecution(s))?.grantedPlan).toBeUndefined();
      const record = await f.inputs.getInput(requestId);
      const message = await f.harness.store.conversations.getMessage(
        record?.appliedMessageId ?? "",
      );
      expect(message?.content).toContain(
        "plan_not_authorized: invalid granted mode",
      );
    }
  });

  it("repairs a grant lost between the marker and the metadata", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { requestId } = await parkPlanRequest(f, s);
    await f.service.approve(requestId, "ans-1");
    // Simulate the store that cannot roll back: the answer committed, the
    // grant did not.
    const chat = await f.harness.store.conversations.getChat(s);
    await f.harness.store.conversations.updateChat(s, {
      metadata: withSessionExecution(chat?.metadata, undefined),
    });
    expect((await f.service.getExecution(s))?.grantedPlan).toBeUndefined();
    const replay = await f.service.approve(requestId, "ans-1");
    expect(replay.duplicate).toBe(true);
    expect((await f.service.getExecution(s))?.grantedPlan?.planDigest).toBe(
      PLAN_DIGEST,
    );
  });

  it("exposes a pending plan in the session state", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { requestId } = await parkPlanRequest(f, s, {
      prompt: "the plan body",
    });
    const state = await f.service.getState(s);
    expect(state.waitingPlan).toMatchObject({
      requestId,
      toolName: "submit_plan",
      planDigest: PLAN_DIGEST,
      plan: "the plan body",
    });
    expect(state.waitingApproval?.toolCallId).toBe("call-1");
  });

  it("keeps a parked approval through a refused compaction request", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { taskId, requestId } = await parkPlanRequest(f, s);
    // Compaction runs on a turn's own pass; a live turn is refused, not changed.
    await expect(f.service.compact(s)).rejects.toMatchObject({
      code: "chat_busy",
    });
    const state = await f.service.getState(s);
    expect(state.activeTurn?.status).toBe("waiting_approval");
    expect(state.waitingApproval?.toolCallId).toBe("call-1");
    const receipt = await f.service.approve(requestId, "ans-1");
    expect(receipt.turnId).toBe(taskId);
    expect(receipt.duplicate).toBe(false);
  });

  it("without a policy wired, a plan answer is only a per-call approval", async () => {
    const f = fixture(null);
    const s = await createSession(f, "s1", "plan");
    const { requestId } = await parkPlanRequest(f, s);
    await f.service.approve(requestId, "ans-1");
    expect((await f.service.getExecution(s))?.grantedPlan).toBeUndefined();
  });

  it("entering plan mode again drops the previous grant", async () => {
    const f = fixture();
    const s = await createSession(f, "s1", "plan");
    const { requestId } = await parkPlanRequest(f, s);
    await f.service.approve(requestId, "ans-1");
    expect((await f.service.getExecution(s))?.mode).toBe("auto_edit");
    const state = await f.service.setExecutionMode(s, "plan");
    expect(state.mode).toBe("plan");
    expect(state.grantedPlan).toBeUndefined();
    expect(state.history).toHaveLength(1); // the audit trail survives
  });
});
