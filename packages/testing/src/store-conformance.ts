// A shared behavioral contract every AssistantStore implementation must pass
// — memory-backed, sqlite-backed, or anything else a host writes later.
//
// FRAMEWORK-NEUTRAL BY DESIGN: this file must not import "bun:test" (or any
// other test runner). @agentkit/testing's build tsconfig sets `types: []`
// specifically so nothing here can quietly depend on one runner's ambient
// globals, and the package is meant to be usable from whatever test runner a
// host already has. The test primitives — `describe`, `it`, `expect`, and the
// optional `beforeEach` — are INJECTED by the caller instead.
//
// Likewise every @agentkit/host import below is `import type`: this package
// has no runtime dependency on host (a peer dependency only, mirroring the
// existing @agentkit/core rule — see stamp.ts's module doc). Error assertions
// therefore match on the `code` string every AgentKitHostError carries
// (see packages/host/src/errors.ts) rather than on `instanceof` against an
// imported class — the same code-not-message discipline host's own error
// vocabulary asks every OTHER consumer to follow.
import type {
  AssistantStore,
  CreateProposalInput,
  CreateTaskInput,
} from "@agentkit/host";
import { createTestEventStamper } from "./stamp.js";

/**
 * What `create()` hands back for one test: a fresh, isolated store plus
 * whatever this adapter cannot promise.
 */
export interface AssistantStoreConformanceHarness {
  store: AssistantStore;
  /**
   * Capabilities this adapter does NOT provide. Absent/undefined means "fully
   * capable" — only `atomicTransactions: false` currently changes suite
   * behavior (see {@link DescribeAssistantStoreConformanceOptions}).
   */
  capabilities?: {
    /** False for an adapter whose `transaction()` cannot roll back (e.g. a plain in-memory store). */
    atomicTransactions?: boolean;
  };
  /** Releases whatever `create()` opened (a db connection, a temp file handle, ...). Synchronous — mirrors `SqliteAssistantStore.close()`. */
  close?: () => void;
}

/**
 * Minimal test-runner primitives this suite needs, injected by the caller so
 * this package never imports a specific runner.
 *
 * `expect` is typed loosely (`any` return) on purpose: every assertion below
 * uses only the near-universal Jest-style subset (`toBe`, `toEqual`,
 * `toBeNull`, `toBeDefined`, `toContain`, `toBeGreaterThan`, `.not`), and
 * giving that chain a precise type would mean importing one runner's matcher
 * types — exactly what this file exists to avoid.
 */
export interface AssistantStoreConformanceTestApi {
  describe: (name: string, fn: () => void) => void;
  it: (name: string, fn: () => void | Promise<void>) => void;
  expect: (value: unknown) => any;
  beforeEach?: (fn: () => void | Promise<void>) => void;
}

export interface DescribeAssistantStoreConformanceOptions {
  /** Adapter name, folded into every `describe` block title. */
  name: string;
  /** Builds one fresh, isolated store per test — never shared across `it()`s. */
  create: () => Promise<AssistantStoreConformanceHarness>;
  test: AssistantStoreConformanceTestApi;
}

/** Catches a rejection and asserts its `code` field — see the module doc on why `code`, not `instanceof`. */
async function expectRejectsWithCode(
  promise: Promise<unknown>,
  expectedCode: string,
  expect: AssistantStoreConformanceTestApi["expect"],
): Promise<void> {
  let caught: { code?: string } | undefined;
  try {
    await promise;
  } catch (err) {
    caught = err as { code?: string };
  }
  expect(caught).toBeDefined();
  expect(caught?.code).toBe(expectedCode);
}

/** Catches a rejection for any reason — used for the plain-Error atomicity probe. */
async function expectRejects(
  promise: Promise<unknown>,
  expect: AssistantStoreConformanceTestApi["expect"],
): Promise<void> {
  let threw = false;
  try {
    await promise;
  } catch {
    threw = true;
  }
  expect(threw).toBe(true);
}

/**
 * The full store-conformance suite. Call once per adapter with a fresh
 * `create()` factory; every `it()` below builds its own store via `create()`
 * and closes it when done, so tests never share state even without
 * `afterEach`.
 */
