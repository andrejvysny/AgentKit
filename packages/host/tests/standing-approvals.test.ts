/**
 * Standing approvals: a human may remember an approval of one exact call for the
 * rest of the session. The grant is written in the answering transaction, matched
 * only after every guard has spoken, and never weakens the one-call guarantees.
 */
import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { resolveToolLimits, type AiTool } from "@agentkit/core";
import {
  AgentSessionService,
  AnswerConflictError,
  RecordNotFoundError,
  STANDING_APPROVALS_METADATA_KEY,
  TaskService,
  TurnRunner,
  createUserInputContributor,
  readStandingApprovals,
  stageRegistry,
  type AssistantStore,
  type StandingApprovalGrant,
  type StandingApprovalPolicy,
  type ToolGuard,
} from "../src/index.js";
import { SqliteAssistantStore } from "../../adapters-sqlite/src/index.js";
import {
  FakeAgentInputStore,
  FakeTaskRunner,
  createHarness,
  createTestClock,
  createTestIds,
} from "./fakes.js";

const DIGEST = "b".repeat(64);
const OTHER_DIGEST = "c".repeat(64);
const STANDING = { digest: DIGEST, label: "bun test" };

interface Fixture {
  store: AssistantStore;
  inputs: AssistantStore["inputs"] & object;
  service: AgentSessionService;
  clock: ReturnType<typeof createTestClock>;
}

function serviceOver(
  store: AssistantStore,
  clock: ReturnType<typeof createTestClock>,
  ids = createTestIds(),
): Fixture {
  const taskRunner = new FakeTaskRunner();
  const runner = new TurnRunner({
    store,
    taskRunner,
    providerFactory: () => {
      throw new Error("no provider in this fixture");
    },
    contributors: [createUserInputContributor({ clock })],
    clock,
    ids,
  });
  const tasks = new TaskService({ store, taskRunner, ids, clock });
  const service = new AgentSessionService({
    store,
    turns: runner,
    tasks,
    taskRunner,
    clock,
    ids,
  });
  return { store, inputs: store.inputs!, service, clock };
}

function fakeFixture(): Fixture {
  const harness = createHarness();
  harness.store.inputs = new FakeAgentInputStore(harness.clock);
  return serviceOver(harness.store, harness.clock, harness.ids);
}

async function session(f: Fixture, sessionId = "s1"): Promise<string> {
  return (
    await f.service.createSession({ sessionId, profile: { id: "coding" } })
  ).sessionId;
}

/** Park a turn on an approval request, as a suspended run would have. */
async function park(
  f: Fixture,
  sessionId: string,
  options: {
    standing?: unknown;
    expiresAt?: string;
    suffix?: string;
  } = {},
): Promise<{ taskId: string; requestId: string }> {
  const staged = await f.service.send(sessionId, { content: "start" });
  if (staged.kind !== "turn") throw new Error("expected a turn");
  const taskId = staged.turnId;
  await f.store.tasks.transitionTask(taskId, ["queued"], "running");
  const requestId = `hreq:${taskId}:call-1${options.suffix ?? ""}`;
  await f.inputs.createInput({
    inputId: requestId,
    sessionId,
    kind: "human_request",
    content: {
      prompt: "Approve run_command?",
      toolName: "run_command",
      kind: "approval",
      argsSha256: "a".repeat(64),
      ...(options.standing === undefined ? {} : { standing: options.standing }),
    },
    contentHash: "h",
    targetTaskId: taskId,
    attemptId: "att-1",
    toolCallId: "call-1",
    ...(options.expiresAt === undefined
      ? {}
      : { expiresAt: options.expiresAt }),
  });
  await f.store.tasks.transitionTask(taskId, ["running"], "waiting_approval");
  return { taskId, requestId };
}

