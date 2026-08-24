/**
 * Two `SqliteAssistantStore` instances over ONE database file — the topology
 * every other test in this directory avoids.
 *
 * Everything else here opens `:memory:` (which cannot be shared: a `:memory:`
 * database belongs to the connection that opened it, so two instances would be
 * two unrelated stores and every "concurrency" assertion would be vacuous) or a
 * single file handle. That leaves the interesting question untested: the
 * per-instance mutex `SqliteTaskStore.claimNext` uses to serialize itself is a
 * JavaScript promise chain, and a second CONNECTION has its own. Nothing in
 * process A's memory can hold process B back. If two handles are safe together,
 * SQLite's own `BEGIN IMMEDIATE` is what makes them safe.
 *
 * ── WHAT THIS FILE FOUND ──────────────────────────────────────────────────
 *
 * `BEGIN IMMEDIATE` alone is NOT enough as the adapter uses it, because the
 * adapter never sets a busy timeout. SQLite's default busy handler returns
 * immediately, so the second connection's `BEGIN IMMEDIATE` fails at once with
 * `SQLITE_BUSY` ("database is locked") and the raw driver error escapes
 * `claimNext`. Two tests below are skipped rather than deleted because they
 * describe what the port promises and the adapter does not yet deliver; the
 * accompanying report has the detail. What DOES hold — and is asserted here —
 * is that the failure is clean: the losing claim writes nothing, and the
 * surviving one is whole.
 *
 * So the passing coverage is the SERIALIZED interleaving: both handles work the
 * same queue, alternately, each getting its transaction to itself. That is not
 * a weaker test of the shared state — the fencing counter, `attempt_count`, the
 * dependency gate and the claim CAS are all cross-handle in it — it is only a
 * weaker test of lock contention, which is the part that does not work yet.
 */
import { describe, expect, it } from "bun:test";
import {
  checkTaskInvariants,
  createRng,
  snapshotTaskInvariants,
  type ObservedLease,
} from "@agentkit/testing";
import {
  CONTRACT_VERSION,
  type TaskEventEnvelope,
} from "@agentkit/contracts";
import type { AssistantStore } from "@agentkit/host";
import { SqliteAssistantStore } from "../src/index.js";
import {
  createSqliteHarness,
  createSqliteScratch,
  dumpSqliteAttempts,
  dumpSqliteLeases,
  type DurabilityHarness,
} from "./support/durability-harness.js";

/**
 * A workload with dependency chains, a fan-in, a lineage branch and a delayed
 * task — the shapes whose gate lives in `claimNext`, so a second handle has to
 * agree with the first about them.
 */
async function seedSharedWorkload(
  harness: DurabilityHarness,
  count: number,
): Promise<string[]> {
  const rng = createRng(0x5eed);
  const ids: string[] = [];
  for (let i = 0; i < count; i += 1) {
    const taskId = `mh${String(i).padStart(2, "0")}`;
    const shape = rng.next();
    await harness.target.submitTask({
      taskId,
      kind: "durability.multi",
      scopeId: `scope-${i}`,
      payload: { index: i },
      priority: rng.int(3),
      ...(ids.length > 0 && shape < 0.25
        ? { dependsOn: [ids[ids.length - 1]!] }
        : {}),
      ...(ids.length > 2 && shape >= 0.25 && shape < 0.4
        ? { dependsOn: [ids[0]!, ids[1]!] }
        : {}),
      ...(ids.length > 0 && shape >= 0.4 && shape < 0.5
        ? { parentTaskId: ids[0]! }
        : {}),
      ...(shape >= 0.5 && shape < 0.58
        ? {
            availableAt: new Date(
              harness.clock.now().getTime() + 500,
            ).toISOString(),
          }
        : {}),
    });
    ids.push(taskId);
  }
  return ids;
}