export function describeAssistantStoreConformance(
  options: DescribeAssistantStoreConformanceOptions,
): void {
  const { name, create, test } = options;
  const { describe, it, expect } = test;

  let counter = 0;
  const uniqueId = (prefix: string): string => {
    counter += 1;
    return `${prefix}-${counter}`;
  };

  const makeTaskInput = (
    overrides: Partial<CreateTaskInput> = {},
  ): CreateTaskInput => ({
    taskId: uniqueId("task"),
    kind: "test.kind",
    scopeId: uniqueId("scope"),
    payload: { model: "test-model" },
    ...overrides,
  });

  const makeProposalInput = (
    overrides: Partial<CreateProposalInput> = {},
  ): CreateProposalInput => ({
    id: uniqueId("prp"),
    chatId: "chat-1",
    scopeKey: "scope-1",
    toolName: "test.tool",
    kind: "test.kind",
    risk: "low",
    envelope: {},
    operations: [],
    warnings: [],
    truncated: false,
    createdAt: new Date().toISOString(),
    ...overrides,
  });

  describe(`AssistantStore conformance — ${name}`, () => {
    it("creates chats and appends messages with per-chat monotonic orderKey", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({ title: "Test chat" });
        expect(chat.title).toBe("Test chat");
        const other = await store.conversations.createChat({});
        const m1 = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "user",
          content: "hi",
        });
        const m2 = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "hello",
        });
        const o1 = await store.conversations.appendMessage({
          chatId: other.id,
          role: "user",
          content: "separate chat",
        });
        expect(m1.orderKey).toBe(1);
        expect(m2.orderKey).toBe(2);
        expect(o1.orderKey).toBe(1); // independent per-chat sequence
        const fetched = await store.conversations.getChat(chat.id);
        expect(fetched?.id).toBe(chat.id);
        const listed = await store.conversations.listMessages(chat.id);
        expect(listed.map((m) => m.id)).toEqual([m1.id, m2.id]);
      } finally {
        close?.();
      }
    });

    it("updates a message's content and REPLACES its metadata", async () => {
      const { store, close } = await create();
      try {
        const chat = await store.conversations.createChat({});
        const message = await store.conversations.appendMessage({
          chatId: chat.id,
          role: "assistant",
          content: "draft",
          metadata: { streaming: true, keep: "no" },
        });
        const updated = await store.conversations.updateMessage(message.id, {
          content: "final",
          metadata: { streaming: false },
        });
        expect(updated.content).toBe("final");
        expect(updated.metadata).toEqual({ streaming: false });
      } finally {
        close?.();
      }
    });

    it("creates a task, allows a legal transition, and rejects an illegal one", async () => {
      const { store, close } = await create();
      try {
        const task = await store.tasks.createTask(makeTaskInput());
        expect(task.status).toBe("queued");
        const running = await store.tasks.transitionTask(task.taskId, ["queued"], "running");
        expect(running.status).toBe("running");
        // "queued" -> "completed" is not a legal edge from TASK_TRANSITIONS.
        await expectRejectsWithCode(
          store.tasks.transitionTask(task.taskId, ["queued"], "completed"),
          "invalid_task_transition",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("round-trips the task kind through create, read, and claim", async () => {
      const { store, close } = await create();
      try {
        const created = await store.tasks.createTask(
          makeTaskInput({ kind: "index.rebuild" }),
        );
        expect(created.kind).toBe("index.rebuild");
        expect((await store.tasks.getTask(created.taskId))?.kind).toBe(
          "index.rebuild",
        );
        // The claim path must carry it too: the dispatcher reads `kind` off the
        // claimed record to pick an executor, and a store that dropped it there
        // would route every claimed task to nobody.
        const claimed = await store.tasks.claimNext({
          ownerId: "worker-1",
          now: new Date(),
          scopesBusy: [],
        });
        expect(claimed?.task.kind).toBe("index.rebuild");
      } finally {
        close?.();
      }
    });

    it("rejects a duplicate taskId instead of overwriting the existing task", async () => {
      const { store, close } = await create();
      try {
        const taskId = uniqueId("task");
        await store.tasks.createTask(makeTaskInput({ taskId }));
        await expectRejectsWithCode(
          store.tasks.createTask(
            makeTaskInput({ taskId, payload: { model: "other" } }),
          ),
          "duplicate_task",
          expect,
        );
        // …and the FIRST task is intact: a silent overwrite would have replaced
        // the payload a live task is executing against.
        const stored = await store.tasks.getTask(taskId);
        expect(stored?.payload).toEqual({ model: "test-model" });
      } finally {
        close?.();
      }
    });

    it("claimNext honours the kinds filter, and claims anything without one", async () => {
      const { store, close } = await create();
      try {
        // `a` is stacked to win EVERY tie-break the ordering knows about: it is
        // created first (FIFO) and carries the higher priority. So a claim that
        // returns `b` can only have got there by filtering on kind — an
        // implementation that ignored `kinds` entirely would hand back `a`, and
        // this test would fail rather than pass by accident.
        // Different scopes, so neither task can be excluded by scope
        // serialization either.
        const a = await store.tasks.createTask(
          makeTaskInput({
            kind: "kind.A",
            scopeId: uniqueId("scope"),
            priority: 10,
          }),
        );
        const b = await store.tasks.createTask(
          makeTaskInput({
            kind: "kind.B",
            scopeId: uniqueId("scope"),
            priority: 0,
          }),
        );
        const filtered = await store.tasks.claimNext({
          ownerId: "worker-B",
          now: new Date(),
          scopesBusy: [],
          kinds: ["kind.B"],
        });
        expect(filtered?.task.taskId).toBe(b.taskId);

        // A kind nobody enqueued claims nothing, even though `a` is sitting
        // right there, queued and available.
        expect(
          await store.tasks.claimNext({
            ownerId: "worker-Z",
            now: new Date(),
            scopesBusy: [],
            kinds: ["kind.Z"],
          }),
        ).toBeNull();
        // And an EMPTY list means "no kind is acceptable", not "any kind" —
        // the difference between a worker pool registered for nothing idling
        // and it eating everyone else's work.
        expect(
          await store.tasks.claimNext({
            ownerId: "worker-none",
            now: new Date(),
            scopesBusy: [],
            kinds: [],
          }),
        ).toBeNull();

        // Unfiltered, the remaining task is claimable by whoever asks.
        const unfiltered = await store.tasks.claimNext({
          ownerId: "worker-any",
          now: new Date(),
          scopesBusy: [],
        });
        expect(unfiltered?.task.taskId).toBe(a.taskId);
      } finally {
        close?.();
      }
    });

    it("creates and ends an attempt", async () => {
      const { store, close } = await create();
      try {
        const task = await store.tasks.createTask(makeTaskInput());
        const attempt = await store.tasks.createAttempt({
          attemptId: uniqueId("att"),
          taskId: task.taskId,
          ownerId: "worker-1",
        });
        expect(attempt.status).toBe("running");
        expect(attempt.attemptNumber).toBe(1);
        const ended = await store.tasks.endAttempt({
          attemptId: attempt.attemptId,
          status: "completed",
        });
        expect(ended.status).toBe("completed");
        expect(ended.endedAt).toBeDefined();
      } finally {
        close?.();
      }
    });

    it("acquires, renews, and releases a lease", async () => {
      const { store, close } = await create();
      try {
        const task = await store.tasks.createTask(makeTaskInput());
        const attempt = await store.tasks.createAttempt({
          attemptId: uniqueId("att"),
          taskId: task.taskId,
          ownerId: "worker-1",
        });
        const lease = await store.tasks.acquireLease({
          taskId: task.taskId,
          attemptId: attempt.attemptId,
          ownerId: "worker-1",
          ttlMs: 60_000,
        });
        expect(lease.taskId).toBe(task.taskId);
        expect(lease.fencingToken).toBeGreaterThan(0);
        const renewed = await store.tasks.renewLease(lease.leaseToken, 120_000);
        expect(renewed.leaseToken).toBe(lease.leaseToken);
        expect(new Date(renewed.expiresAt).getTime()).toBeGreaterThan(
          new Date(lease.expiresAt).getTime(),
        );
        await store.tasks.releaseLease(lease.leaseToken);
      } finally {
        close?.();
      }
    });

    it("rejects renewLease with a stale or unknown token", async () => {
      const { store, close } = await create();
      try {
        await expectRejectsWithCode(
          store.tasks.renewLease("not-a-real-token", 30_000),
          "lease_lost",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("issues a strictly higher fencingToken when re-acquiring after expiry", async () => {
      const { store, close } = await create();
      try {
        const task = await store.tasks.createTask(makeTaskInput());
        const attempt = await store.tasks.createAttempt({
          attemptId: uniqueId("att"),
          taskId: task.taskId,
          ownerId: "worker-1",
        });
        const first = await store.tasks.acquireLease({
          taskId: task.taskId,
          attemptId: attempt.attemptId,
          ownerId: "worker-1",
          ttlMs: 1,
        });
        // Expire it deterministically via an explicit future "now" — the
        // caller controls `now` for expireStaleLeases, so no real sleeping
        // is needed to simulate expiry.
        const future = new Date(Date.now() + 60_000);
        const expired = await store.tasks.expireStaleLeases(future);
        expect(expired.some((l) => l.leaseToken === first.leaseToken)).toBe(true);
        const second = await store.tasks.acquireLease({
          taskId: task.taskId,
          attemptId: attempt.attemptId,
          ownerId: "worker-2",
          ttlMs: 30_000,
        });
        expect(second.fencingToken).toBeGreaterThan(first.fencingToken);
      } finally {
        close?.();
      }
    });

    it("rejects appendEvents when the leaseToken is not current", async () => {
      const { store, close } = await create();
      try {
        const task = await store.tasks.createTask(makeTaskInput());
        const stamp = createTestEventStamper();
        const event = stamp({
          type: "run.started",
          runId: task.taskId,
          timestamp: new Date().toISOString(),
          data: { model: "m", toolCount: 0 },
        });
        await expectRejectsWithCode(
          store.tasks.appendEvents(task.taskId, [event], { leaseToken: "bogus" }),
          "lease_lost",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("rejects a non-monotonic seq in appendEvents", async () => {
      const { store, close } = await create();
      try {
        const task = await store.tasks.createTask(makeTaskInput());
        const attempt = await store.tasks.createAttempt({
          attemptId: uniqueId("att"),
          taskId: task.taskId,
          ownerId: "worker-1",
        });
        const lease = await store.tasks.acquireLease({
          taskId: task.taskId,
          attemptId: attempt.attemptId,
          ownerId: "worker-1",
          ttlMs: 60_000,
        });
        const stamp = createTestEventStamper();
        const e0 = stamp({
          type: "run.started",
          runId: task.taskId,
          timestamp: new Date().toISOString(),
          data: { model: "m", toolCount: 0 },
        });
        await store.tasks.appendEvents(task.taskId, [e0], {
          leaseToken: lease.leaseToken,
        });
        const stampAgain = createTestEventStamper({ firstSeq: 0 });
        const regressed = stampAgain({
          type: "run.completed",
          runId: task.taskId,
          timestamp: new Date().toISOString(),
          data: { iterations: 1 },
        });
        await expectRejectsWithCode(
          store.tasks.appendEvents(task.taskId, [regressed], {
            leaseToken: lease.leaseToken,
          }),
          "seq_conflict",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("lists events after a seq and reports nextSeq", async () => {
      const { store, close } = await create();
      try {
        const task = await store.tasks.createTask(makeTaskInput());
        const attempt = await store.tasks.createAttempt({
          attemptId: uniqueId("att"),
          taskId: task.taskId,
          ownerId: "worker-1",
        });
        const lease = await store.tasks.acquireLease({
          taskId: task.taskId,
          attemptId: attempt.attemptId,
          ownerId: "worker-1",
          ttlMs: 60_000,
        });
        expect(await store.tasks.nextSeq(task.taskId)).toBe(0);
        const stamp = createTestEventStamper();
        const events = [
          stamp({
            type: "run.started",
            runId: task.taskId,
            timestamp: new Date().toISOString(),
            data: { model: "m", toolCount: 0 },
          }),
          stamp({
            type: "run.message.delta",
            runId: task.taskId,
            timestamp: new Date().toISOString(),
            data: { delta: "hi" },
          }),
          stamp({
            type: "run.completed",
            runId: task.taskId,
            timestamp: new Date().toISOString(),
            data: { iterations: 1 },
          }),
        ];
        await store.tasks.appendEvents(task.taskId, events, {
          leaseToken: lease.leaseToken,
        });
        expect(await store.tasks.nextSeq(task.taskId)).toBe(3);
        const after = await store.tasks.listEvents(task.taskId, { afterSeq: 0 });
        expect(after.map((e) => e.seq)).toEqual([1, 2]);
        const all = await store.tasks.listEvents(task.taskId);
        expect(all.map((e) => e.type)).toEqual([
          "run.started",
          "run.message.delta",
          "run.completed",
        ]);
      } finally {
        close?.();
      }
    });

    it("claimNext returns null when there is nothing queued", async () => {
      const { store, close } = await create();
      try {
        const claimed = await store.tasks.claimNext({
          ownerId: "worker-1",
          now: new Date(),
          scopesBusy: [],
        });
        expect(claimed).toBeNull();
      } finally {
        close?.();
      }
    });

    it("claimNext does not claim a task whose availableAt is in the future", async () => {
      const { store, close } = await create();
      try {
        const future = new Date(Date.now() + 3_600_000).toISOString();
        await store.tasks.createTask(makeTaskInput({ availableAt: future }));
        const claimed = await store.tasks.claimNext({
          ownerId: "worker-1",
          now: new Date(),
          scopesBusy: [],
        });
        expect(claimed).toBeNull();
      } finally {
        close?.();
      }
    });

    it("claimNext skips scopes already marked busy", async () => {
      const { store, close } = await create();
      try {
        const scope = uniqueId("scope");
        const task = await store.tasks.createTask(makeTaskInput({ scopeId: scope }));
        const claimed = await store.tasks.claimNext({
          ownerId: "worker-1",
          now: new Date(),
          scopesBusy: [scope],
        });
        expect(claimed).toBeNull();
        const claimedAfter = await store.tasks.claimNext({
          ownerId: "worker-1",
          now: new Date(),
          scopesBusy: [],
        });
        expect(claimedAfter?.task.taskId).toBe(task.taskId);
      } finally {
        close?.();
      }
    });

    it("claimNext prefers the higher-priority task", async () => {
      const { store, close } = await create();
      try {
        await store.tasks.createTask(makeTaskInput({ priority: 0 }));
        const high = await store.tasks.createTask(makeTaskInput({ priority: 10 }));
        // Captured AFTER both creates: an adapter stamps enqueuedAt/availableAt
        // from its own clock at createTask time, so a `now` captured any
        // earlier can race that stamp and spuriously exclude a just-created
        // row from claimNext's `availableAt <= now` filter.
        const claimed = await store.tasks.claimNext({
          ownerId: "worker-1",
          now: new Date(),
          scopesBusy: [],
        });
        expect(claimed?.task.taskId).toBe(high.taskId);
      } finally {
        close?.();
      }
    });

    it("claimNext breaks a priority tie in FIFO (enqueuedAt) order", async () => {
      const { store, close } = await create();
      try {
        const first = await store.tasks.createTask(makeTaskInput());
        await store.tasks.createTask(makeTaskInput());
        const claimed = await store.tasks.claimNext({
          ownerId: "worker-1",
          now: new Date(),
          scopesBusy: [],
        });
        expect(claimed?.task.taskId).toBe(first.taskId);
      } finally {
        close?.();
      }
    });

    it("claimNext claims atomically — a second claim with the same scope busy sees nothing", async () => {
      const { store, close } = await create();
      try {
        const scope = uniqueId("scope");
        const task = await store.tasks.createTask(makeTaskInput({ scopeId: scope }));
        const claimed = await store.tasks.claimNext({
          ownerId: "worker-1",
          now: new Date(),
          scopesBusy: [],
        });
        expect(claimed?.task.taskId).toBe(task.taskId);
        expect(claimed?.task.status).toBe("running");
        expect(claimed?.attempt.ownerId).toBe("worker-1");
        expect(claimed?.lease.taskId).toBe(task.taskId);
        const second = await store.tasks.claimNext({
          ownerId: "worker-2",
          now: new Date(),
          scopesBusy: [scope],
        });
        expect(second).toBeNull();
      } finally {
        close?.();
      }
    });

    it("creates a proposal and rejects a duplicate live actionId in the same scope", async () => {
      const { store, close } = await create();
      try {
        const scope = uniqueId("scope");
        const first = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, actionId: "act-1" }),
        );
        expect(first.status).toBe("pending");
        await expectRejectsWithCode(
          store.proposals.create(
            makeProposalInput({ scopeKey: scope, actionId: "act-1" }),
          ),
          "duplicate_action_id",
          expect,
        );
      } finally {
        close?.();
      }
    });

    it("allows a fresh actionId reuse after the earlier proposal was rejected or invalidated", async () => {
      const { store, close } = await create();
      try {
        const scope = uniqueId("scope");
        const first = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, actionId: "act-rej" }),
        );
        await store.proposals.transition(first.id, ["pending"], "rejected");
        const second = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, actionId: "act-rej" }),
        );
        expect(second.status).toBe("pending");

        const third = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, actionId: "act-inv", revisionAtCreate: "rev-1" }),
        );
        expect(third.status).toBe("pending");
        await store.proposals.invalidatePendingForRevision(scope, "rev-2");
        const fourth = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, actionId: "act-inv" }),
        );
        expect(fourth.status).toBe("pending");
      } finally {
        close?.();
      }
    });

    it("getByActionId returns the most recent proposal for a (scopeKey, actionId) pair", async () => {
      const { store, close } = await create();
      try {
        const scope = uniqueId("scope");
        const first = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, actionId: "act-recent" }),
        );
        await store.proposals.transition(first.id, ["pending"], "rejected");
        const second = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, actionId: "act-recent" }),
        );
        const found = await store.proposals.getByActionId(scope, "act-recent");
        expect(found?.id).toBe(second.id);
      } finally {
        close?.();
      }
    });

    it("recordOutcome is idempotent per operationId, and getOutcome reads it back", async () => {
      const { store, close } = await create();
      try {
        const operationId = uniqueId("op");
        const first = await store.proposals.recordOutcome(operationId, {
          status: "applied",
          appliedOps: 2,
          failedOps: [],
        });
        const second = await store.proposals.recordOutcome(operationId, {
          status: "failed",
          appliedOps: 0,
          failedOps: [{ opIndex: 0, error: "boom" }],
        });
        expect(second).toEqual(first); // first write wins
        const fetched = await store.proposals.getOutcome(operationId);
        expect(fetched).toEqual(first);
        expect(await store.proposals.getOutcome(uniqueId("never-recorded"))).toBeNull();
      } finally {
        close?.();
      }
    });

    it("listByStatus finds proposals across chats by status", async () => {
      const { store, close } = await create();
      try {
        const a = await store.proposals.create(
          makeProposalInput({ chatId: "chat-a", scopeKey: uniqueId("scope") }),
        );
        const b = await store.proposals.create(
          makeProposalInput({ chatId: "chat-b", scopeKey: uniqueId("scope") }),
        );
        await store.proposals.transition(b.id, ["pending"], "rejected");
        const pending = await store.proposals.listByStatus("pending");
        expect(pending.map((p) => p.id)).toContain(a.id);
        expect(pending.map((p) => p.id)).not.toContain(b.id);
        const rejected = await store.proposals.listByStatus("rejected");
        expect(rejected.map((p) => p.id)).toContain(b.id);
      } finally {
        close?.();
      }
    });

    it("invalidatePendingForRevision invalidates stale pending proposals with reason revision_conflict", async () => {
      const { store, close } = await create();
      try {
        const scope = uniqueId("scope");
        const stale = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, revisionAtCreate: "rev-1" }),
        );
        const fresh = await store.proposals.create(
          makeProposalInput({ scopeKey: scope, revisionAtCreate: "rev-2" }),
        );
        const count = await store.proposals.invalidatePendingForRevision(scope, "rev-2");
        expect(count).toBe(1);
        const staleAfter = await store.proposals.get(stale.id);
        expect(staleAfter?.status).toBe("invalidated");
        expect(staleAfter?.reason).toBe("revision_conflict");
        const freshAfter = await store.proposals.get(fresh.id);
        expect(freshAfter?.status).toBe("pending");
      } finally {
        close?.();
      }
    });

    it("upserts and lists providers, plus their models and capabilities", async () => {
      const { store, close } = await create();
      try {
        const providerId = uniqueId("provider");
        const config = {
          id: providerId,
          label: "Test Provider",
          kind: "openai-compatible",
          baseUrl: "https://example.test",
          defaultModel: "test-model",
          enabled: true,
        };
        await store.providers.upsertProvider(config);
        const fetched = await store.providers.getProvider(providerId);
        expect(fetched?.label).toBe("Test Provider");
        await store.providers.upsertProvider({ ...config, label: "Renamed" });
        expect((await store.providers.getProvider(providerId))?.label).toBe(
          "Renamed",
        );
        expect((await store.providers.listProviders()).map((p) => p.id)).toContain(
          providerId,
        );

        await store.providers.replaceModels(providerId, [
          {
            providerId,
            modelId: "model-a",
            displayName: "Model A",
            fetchedAt: new Date().toISOString(),
          },
        ]);
        expect(
          (await store.providers.listModels(providerId)).map((m) => m.modelId),
        ).toEqual(["model-a"]);

        await store.providers.saveCapabilities(providerId, {
          streaming: true,
          toolCalling: false,
          modelList: true,
        });
        const caps = await store.providers.getCapabilities(providerId);
        expect(caps?.streaming).toBe(true);
        expect(caps?.toolCalling).toBe(false);
        expect(await store.providers.getCapabilities(uniqueId("no-such"))).toBeNull();
      } finally {
        close?.();
      }
    });

    it("reads default settings and applies a partial update", async () => {
      const { store, close } = await create();
      try {
        const initial = await store.settings.getSettings();
        expect(initial.writePolicyMode).toBeDefined();
        const updated = await store.settings.updateSettings({
          defaultModel: "gpt-test",
          allowRawToolData: true,
        });
        expect(updated.defaultModel).toBe("gpt-test");
        expect(updated.allowRawToolData).toBe(true);
        // Untouched fields survive the partial update.
        expect(updated.contextSizePreference).toBe(initial.contextSizePreference);
        const fetched = await store.settings.getSettings();
        expect(fetched.defaultModel).toBe("gpt-test");
      } finally {
        close?.();
      }
    });

    it("enqueues, claims, and publishes an outbox record without re-claiming it while in flight", async () => {
      const { store, close } = await create();
      try {
        const enqueued = await store.outbox.enqueue({
          topic: "run.events",
          payload: { hello: "world" },
        });
        const now = new Date();
        const claimed = await store.outbox.claimBatch({
          limit: 10,
          now,
          ownerId: "publisher-1",
        });
        expect(claimed.map((r) => r.id)).toEqual([enqueued.id]);
        expect(claimed[0]?.attempts).toBe(1);
        // Still pending (unpublished) — an immediate second claim must NOT
        // hand it out again while it is in flight.
        const reclaimed = await store.outbox.claimBatch({
          limit: 10,
          now,
          ownerId: "publisher-2",
        });
        expect(reclaimed.map((r) => r.id)).not.toContain(enqueued.id);
        await store.outbox.markPublished(enqueued.id, new Date());
        const afterPublish = await store.outbox.claimBatch({
          limit: 10,
          now: new Date(now.getTime() + 120_000),
          ownerId: "publisher-3",
        });
        expect(afterPublish.map((r) => r.id)).not.toContain(enqueued.id);
      } finally {
        close?.();
      }
    });

    it("markFailed schedules a retry via availableAt", async () => {
      const { store, close } = await create();
      try {
        const enqueued = await store.outbox.enqueue({ topic: "run.events", payload: {} });
        const now = new Date();
        await store.outbox.claimBatch({ limit: 10, now, ownerId: "publisher-1" });
        const retryAt = new Date(now.getTime() + 5_000);
        await store.outbox.markFailed(enqueued.id, "boom", retryAt);
        const tooSoon = await store.outbox.claimBatch({
          limit: 10,
          now,
          ownerId: "publisher-2",
        });
        expect(tooSoon.map((r) => r.id)).not.toContain(enqueued.id);
        const afterRetry = await store.outbox.claimBatch({
          limit: 10,
          now: new Date(retryAt.getTime() + 1),
          ownerId: "publisher-2",
        });
        expect(afterRetry.map((r) => r.id)).toContain(enqueued.id);
      } finally {
        close?.();
      }
    });

    it("transaction() rolls back every write when fn throws", async () => {
      const { store, capabilities, close } = await create();
      try {
        // Adapters that document no rollback (e.g. a plain in-memory store)
        // opt out via capabilities.atomicTransactions === false.
        if (capabilities?.atomicTransactions === false) return;
        const chatId = uniqueId("chat");
        await expectRejects(
          store.transaction(async (tx) => {
            await tx.conversations.createChat({ id: chatId });
            throw new Error("boom");
          }),
          expect,
        );
        const found = await store.conversations.getChat(chatId);
        expect(found).toBeNull();
      } finally {
        close?.();
      }
    });
  });
}
