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
 * ── WHAT THIS FILE FOUND, AND WHAT WAS DONE ABOUT IT ──────────────────────
 *
 * `BEGIN IMMEDIATE` alone was NOT enough as the adapter used it, because the
 * adapter set no busy timeout: SQLite's default handler gives up instantly, so
 * the second connection's BEGIN failed with a raw `SQLITE_BUSY` escaping
 * `claimNext` — a failure the port documents nowhere. Worse, that throw left
 * `SqliteConnection.txDepth` raised for the life of the connection, silently
 * disabling BEGIN/COMMIT/ROLLBACK on every later call. Both are fixed:
 * `withTx`/`withAsyncTx` now raise the counter only once the transaction is
 * really open, the store sets `PRAGMA busy_timeout`, and async transactions
 * wait on the event loop instead of parking the thread. The tests that pinned
 * those findings are un-skipped below and now assert the fixed behaviour.
 *
 * ── WHAT IS STILL NOT SUPPORTED ───────────────────────────────────────────
 *
 * A fully concurrent claim-AND-EXECUTE workload over two handles IN ONE
 * PROCESS. `claimNext` holds its transaction across `await`s, and the other
 * handle's SYNCHRONOUS transactions (`appendEvents`, `transitionTask`, …)
 * cannot wait on the event loop — they can only park the thread, which is the
 * one thread that could let the holder commit. See the skipped case at the
 * bottom for the exact reproduction. Concurrent CLAIMS are fine (both are async
 * and both yield), and so is the cross-PROCESS topology, where the holder runs
 * on its own thread and `busy_timeout` does exactly the right thing — the
 * `Bun.spawn` case covers that one.
 */