describe("SqliteAssistantStore — two handles, one file", () => {
  it("cannot be simulated with :memory: — two instances are two databases", async () => {
    // The premise of this whole file. If `:memory:` were shareable, the cheap
    // version of these tests would be the right one; it is not, and asserting
    // so keeps anyone from "simplifying" the temp file away — which would
    // silently turn every cross-handle assertion below into a tautology about
    // two unrelated stores.
    const a = new SqliteAssistantStore(":memory:");
    const b = new SqliteAssistantStore(":memory:");
    try {
      await a.tasks.createTask({
        taskId: "only-in-a",
        kind: "unit",
        scopeId: "s",
        payload: {},
      });
      expect(await a.tasks.getTask("only-in-a")).not.toBeNull();
      expect(await b.tasks.getTask("only-in-a")).toBeNull();

      // And the same two writes through ONE file ARE shared — the property the
      // rest of the file relies on.
      const scratch = createSqliteScratch("shared-probe");
      const fileA = new SqliteAssistantStore(scratch.path);
      const fileB = new SqliteAssistantStore(scratch.path);
      try {
        await fileA.tasks.createTask({
          taskId: "shared",
          kind: "unit",
          scopeId: "s",
          payload: {},
        });
        expect((await fileB.tasks.getTask("shared"))?.taskId).toBe("shared");
      } finally {
        fileA.close();
        fileB.close();
        scratch.cleanup();
      }
    } finally {
      a.close();
      b.close();
    }
  });

  it("drains one shared queue across both handles with no task claimed twice", async () => {
    const harness = createSqliteHarness(2);
    try {
      const handles = harness.target.handles;
      expect(handles.length).toBe(2);
      const taskIds = await seedSharedWorkload(harness, 18);

      const rng = createRng(0xa11ce);
      const observedLeases: ObservedLease[] = [];
      /** taskId → which handle claimed it. A second entry would be a double claim. */
      const claimedBy = new Map<string, number>();
      let leaseCounter = 0;
      let idleRounds = 0;

      // SERIALIZED, and deliberately so — see the module doc. Each unit is
      // awaited to completion before the other handle is touched, so no async
      // transaction is ever open while the other connection writes.
      for (let step = 0; step < 400 && idleRounds < handles.length; step += 1) {
        const index = step % handles.length;
        const store = handles[index] as AssistantStore;

        if (rng.chance(0.05)) {
          await harness.target.cancelTask(rng.pick(taskIds));
          continue;
        }
        // Past every `availableAt`, so a delayed task cannot end the drain
        // early by looking un-claimable.
        harness.clock.advance(200);

        const claim = await store.tasks.claimNext({
          ownerId: `handle-${index}`,
          now: harness.clock.now(),
          scopesBusy: [],
        });
        if (claim === null) {
          idleRounds += 1;
          continue;
        }
        idleRounds = 0;

        // THE HEADLINE ASSERTION. `claimNext` only ever takes a `queued` task
        // and there is no edge back to `queued`, so across BOTH handles a task
        // may be handed out exactly once. A second entry here is the
        // double-execution bug leases exist to prevent.
        expect(claimedBy.has(claim.task.taskId)).toBe(false);
        claimedBy.set(claim.task.taskId, index);
        expect(claim.task.status).toBe("running");
        leaseCounter += 1;
        observedLeases.push({ ...claim.lease, observedAt: leaseCounter });

        const first = await store.tasks.nextSeq(claim.task.taskId);
        const events: TaskEventEnvelope[] = [0, 1].map((offset) => ({
          type: "durability.multi",
          seq: first + offset,
          eventId: harness.ids.eventId(),
          timestamp: harness.clock.nowIso(),
          contractVersion: CONTRACT_VERSION,
          attemptId: claim.attempt.attemptId,
        }));
        await store.tasks.appendEvents(claim.task.taskId, events, {
          leaseToken: claim.lease.leaseToken,
        });

        const cancelled = harness.target.cancelRequested(claim.task.taskId);
        await store.tasks.transitionTask(
          claim.task.taskId,
          ["running"],
          cancelled ? "cancelled" : "completed",
          { finishedAt: harness.clock.nowIso() },
        );
        await store.tasks.endAttempt({
          attemptId: claim.attempt.attemptId,
          status: cancelled ? "cancelled" : "completed",
        });
        await store.tasks.releaseLease(claim.lease.leaseToken);
      }

      // Both handles really did work — a drain that ran entirely on one
      // connection would assert nothing about the topology.
      const byHandle = [...claimedBy.values()];
      expect(byHandle.filter((h) => h === 0).length).toBeGreaterThan(2);
      expect(byHandle.filter((h) => h === 1).length).toBeGreaterThan(2);

      const view = await snapshotTaskInvariants({
        reader: handles[0]!.tasks,
        taskIds,
        observedLeases,
        dumpAttempts: harness.target.dumpAttempts?.bind(harness.target),
        dumpLiveLeases: harness.target.dumpLiveLeases?.bind(harness.target),
      });
      expect(
        checkTaskInvariants(view, {
          phase: "quiescent",
          label: "sqlite two handles",
        }),
      ).toEqual([]);

      // Nothing left half-done: everything is terminal, and every completed
      // task ran exactly once — one attempt, whose id every one of its events
      // carries.
      for (const task of view.tasks) {
        expect(["completed", "failed", "cancelled"]).toContain(task.status);
        if (task.status !== "completed") continue;
        expect(task.attemptCount).toBe(1);
        const attemptIds = new Set(
          (view.events.get(task.taskId) ?? []).map((e) => e.attemptId),
        );
        expect(attemptIds.size).toBe(1);
      }

      // The fencing counter is one row in one table, so tokens are unique
      // across handles — the checker asserts that too, but stating it here is
      // the point of the topology.
      const tokens = observedLeases.map((l) => l.fencingToken);
      expect(new Set(tokens).size).toBe(tokens.length);
    } finally {
      harness.close();
    }
  });

  it("fails cleanly when two handles claim at the same instant", async () => {
    // CHARACTERIZATION of today's behaviour, and the reason the two cases
    // after it are skipped: the second connection's `BEGIN IMMEDIATE` cannot
    // take the write lock and gives up immediately, because the adapter sets no
    // busy timeout. What is asserted is the part that IS sound — the losing
    // claim never got as far as writing, so the queue is untouched by it.
    const scratch = createSqliteScratch("busy");
    const a = new SqliteAssistantStore(scratch.path);
    const b = new SqliteAssistantStore(scratch.path);
    try {
      for (let i = 0; i < 6; i += 1) {
        await a.tasks.createTask({
          taskId: `busy-${i}`,
          kind: "unit",
          scopeId: `scope-${i}`,
          payload: {},
        });
      }
      const now = new Date();
      const settled = await Promise.allSettled([
        a.tasks.claimNext({ ownerId: "handle-a", now, scopesBusy: [] }),
        b.tasks.claimNext({ ownerId: "handle-b", now, scopesBusy: [] }),
      ]);

      const rejected = settled.filter((r) => r.status === "rejected");
      expect(rejected.length).toBe(1);
      const reason = (rejected[0] as PromiseRejectedResult).reason as {
        code?: string;
      };
      expect(reason.code).toBe("SQLITE_BUSY");

      const claims = settled
        .filter((r) => r.status === "fulfilled")
        .map((r) => (r as PromiseFulfilledResult<unknown>).value)
        .filter((v): v is { task: { taskId: string } } => v !== null);
      expect(claims.length).toBe(1);

      // Exactly one task moved. The loser rolled back before writing anything,
      // so five rows are still queued with no attempts and there is one lease.
      const attempts = dumpSqliteAttempts(scratch.path);
      const leases = dumpSqliteLeases(scratch.path);
      expect(attempts.length).toBe(1);
      expect(leases.length).toBe(1);
      expect(attempts[0]!.taskId).toBe(claims[0]!.task.taskId);
      let queued = 0;
      for (let i = 0; i < 6; i += 1) {
        const task = await a.tasks.getTask(`busy-${i}`);
        if (task?.status === "queued") {
          queued += 1;
          expect(task.attemptCount).toBe(0);
        }
      }
      expect(queued).toBe(5);
    } finally {
      a.close();
      b.close();
      scratch.cleanup();
    }
  });

  it.skip("serializes concurrent claims across handles instead of surfacing SQLITE_BUSY", async () => {
    // FINDING — NOT A TEST GAP. `TaskStore.claimNext` documents an atomic
    // claim that "returns null when there is nothing to do"; it does not
    // document a driver error a caller must know to retry. Today the second
    // connection's `BEGIN IMMEDIATE` fails instantly with SQLITE_BUSY and that
    // raw `SQLiteError` escapes, so any host running two processes (or two
    // handles) over one file sees claims fail under ordinary contention.
    //
    // The adapter sets no `PRAGMA busy_timeout`, and `bun:sqlite` defaults to
    // none, so there is no wait-and-retry at the SQLite level. The fix is in
    // the adapter — a busy timeout, or catching SQLITE_BUSY on the claim path
    // and returning null / retrying — which is production source and out of
    // scope for this landing. Un-skip when it lands: the assertion below is
    // what the port already promises.
    const scratch = createSqliteScratch("busy-fixed");
    const a = new SqliteAssistantStore(scratch.path);
    const b = new SqliteAssistantStore(scratch.path);
    try {
      for (let i = 0; i < 6; i += 1) {
        await a.tasks.createTask({
          taskId: `busy-${i}`,
          kind: "unit",
          scopeId: `scope-${i}`,
          payload: {},
        });
      }
      const now = new Date();
      const claims = await Promise.all([
        a.tasks.claimNext({ ownerId: "handle-a", now, scopesBusy: [] }),
        b.tasks.claimNext({ ownerId: "handle-b", now, scopesBusy: [] }),
      ]);
      const ids = claims.filter((c) => c !== null).map((c) => c!.task.taskId);
      expect(ids.length).toBe(2);
      expect(new Set(ids).size).toBe(2);
    } finally {
      a.close();
      b.close();
      scratch.cleanup();
    }
  });

  it.skip("keeps transactions atomic on a handle that has hit SQLITE_BUSY", async () => {
    // FINDING, and the more serious of the two. `SqliteConnection.withTx` /
    // `withAsyncTx` raise `txDepth` and THEN issue `BEGIN IMMEDIATE` outside
    // the try/finally that lowers it again. When the BEGIN itself throws — which
    // is exactly what a cross-handle SQLITE_BUSY does — the counter is never
    // restored, so it sits at 1 for the life of that connection.
    //
    // From then on every `withTx`/`withAsyncTx` on that handle takes the
    // "already in a transaction" branch and runs with NO BEGIN, NO COMMIT and NO
    // ROLLBACK. `AssistantStore.transaction(fn)` silently stops being atomic:
    // a throw inside `fn` leaves its writes committed. The conformance suite's
    // "transaction() rolls back every write when fn throws" passes only because
    // it never touches a handle that has seen a BUSY.
    //
    // Reproduced by the probe this test is written from: after one cross-handle
    // BUSY, `b.transaction(tx => { createTask(...); throw })` leaves the task
    // row behind, while the same call on the untouched handle rolls back.
    // The fix is one line of adapter source (move the BEGIN inside the try, or
    // lower `txDepth` when it throws) and is out of scope for this landing.
    const scratch = createSqliteScratch("txdepth");
    const a = new SqliteAssistantStore(scratch.path);
    const b = new SqliteAssistantStore(scratch.path);
    try {
      await a.tasks.createTask({
        taskId: "seed",
        kind: "unit",
        scopeId: "s",
        payload: {},
      });
      const now = new Date();
      await Promise.allSettled([
        a.tasks.claimNext({ ownerId: "handle-a", now, scopesBusy: [] }),
        b.tasks.claimNext({ ownerId: "handle-b", now, scopesBusy: [] }),
      ]);

      await expect(
        b.transaction(async (tx) => {
          await tx.tasks.createTask({
            taskId: "rolled-back",
            kind: "unit",
            scopeId: "s2",
            payload: {},
          });
          throw new Error("boom");
        }),
      ).rejects.toThrow("boom");
      expect(await b.tasks.getTask("rolled-back")).toBeNull();
    } finally {
      a.close();
      b.close();
      scratch.cleanup();
    }
  });
});
