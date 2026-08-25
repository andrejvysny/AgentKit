// Two `claimNext` calls in flight at once, against the same store instance.
//
// `claimNext` walks an ORDERED candidate list and awaits a transition per
// candidate, so its rows are a snapshot a second caller can invalidate under
// it. Both adapters used to treat that as a fault: the loser of the
// `queued`->{failed,running} CAS threw `InvalidTaskTransitionError` out of
// `claimNext`. The sqlite adapter was worse — `withAsyncTx` FLATTENS a
// re-entrant call into the transaction already open on the connection, so the
// two callers shared one transaction and the first caller's ROLLBACK discarded
// the claim the second had already been granted, reverting the task row to
// `queued` while its attempt and lease committed afterwards. A later
// `claimNext` then handed the same task to a second worker.
import { describe, expect, it } from "bun:test";
import type { AssistantStore, ClaimedTask, IdGenerator } from "@agentkit/host";
import { defaultIds } from "@agentkit/host";
import { MemoryAssistantStore, SqliteAssistantStore } from "../src/index.js";

/**
 * A queue whose head is doomed, whose middle is claimable, and whose tail is
 * doomed again — the interleaving that made the second caller claim
 * `claimable` while the first was still settling the head, and then hit the
 * head-on collision on its own way past it.
 *
 * Distinct scopes throughout: this is about two claims racing on the SAME
 * candidate list, not about the scope filter.
 */
async function seedDoomedAroundClaimable(store: AssistantStore): Promise<void> {
  await store.tasks.createTask({
    taskId: "dep",
    kind: "unit",
    scopeId: "scope-dep",
    payload: {},
  });
  await store.tasks.transitionTask("dep", ["queued"], "running");
  await store.tasks.transitionTask("dep", ["running"], "failed", {
    error: "dependency blew up",
  });

  for (const [taskId, priority] of [
    ["doomed-head", 10],
    ["doomed-tail", 1],
  ] as const) {
    await store.tasks.createTask({
      taskId,
      kind: "unit",
      scopeId: `scope-${taskId}`,
      payload: {},
      priority,
      dependsOn: ["dep"],
    });
  }
  await store.tasks.createTask({
    taskId: "claimable",
    kind: "unit",
    scopeId: "scope-claimable",
    payload: {},
    priority: 5,
  });
}

/** `Promise.all` would hide every later assertion behind the first rejection. */
async function claimTwice(
  store: AssistantStore,
): Promise<PromiseSettledResult<ClaimedTask | null>[]> {
  const now = new Date();
  return Promise.allSettled([
    store.tasks.claimNext({ ownerId: "worker-a", now, scopesBusy: [] }),
    store.tasks.claimNext({ ownerId: "worker-b", now, scopesBusy: [] }),
  ]);
}

function fulfilledValues(
  settled: PromiseSettledResult<ClaimedTask | null>[],
): (ClaimedTask | null)[] {
  const rejections = settled
    .filter((r) => r.status === "rejected")
    .map((r) => String((r as PromiseRejectedResult).reason));
  // A lost CAS is the race resolving normally — the loser skips the candidate
  // and keeps walking, it does not fail the caller.
  expect(rejections).toEqual([]);
  return settled.map(
    (r) => (r as PromiseFulfilledResult<ClaimedTask | null>).value,
  );
}

function describeConcurrentClaim(
  name: string,
  create: () => { store: AssistantStore; close: () => void },
): void {
  describe(`${name} — concurrent claimNext`, () => {
    it("hands each task to at most one caller and settles the doomed dependents", async () => {
      const { store, close } = create();
      try {
        await seedDoomedAroundClaimable(store);

        const claims = fulfilledValues(await claimTwice(store)).filter(
          (claim): claim is ClaimedTask => claim !== null,
        );

        // Exactly one claimable task existed, so exactly one call may win it
        // and no task id may appear twice.
        const claimedIds = claims.map((c) => c.task.taskId);
        expect(claimedIds).toEqual(["claimable"]);
        expect(new Set(claimedIds).size).toBe(claimedIds.length);

        // The doomed dependents settle on the claim path, whichever caller
        // walks past them — and they stay settled.
        for (const taskId of ["doomed-head", "doomed-tail"]) {
          const doomed = await store.tasks.getTask(taskId);
          expect(doomed?.status).toBe("failed");
        }
      } finally {
        close();
      }
    });

    it("leaves the granted claim intact: task running, lease still current", async () => {
      const { store, close } = create();
      try {
        await seedDoomedAroundClaimable(store);

        const claims = fulfilledValues(await claimTwice(store)).filter(
          (claim): claim is ClaimedTask => claim !== null,
        );
        expect(claims.length).toBe(1);
        const claim = claims[0]!;

        // The rollback bug reverted this row to `queued` while the attempt and
        // lease rows committed — a task a later claimNext would hand out again.
        const task = await store.tasks.getTask(claim.task.taskId);
        expect(task?.status).toBe("running");

        // renewLease throws LeaseLostError unless the token is the current one.
        const renewed = await store.tasks.renewLease(
          claim.lease.leaseToken,
          30_000,
        );
        expect(renewed.taskId).toBe(claim.task.taskId);
        expect(renewed.attemptId).toBe(claim.attempt.attemptId);

        // And the queue agrees it is gone: nothing claimable is left.
        expect(
          await store.tasks.claimNext({
            ownerId: "worker-c",
            now: new Date(),
            scopesBusy: [],
          }),
        ).toBeNull();
      } finally {
        close();
      }
    });
  });
}