describe("remembering an approval", () => {
  it("writes a grant in the answering transaction and lists it newest first", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const { requestId } = await park(f, s, { standing: STANDING });
    await f.service.approve(requestId, "ans-1", undefined, { remember: true });
    const [grant] = await f.service.listApprovalGrants(s);
    expect(grant).toEqual({
      grantId: `grant:${requestId}`,
      digest: DIGEST,
      label: "bun test",
      toolName: "run_command",
      sourceRequestId: requestId,
      answerId: "ans-1",
      grantedAt: f.clock.nowIso(),
    });
  });

  it("an approve without remember writes nothing", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const { requestId } = await park(f, s, { standing: STANDING });
    await f.service.approve(requestId, "ans-1");
    expect(await f.service.listApprovalGrants(s)).toEqual([]);
  });

  it("refuses remember on a denial and on a non-boolean, leaving the request pending", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const { requestId } = await park(f, s, { standing: STANDING });
    for (const response of [
      { decision: "deny", remember: true },
      { decision: "deny", remember: false },
      { decision: "approve", remember: "yes" },
    ])
      await expect(
        f.service.answer({ requestId, answerId: "ans-1", response }),
      ).rejects.toMatchObject({ code: "invalid_input" });
    expect((await f.inputs.getInput(requestId))?.status).toBe("pending");
    expect(await f.service.listApprovalGrants(s)).toEqual([]);
  });

  it("refuses remember when the call offered no standing approval", async () => {
    for (const standing of [undefined, { digest: "short", label: "x" }, 7]) {
      const f = fakeFixture();
      const s = await session(f);
      const { requestId } = await park(f, s, {
        ...(standing === undefined ? {} : { standing }),
      });
      await expect(
        f.service.approve(requestId, "ans-1", undefined, { remember: true }),
      ).rejects.toThrow("This call is not eligible for a standing approval.");
      expect((await f.inputs.getInput(requestId))?.status).toBe("pending");
      expect(await f.service.listApprovalGrants(s)).toEqual([]);
    }
  });

  it("a cancelled or expired request never writes a grant", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const cancelled = await park(f, s, { standing: STANDING });
    await f.store.tasks.transitionTask(
      cancelled.taskId,
      ["waiting_approval"],
      "cancelled",
      { finishedAt: f.clock.nowIso() },
    );
    await expect(
      f.service.approve(cancelled.requestId, "ans-1", undefined, {
        remember: true,
      }),
    ).rejects.toThrow(/can no longer be answered/);

    const g = fakeFixture();
    const s2 = await session(g);
    const expiring = await park(g, s2, {
      standing: STANDING,
      expiresAt: "2026-01-01T00:00:10.000Z",
    });
    g.clock.advance(60_000);
    await expect(
      g.service.approve(expiring.requestId, "ans-1", undefined, {
        remember: true,
      }),
    ).rejects.toThrow(/expired/);
    expect(await f.service.listApprovalGrants(s)).toEqual([]);
    expect(await g.service.listApprovalGrants(s2)).toEqual([]);
  });

  it("a lost concurrent answer writes no second grant", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const { requestId } = await park(f, s, { standing: STANDING });
    await f.service.approve(requestId, "ans-1", undefined, { remember: true });
    await expect(
      f.service.approve(requestId, "ans-2", undefined, { remember: true }),
    ).rejects.toBeInstanceOf(AnswerConflictError);
    expect(await f.service.listApprovalGrants(s)).toHaveLength(1);
  });

  it("a duplicate answer is idempotent; a replay with another remember conflicts", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const { requestId } = await park(f, s, { standing: STANDING });
    await f.service.approve(requestId, "ans-1", undefined, { remember: true });
    const again = await f.service.approve(requestId, "ans-1", undefined, {
      remember: true,
    });
    expect(again.duplicate).toBe(true);
    expect(await f.service.listApprovalGrants(s)).toHaveLength(1);
    await expect(f.service.approve(requestId, "ans-1")).rejects.toBeInstanceOf(
      AnswerConflictError,
    );
    await expect(
      f.service.answer({
        requestId,
        answerId: "ans-1",
        response: { decision: "approve", remember: false },
      }),
    ).rejects.toBeInstanceOf(AnswerConflictError);
    expect(await f.service.listApprovalGrants(s)).toHaveLength(1);
  });

  it("repairs a grant lost after the approval marker was written", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const { requestId } = await park(f, s, { standing: STANDING });
    await f.service.approve(requestId, "ans-1", undefined, { remember: true });
    const chat = await f.store.conversations.getChat(s);
    const { [STANDING_APPROVALS_METADATA_KEY]: _lost, ...rest } =
      chat!.metadata;
    await f.store.conversations.updateChat(s, { metadata: rest });
    expect(await f.service.listApprovalGrants(s)).toEqual([]);
    await f.service.approve(requestId, "ans-1", undefined, { remember: true });
    expect(await f.service.listApprovalGrants(s)).toHaveLength(1);
  });

  it("refuses to overwrite unreadable grants and commits nothing", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const chat = await f.store.conversations.getChat(s);
    await f.store.conversations.updateChat(s, {
      metadata: {
        ...chat!.metadata,
        [STANDING_APPROVALS_METADATA_KEY]: { version: 9, grants: "junk" },
      },
    });
    const { taskId, requestId } = await park(f, s, { standing: STANDING });
    await expect(
      f.service.approve(requestId, "ans-1", undefined, { remember: true }),
    ).rejects.toMatchObject({ code: "invalid_input" });
    expect((await f.inputs.getInput(requestId))?.status).toBe("pending");
    expect((await f.store.tasks.getTask(taskId))?.status).toBe(
      "waiting_approval",
    );
    expect(await f.service.listApprovalGrants(s)).toEqual([]);
  });

  it("keeps at most 256 grants: revoked ones make room, active ones do not", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const grants: StandingApprovalGrant[] = Array.from(
      { length: 256 },
      (_, i) => ({
        grantId: `grant:g${i}`,
        digest: i.toString(16).padStart(64, "0"),
        label: `cmd ${i}`,
        toolName: "run_command",
        sourceRequestId: `g${i}`,
        answerId: "a",
        grantedAt: "2025-12-31T00:00:00.000Z",
      }),
    );
    const fill = async (list: StandingApprovalGrant[]) => {
      const chat = await f.store.conversations.getChat(s);
      await f.store.conversations.updateChat(s, {
        metadata: {
          ...chat!.metadata,
          [STANDING_APPROVALS_METADATA_KEY]: { version: 1, grants: list },
        },
      });
    };
    await fill(grants);
    const { requestId } = await park(f, s, { standing: STANDING });
    await expect(
      f.service.approve(requestId, "ans-1", undefined, { remember: true }),
    ).rejects.toThrow(/at most 256/);
    await f.service.revokeApprovalGrant(s, "grant:g5");
    await f.service.approve(requestId, "ans-1", undefined, { remember: true });
    const kept = await f.service.listApprovalGrants(s);
    expect(kept).toHaveLength(256);
    expect(kept.some((g) => g.grantId === "grant:g5")).toBe(false);
    expect(kept[0]?.grantId).toBe(`grant:${requestId}`);
  });

  it("rejects a caller-supplied grants key at createSession", async () => {
    const f = fakeFixture();
    await expect(
      f.service.createSession({
        profile: { id: "coding" },
        metadata: {
          [STANDING_APPROVALS_METADATA_KEY]: { version: 1, grants: [] },
        },
      }),
    ).rejects.toMatchObject({ code: "invalid_input" });
  });
});