import { describe, expect, it } from "bun:test";
import { Database } from "bun:sqlite";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import {
  checkTaskInvariants,
  createRng,
  snapshotTaskInvariants,
  type ObservedLease,
} from "@agentkit/testing";
import { CONTRACT_VERSION, type TaskEventEnvelope } from "@agentkit/contracts";
import type { AssistantStore, ClaimedTask } from "@agentkit/host";
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

  it("hands concurrent cross-handle claims to both callers, with no SQLITE_BUSY", async () => {
    // WAS SKIPPED as a finding: the second connection's `BEGIN IMMEDIATE` used
    // to fail instantly with a raw `SQLITE_BUSY` — and adding SQLite's own
    // busy_timeout made it WORSE, because parking this thread is parking the
    // only thread that could let the other handle commit (measured: a 5.3s
    // stall, then the same error). Async transactions now wait on the event
    // loop instead, so the contention resolves in single-digit milliseconds.
    const scratch = createSqliteScratch("concurrent");
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
      const started = Date.now();
      // Three claims per handle, all in flight at once: enough that several
      // must queue behind someone else's write lock.
      const settled = await Promise.allSettled(
        [a, b, a, b, a, b].map((store, index) =>
          store.tasks.claimNext({
            ownerId: `owner-${index}`,
            now,
            scopesBusy: [],
          }),
        ),
      );
      const elapsed = Date.now() - started;

      const rejections = settled
        .filter((r) => r.status === "rejected")
        .map((r) => String((r as PromiseRejectedResult).reason));
      expect(rejections).toEqual([]);

      const claims = settled
        .map((r) => (r as PromiseFulfilledResult<ClaimedTask | null>).value)
        .filter((claim): claim is ClaimedTask => claim !== null);
      // Six claimable tasks, six claims, no task handed out twice.
      expect(claims.length).toBe(6);
      const claimedIds = claims.map((c) => c.task.taskId);
      expect(new Set(claimedIds).size).toBe(6);
      // The fencing counter is one row shared by both connections.
      expect(new Set(claims.map((c) => c.lease.fencingToken)).size).toBe(6);
      // Waiting on the event loop, not in SQLite: the parking version took the
      // full busy timeout and then failed. A generous ceiling — the point is
      // that no caller sat out a multi-second timeout, not the exact number.
      expect(elapsed).toBeLessThan(1_000);

      const view = await snapshotTaskInvariants({
        reader: a.tasks,
        taskIds: claimedIds,
        observedLeases: claims.map((c, index) => ({
          ...c.lease,
          observedAt: index + 1,
        })),
        dumpAttempts: () => dumpSqliteAttempts(scratch.path),
        dumpLiveLeases: () => dumpSqliteLeases(scratch.path),
        inFlightTaskIds: new Set(claimedIds),
      });
      expect(
        checkTaskInvariants(view, {
          phase: "in-flight",
          label: "concurrent cross-handle claims",
        }),
      ).toEqual([]);
    } finally {
      a.close();
      b.close();
      scratch.cleanup();
    }
  });

  it("keeps transactions atomic on a handle that has hit SQLITE_BUSY", async () => {
    // WAS SKIPPED as the more serious of the two findings.
    // `SqliteConnection.withTx`/`withAsyncTx` raised `txDepth` and THEN issued
    // `BEGIN IMMEDIATE`, outside the try/finally that lowers it. A BEGIN that
    // threw left the counter at 1 forever, so every later call took the
    // "already in a transaction" branch and ran with NO BEGIN, NO COMMIT and NO
    // ROLLBACK: `transaction(fn)` silently stopped being atomic for the life of
    // the connection. The counter is now raised only once the transaction is
    // really open.
    //
    // The BUSY is forced with an OUTSIDE lock holder plus a short timeout,
    // rather than with a second store handle — two handles now wait for each
    // other, which is the other fix, and this test has to be able to produce a
    // failed BEGIN on demand however that one behaves.
    const scratch = createSqliteScratch("txdepth");
    const store = new SqliteAssistantStore(scratch.path, { busyTimeoutMs: 25 });
    const blocker = new Database(scratch.path);
    try {
      blocker.exec("PRAGMA busy_timeout = 0;");
      blocker.exec("BEGIN IMMEDIATE");

      // BOTH transaction helpers get their BEGIN refused, because both used to
      // corrupt the counter and they raise it in different places: `withTx`
      // around a single port method's SQL, `withAsyncTx` around
      // `AssistantStore.transaction`.
      const codes: (string | undefined)[] = [];
      for (const attempt of [
        // Synchronous: one port method's own mini-transaction.
        () =>
          store.tasks.createTask({
            taskId: "never-written-sync",
            kind: "unit",
            scopeId: "s",
            payload: {},
          }),
        // Asynchronous: the caller's own multi-write transaction.
        () =>
          store.transaction(async (tx) => {
            await tx.tasks.createTask({
              taskId: "never-written-async",
              kind: "unit",
              scopeId: "s",
              payload: {},
            });
          }),
      ]) {
        try {
          await attempt();
          codes.push(undefined);
        } catch (err) {
          codes.push((err as { code?: string }).code);
        }
      }
      // The lock really was unavailable — otherwise the rest proves nothing.
      expect(codes).toEqual(["SQLITE_BUSY", "SQLITE_BUSY"]);

      blocker.exec("ROLLBACK");

      // THE ASSERTION: the handle is still transactional. Under the bug this
      // write survived its own rollback.
      await expect(
        store.transaction(async (tx) => {
          await tx.tasks.createTask({
            taskId: "rolled-back",
            kind: "unit",
            scopeId: "s",
            payload: {},
          });
          throw new Error("boom");
        }),
      ).rejects.toThrow("boom");
      expect(await store.tasks.getTask("rolled-back")).toBeNull();

      // And still usable: the failed BEGIN did not wedge the connection either.
      await store.tasks.createTask({
        taskId: "after",
        kind: "unit",
        scopeId: "s2",
        payload: {},
      });
      expect((await store.tasks.getTask("after"))?.taskId).toBe("after");
      const claim = await store.tasks.claimNext({
        ownerId: "owner",
        now: new Date(),
        scopesBusy: [],
      });
      expect(claim?.task.taskId).toBe("after");
    } finally {
      blocker.close();
      store.close();
      scratch.cleanup();
    }
  });

  it("shares one file with a SECOND BUN PROCESS, each task executed once", async () => {
    // The topology `busy_timeout` is actually for: the lock holder is another
    // OS process, running on its own thread, so parking this one is exactly the
    // right wait — and unlike the single-process case, the holder can commit
    // while we sleep. This is also the only case in this file where the two
    // handles are genuinely, uncoordinatedly concurrent through a full
    // claim-execute-land cycle.
    const scratch = createSqliteScratch("spawn");
    const dir = dirname(scratch.path);
    const workerPath = join(dir, "worker.ts");
    const readyPath = join(dir, "ready");
    const goPath = join(dir, "go");
    const resultPath = join(dir, "child-claims.json");
    const storeModule = join(import.meta.dir, "..", "src", "index.js");

    // A whole second worker: same store class, same file, no shared memory
    // whatsoever — the only thing keeping it and the parent apart is SQLite.
    writeFileSync(
      workerPath,
      `import { existsSync, writeFileSync } from "node:fs";
import { SqliteAssistantStore } from ${JSON.stringify(storeModule)};

const store = new SqliteAssistantStore(process.argv[2]);
const claimed = [];
writeFileSync(process.argv[3], "ready");
// Barrier: the parent writes "go" once it has seen "ready", so neither side
// can drain the queue while the other is still starting up.
while (!existsSync(process.argv[5])) await Bun.sleep(1);
for (let idle = 0; idle < 40; ) {
  const claim = await store.tasks.claimNext({
    ownerId: "child",
    now: new Date(),
    scopesBusy: [],
  });
  if (claim === null) { idle += 1; await Bun.sleep(1); continue; }
  idle = 0;
  // A tick of "work" per task, so both processes are in the queue at once
  // instead of one of them finishing before the other's first claim.
  await Bun.sleep(1);
  claimed.push(claim.task.taskId);
  const seq = await store.tasks.nextSeq(claim.task.taskId);
  await store.tasks.appendEvents(
    claim.task.taskId,
    [{ type: "spawn.done", seq, eventId: "evt-child-" + claim.task.taskId,
       timestamp: new Date().toISOString(), contractVersion: ${JSON.stringify(CONTRACT_VERSION)},
       attemptId: claim.attempt.attemptId }],
    { leaseToken: claim.lease.leaseToken },
  );
  await store.tasks.transitionTask(claim.task.taskId, ["running"], "completed", {
    finishedAt: new Date().toISOString(),
  });
  await store.tasks.endAttempt({ attemptId: claim.attempt.attemptId, status: "completed" });
  await store.tasks.releaseLease(claim.lease.leaseToken);
}
writeFileSync(process.argv[4], JSON.stringify(claimed));
store.close();
`,
    );

    const parent = new SqliteAssistantStore(scratch.path);
    const taskIds: string[] = [];
    try {
      for (let i = 0; i < 24; i += 1) {
        const taskId = `sp${String(i).padStart(2, "0")}`;
        await parent.tasks.createTask({
          taskId,
          kind: "unit",
          scopeId: `scope-${i}`,
          payload: {},
        });
        taskIds.push(taskId);
      }

      const child = Bun.spawn({
        cmd: [
          "bun",
          "run",
          workerPath,
          scratch.path,
          readyPath,
          resultPath,
          goPath,
        ],
        stdout: "pipe",
        stderr: "pipe",
      });

      // Start together, so the two processes really do contend rather than
      // taking turns because one of them finished first.
      const readyBy = Date.now() + 20_000;
      while (!existsSync(readyPath)) {
        if (Date.now() > readyBy)
          throw new Error("child never signalled ready");
        await Bun.sleep(2);
      }
      writeFileSync(goPath, "go");

      const parentClaims: string[] = [];
      for (let idle = 0; idle < 40; ) {
        const claim = await parent.tasks.claimNext({
          ownerId: "parent",
          now: new Date(),
          scopesBusy: [],
        });
        if (claim === null) {
          idle += 1;
          await Bun.sleep(1);
          continue;
        }
        idle = 0;
        await Bun.sleep(1);
        parentClaims.push(claim.task.taskId);
        const seq = await parent.tasks.nextSeq(claim.task.taskId);
        await parent.tasks.appendEvents(
          claim.task.taskId,
          [
            {
              type: "spawn.done",
              seq,
              eventId: `evt-parent-${claim.task.taskId}`,
              timestamp: new Date().toISOString(),
              contractVersion: CONTRACT_VERSION,
              attemptId: claim.attempt.attemptId,
            },
          ],
          { leaseToken: claim.lease.leaseToken },
        );
        await parent.tasks.transitionTask(
          claim.task.taskId,
          ["running"],
          "completed",
          { finishedAt: new Date().toISOString() },
        );
        await parent.tasks.endAttempt({
          attemptId: claim.attempt.attemptId,
          status: "completed",
        });
        await parent.tasks.releaseLease(claim.lease.leaseToken);
      }

      const exitCode = await child.exited;
      if (exitCode !== 0) {
        throw new Error(
          `child exited ${exitCode}: ${await new Response(child.stderr).text()}`,
        );
      }
      const childClaims = JSON.parse(
        readFileSync(resultPath, "utf8"),
      ) as string[];

      // BOTH processes did work — otherwise this is a one-process test wearing
      // a costume.
      expect(parentClaims.length).toBeGreaterThan(0);
      expect(childClaims.length).toBeGreaterThan(0);
      // And every task was executed EXACTLY once, across both of them. No
      // overlap is the whole claim `BEGIN IMMEDIATE` is making.
      const all = [...parentClaims, ...childClaims];
      expect(new Set(all).size).toBe(all.length);
      expect(new Set(all).size).toBe(taskIds.length);

      const view = await snapshotTaskInvariants({
        reader: parent.tasks,
        taskIds,
        dumpAttempts: () => dumpSqliteAttempts(scratch.path),
        dumpLiveLeases: () => dumpSqliteLeases(scratch.path),
      });
      expect(
        checkTaskInvariants(view, {
          phase: "quiescent",
          label: "two processes",
        }),
      ).toEqual([]);
      for (const task of view.tasks) {
        expect(task.status).toBe("completed");
        expect(task.attemptCount).toBe(1);
        // One event, written by whichever process owned the one attempt.
        expect((view.events.get(task.taskId) ?? []).length).toBe(1);
      }
    } finally {
      parent.close();
      scratch.cleanup();
    }
  }, 30_000);

  it.skip("runs a concurrent claim-AND-EXECUTE workload over two handles in one process", async () => {
    // REMAINING LIMITATION, pinned rather than asserted — and NOT the one the
    // busy-timeout work fixed. Concurrent CLAIMS are fine (the test above), and
    // so is the cross-process topology (the `Bun.spawn` test). What still fails
    // is both at once IN ONE PROCESS:
    //
    //   `SqliteTaskStore.claimNext` holds its transaction across `await`s. The
    //   other handle's SYNCHRONOUS transactions — `appendEvents`,
    //   `transitionTask`, `endAttempt`, every `withTx` — cannot wait on the
    //   event loop, so their `BEGIN IMMEDIATE` parks the thread, which is the
    //   only thread that could run the holder's continuation and commit. The
    //   result is a stall for the whole `busy_timeout` and then SQLITE_BUSY.
    //
    // Reproduction: `runTaskSchedule` over `createSqliteHarness(2)` with
    // `workers: 4, tasks: 24, steps: 40`. Seeds 1 and 1337 pass in ~30ms; SEED
    // 7 stalls 5.3s and throws `SQLiteError: database is locked`.
    //
    // The fix is architectural, not a pragma: either the synchronous
    // transaction helpers become async, or `claimNext` stops holding its
    // transaction across `await`s (which would also close the documented
    // `transaction()` flattening hazard). Both are adapter redesigns beyond the
    // scope this landing was given.
    expect(true).toBe(true);
  });
});