describeConcurrentClaim("MemoryAssistantStore", () => ({
  store: new MemoryAssistantStore(),
  close: () => undefined,
}));

describeConcurrentClaim("SqliteAssistantStore", () => {
  const store = new SqliteAssistantStore(":memory:");
  return { store, close: () => store.close() };
});

/** `defaultIds`, but the nth `attemptId()` call blows up instead of answering. */
function idsFailingOnAttempt(nth: number): IdGenerator {
  let calls = 0;
  return {
    ...defaultIds,
    attemptId: () => {
      calls += 1;
      if (calls === nth) throw new Error("attempt id generator is down");
      return defaultIds.attemptId();
    },
  };
}

describe("SqliteAssistantStore — concurrent claimNext rollback isolation", () => {
  it("rolls back only the failing caller, leaving the other claim committed", async () => {
    // Skipping a lost CAS is not enough on its own: `withAsyncTx` flattens a
    // second `claimNext` into the transaction the first already opened, so a
    // genuine failure on either path used to ROLL BACK BOTH callers' work. The
    // survivor kept a claim whose task row had reverted to `queued` while its
    // lease committed afterwards — the same task, free for a second worker.
    //
    // Serialized, the two claims get a transaction each: the second caller's
    // failure is its own, and the first caller's claim stands.
    const store = new SqliteAssistantStore(":memory:", {
      // The FIRST claim to reach `createAttempt` succeeds; the second fails.
      ids: idsFailingOnAttempt(2),
    });
    try {
      await store.tasks.createTask({
        taskId: "dep",
        kind: "unit",
        scopeId: "scope-dep",
        payload: {},
      });
      await store.tasks.transitionTask("dep", ["queued"], "running");
      await store.tasks.transitionTask("dep", ["running"], "failed", {
        error: "dependency blew up",
      });
      await store.tasks.createTask({
        taskId: "doomed-head",
        kind: "unit",
        scopeId: "scope-doomed-head",
        payload: {},
        priority: 10,
        dependsOn: ["dep"],
      });
      for (const [taskId, priority] of [
        ["claimable-1", 5],
        ["claimable-2", 1],
      ] as const) {
        await store.tasks.createTask({
          taskId,
          kind: "unit",
          scopeId: `scope-${taskId}`,
          payload: {},
          priority,
        });
      }

      const settled = await claimTwice(store);
      const claims = settled
        .filter((r) => r.status === "fulfilled")
        .map((r) => (r as PromiseFulfilledResult<ClaimedTask | null>).value)
        .filter((claim): claim is ClaimedTask => claim !== null);
      expect(settled.filter((r) => r.status === "rejected").length).toBe(1);
      expect(claims.length).toBe(1);
      const claim = claims[0]!;

      // The surviving claim is real, not a lease pointing at a queued task.
      expect((await store.tasks.getTask(claim.task.taskId))?.status).toBe(
        "running",
      );
      expect(
        (await store.tasks.renewLease(claim.lease.leaseToken, 30_000)).taskId,
      ).toBe(claim.task.taskId);

      // The settle the winner performed on its way past the doomed head is
      // committed too, rather than undone by the other caller's rollback.
      expect((await store.tasks.getTask("doomed-head"))?.status).toBe("failed");

      // And the failing caller left nothing behind: its candidate is untouched.
      const abandoned = await store.tasks.getTask("claimable-2");
      expect(abandoned?.status).toBe("queued");
      expect(abandoned?.attemptCount).toBe(0);
    } finally {
      store.close();
    }
  });
});