describe("listing and revoking", () => {
  it("revokes once, idempotently, and reports unknown ids", async () => {
    const f = fakeFixture();
    const s = await session(f);
    const { requestId } = await park(f, s, { standing: STANDING });
    await f.service.approve(requestId, "ans-1", undefined, { remember: true });
    const grantId = `grant:${requestId}`;
    f.clock.advance(1000);
    const revoked = await f.service.revokeApprovalGrant(s, grantId);
    expect(revoked.revokedAt).toBe(f.clock.nowIso());
    f.clock.advance(1000);
    const again = await f.service.revokeApprovalGrant(s, grantId);
    expect(again.revokedAt).toBe(revoked.revokedAt);
    expect(await f.service.listApprovalGrants(s)).toEqual([revoked]);
    await expect(
      f.service.revokeApprovalGrant(s, "grant:nope"),
    ).rejects.toBeInstanceOf(RecordNotFoundError);
    await expect(f.service.listApprovalGrants("missing")).rejects.toThrow();
  });
});

describe("workspace trust", () => {
  it("changes only the trusted flag of a session with a workspace", async () => {
    const f = fakeFixture();
    const withWorkspace = (
      await f.service.createSession({
        sessionId: "w",
        profile: { id: "coding" },
        workspace: { root: "/tmp/ws", trusted: false },
      })
    ).sessionId;
    const state = await f.service.configure(withWorkspace, {
      workspaceTrusted: true,
    });
    expect(state.descriptor.workspace).toEqual({
      root: "/tmp/ws",
      trusted: true,
    });
    const plain = await session(f, "p");
    await expect(
      f.service.configure(plain, { workspaceTrusted: true }),
    ).rejects.toMatchObject({ code: "invalid_input" });
  });
});

// --- reuse in the guarded tool -------------------------------------------------

const limits = resolveToolLimits({ preference: "small" });

interface Run {
  executed: number;
  suspended: Record<string, unknown>[];
  result: { ok: boolean };
}

async function callOnce(options: {
  store: AssistantStore;
  policy?: StandingApprovalPolicy;
  extraGuards?: ToolGuard[];
  command?: string;
  chatId?: string;
}): Promise<Run> {
  const state = { executed: 0 };
  const tool: AiTool = {
    definition: {
      name: "run_command",
      version: "1.0.0",
      effect: "write",
      capability: "process.exec",
      description: "run",
      inputSchema: { type: "object" },
    },
    async execute(ctx) {
      state.executed += 1;
      return {
        ok: true,
        data: {},
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };
  const askGuard: ToolGuard = {
    canExecute: () => ({
      allowed: false,
      reason: "approval_required: run_command",
      approval: {},
    }),
  };
  const staged = await stageRegistry({
    contributors: [{ namespace: "coding", contribute: async () => [tool] }],
    ctx: { chatId: "s1", runId: "r1", bindings: [], limits },
    hasPrimaryBinding: true,
    guards: [askGuard, ...(options.extraGuards ?? [])],
    ...(options.policy === undefined
      ? {}
      : { standing: { policy: options.policy, store: options.store } }),
  });
  const suspended: Record<string, unknown>[] = [];
  const result = await staged.registry.get("run_command")!.execute(
    {
      runId: "r1",
      chatId: options.chatId ?? "s1",
      bindings: [],
      limits,
      toolCallId: "call-1",
      suspend: (request: unknown) => {
        suspended.push(request as Record<string, unknown>);
      },
    },
    { command: options.command ?? "bun test" },
  );
  return { executed: state.executed, suspended, result };
}

/** A policy keyed by the exact command, like the coding pack's, minus the workspace. */
const byCommand: StandingApprovalPolicy = {
  async keyFor(ctx) {
    const command = (ctx.arguments as { command?: string }).command;
    if (command === "bun test") return STANDING;
    if (command === "tsc") return { digest: OTHER_DIGEST, label: "tsc" };
    return null;
  },
};

async function chatWith(
  grants: unknown,
): Promise<{ store: AssistantStore; chatId: string }> {
  const harness = createHarness();
  const chat = await harness.store.conversations.createChat({
    id: "s1",
    metadata:
      grants === undefined ? {} : { [STANDING_APPROVALS_METADATA_KEY]: grants },
  });
  return { store: harness.store, chatId: chat.id };
}

function grant(digest: string, revokedAt?: string): StandingApprovalGrant {
  return {
    grantId: `grant:${digest.slice(0, 4)}`,
    digest,
    label: "x",
    toolName: "run_command",
    sourceRequestId: "r",
    answerId: "a",
    grantedAt: "2026-01-01T00:00:00.000Z",
    ...(revokedAt === undefined ? {} : { revokedAt }),
  };
}

describe("standing approval reuse", () => {
  it("runs a call whose exact digest has an active grant, and only that one", async () => {
    const { store } = await chatWith({ version: 1, grants: [grant(DIGEST)] });
    const hit = await callOnce({ store, policy: byCommand });
    expect(hit.executed).toBe(1);
    expect(hit.suspended).toEqual([]);
    const miss = await callOnce({ store, policy: byCommand, command: "tsc" });
    expect(miss.executed).toBe(0);
    expect(miss.suspended[0]?.["standing"]).toEqual({
      digest: OTHER_DIGEST,
      label: "tsc",
    });
  });

  it("asks again once the grant is revoked", async () => {
    const { store } = await chatWith({
      version: 1,
      grants: [grant(DIGEST, "2026-01-02T00:00:00.000Z")],
    });
    const run = await callOnce({ store, policy: byCommand });
    expect(run.executed).toBe(0);
    expect(run.suspended).toHaveLength(1);
    expect(run.suspended[0]?.["standing"]).toEqual(STANDING);
  });

  it("offers no standing key when the policy says null, and none when unconfigured", async () => {
    const { store } = await chatWith({ version: 1, grants: [grant(DIGEST)] });
    const never = await callOnce({ store, policy: byCommand, command: "ls" });
    expect(never.suspended[0]).not.toHaveProperty("standing");
    const unconfigured = await callOnce({ store });
    expect(unconfigured.executed).toBe(0);
    expect(unconfigured.suspended[0]).not.toHaveProperty("standing");
  });

  it("a final refusal still wins over a matching grant", async () => {
    const { store } = await chatWith({ version: 1, grants: [grant(DIGEST)] });
    const run = await callOnce({
      store,
      policy: byCommand,
      extraGuards: [
        { canExecute: () => ({ allowed: false, reason: "role_ceiling: no" }) },
      ],
    });
    expect(run.executed).toBe(0);
    expect(run.suspended).toEqual([]);
    expect(run.result.ok).toBe(false);
  });

  it("fails closed when the policy throws or answers a malformed key", async () => {
    const { store } = await chatWith({ version: 1, grants: [grant(DIGEST)] });
    const throwing: StandingApprovalPolicy = {
      keyFor: async () => {
        throw new Error("boom");
      },
    };
    const bad: StandingApprovalPolicy = {
      keyFor: async () => ({ digest: "NOT-HEX", label: "x" }),
    };
    for (const policy of [throwing, bad]) {
      const run = await callOnce({ store, policy });
      expect(run.executed).toBe(0);
      expect(run.suspended).toHaveLength(1);
      expect(run.suspended[0]).not.toHaveProperty("standing");
    }
  });

  it("treats unreadable grants as no grants", async () => {
    for (const grants of [
      { version: 1, grants: [{ ...grant(DIGEST), digest: 5 }] },
      { version: 2, grants: [grant(DIGEST)] },
      "junk",
    ]) {
      const { store } = await chatWith(grants);
      const run = await callOnce({ store, policy: byCommand });
      expect(run.executed).toBe(0);
      expect(run.suspended).toHaveLength(1);
    }
  });

  it("does not match another chat's grants", async () => {
    const { store } = await chatWith({ version: 1, grants: [grant(DIGEST)] });
    const run = await callOnce({
      store,
      policy: byCommand,
      chatId: "other-chat",
    });
    expect(run.executed).toBe(0);
  });
});

// --- restart on the sqlite adapter ---------------------------------------------

describe("durability", () => {
  const dirs: string[] = [];
  afterEach(() => {
    for (const dir of dirs.splice(0))
      rmSync(dir, { recursive: true, force: true });
  });

  it("a grant survives a new store instance and still matches", async () => {
    const dir = mkdtempSync(join(tmpdir(), "agentkit-standing-"));
    dirs.push(dir);
    const path = join(dir, "state.sqlite");
    const clock = createTestClock();
    let store = new SqliteAssistantStore(path, { clock });
    try {
      const f = serviceOver(store, clock);
      const s = await session(f);
      const { requestId } = await park(f, s, { standing: STANDING });
      await f.service.approve(requestId, "ans-1", undefined, {
        remember: true,
      });
      store.close();
      store = new SqliteAssistantStore(path, { clock });
      const reopened = serviceOver(store, clock);
      const grants = await reopened.service.listApprovalGrants(s);
      expect(grants.map((g) => g.digest)).toEqual([DIGEST]);
      expect(
        readStandingApprovals((await store.conversations.getChat(s))?.metadata),
      ).toHaveLength(1);
      const hit = await callOnce({ store, policy: byCommand });
      expect(hit.executed).toBe(1);
      await reopened.service.revokeApprovalGrant(s, `grant:${requestId}`);
      const after = await callOnce({ store, policy: byCommand });
      expect(after.executed).toBe(0);
    } finally {
      store.close();
    }
  });
});
