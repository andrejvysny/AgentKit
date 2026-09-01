/**
 * What a claim leaves behind when one of its sub-steps blows up.
 *
 * `TaskStore.claimNext` promises to "atomically claim ... creating its attempt
 * and lease". Atomic is a claim about FAILURE, not about success — any
 * implementation looks atomic when nothing goes wrong. So this file breaks each
 * step of the claim in turn and asks the only question that grades the promise:
 * is the durable state afterwards one the port permits?
 *
 * HOW THE FAULT GETS IN, AND WHY NOT A WRAPPER. A proxy around the store —
 * the obvious way to make `transitionTask` throw on the nth call — cannot reach
 * this: `claimNext` calls its own `this.transitionTask` / `this.createAttempt` /
 * `this.acquireLease`, so a wrapper only ever sees the OUTER call and the
 * sub-steps run on the unwrapped instance. What both adapters do take from
 * outside is their two constructor dependencies, the {@link Clock} and the
 * {@link IdGenerator}, and between them they land on every step of the claim,
 * in order:
 *
 * | armed call     | claim reaches                                     |
 * |----------------|---------------------------------------------------|
 * | `nowIso` #1    | the `startedAt` stamp, BEFORE the transition        |
 * | `nowIso` #2    | `createAttempt`, AFTER the transition committed     |
 * | `attemptId` #1 | the same boundary, through the id seam              |
 * | `now` #1       | `acquireLease`, after the attempt row was written   |
 * | `nowIso` #3    | nothing — the claim is over. The control case: it   |
 * |                | proves the harness does not just fail everything.   |
 *
 * The third and fourth are the interesting ones: by the time they fire, the
 * claim has already written the task row and (for `now` #1) the attempt row, so
 * "the store is unchanged afterwards" is a statement about rollback, not about
 * luck. `claim-next-concurrency.test.ts` uses the same `attemptId` seam for a
 * different question (does ONE caller's rollback take the OTHER caller's claim
 * with it?); this file asks what a single caller's rollback leaves.
 *
 * The suite is written so that every injection point asserts the same
 * disjunction — either the claim threw AND changed nothing, or it succeeded AND
 * produced a whole claim — which means it does not have to hard-code which call
 * index hits which step, and stays honest if the adapter's call order changes.
 *
 * WHAT IT FOUND. `MemoryTaskStore.claimNext` used to fail this: its three
 * writes had no undo, so a throw at `nowIso` #2, `attemptId` #1 or `now` #1 left
 * the task `running` with no lease — work nothing could finish, nothing could
 * re-claim (there is no `running → queued` edge) and `expireStaleLeases` could
 * not see. It now captures the pre-claim state and restores it, mirroring the
 * sqlite adapter's ROLLBACK, and BOTH adapters are held to the identical
 * assertions below — which is the point of running one suite over both.
 */
import { describe, expect, it } from "bun:test";
import {
  checkTaskInvariants,
  snapshotTaskInvariants,
  type ObservedLease,
} from "@agentkit/testing";
import {
  defaultIds,
  type AssistantStore,
  type Clock,
  type IdGenerator,
} from "@agentkit/host";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SqliteAssistantStore } from "../src/index.js";
import {
  createSqliteScratch,
  dumpSqliteAttempts,
  dumpSqliteLeases,
} from "./support/durability-harness.js";

const FIXED_NOW = "2026-01-01T00:00:00.000Z";

/**
 * A clock and an id generator that behave until they are ARMED, then throw on
 * the nth call of one named method.
 *
 * Armed after setup rather than at construction because the setup itself burns
 * calls — `createTask` stamps an `enqueuedAt` — and an injection point counted
 * from process start would name a different step every time the setup changed.
 */
interface FaultRig {
  clock: Clock;
  ids: IdGenerator;
  arm(target: FaultTarget, nth: number): void;
  fired(): boolean;
}

type FaultTarget = "nowIso" | "now" | "attemptId";

function createFaultRig(): FaultRig {
  let armed: { target: FaultTarget; nth: number } | null = null;
  let fired = false;
  const counts: Record<FaultTarget, number> = {
    nowIso: 0,
    now: 0,
    attemptId: 0,
  };

  const maybeThrow = (target: FaultTarget): void => {
    if (armed === null || armed.target !== target) return;
    counts[target] += 1;
    if (counts[target] !== armed.nth) return;
    fired = true;
    throw new Error(`injected failure: ${target} call ${armed.nth}`);
  };

  return {
    clock: {
      now: () => {
        maybeThrow("now");
        return new Date(FIXED_NOW);
      },
      nowIso: () => {
        maybeThrow("nowIso");
        return FIXED_NOW;
      },
    },
    ids: {
      ...defaultIds,
      attemptId: () => {
        maybeThrow("attemptId");
        return defaultIds.attemptId();
      },
    },
    arm: (target, nth) => {
      armed = { target, nth };
      counts.nowIso = 0;
      counts.now = 0;
      counts.attemptId = 0;
      fired = false;
    },
    fired: () => fired,
  };
}

/** Every injection point the two seams reach on the claim path. */
const INJECTION_POINTS: readonly { target: FaultTarget; nth: number }[] = [
  { target: "nowIso", nth: 1 },
  { target: "nowIso", nth: 2 },
  { target: "nowIso", nth: 3 },
  { target: "attemptId", nth: 1 },
  { target: "now", nth: 1 },
];

interface InjectionHarness {
  store: AssistantStore;
  rig: FaultRig;
  dumpAttempts(): ReturnType<typeof dumpSqliteAttempts>;
  dumpLiveLeases(): ReturnType<typeof dumpSqliteLeases>;
  close(): void;
}

function memoryInjectionHarness(): InjectionHarness {
  const rig = createFaultRig();
  const store = new MemoryAssistantStore({ clock: rig.clock, ids: rig.ids });
  return {
    store,
    rig,
    dumpAttempts: () => [...store.tasks.attempts.values()],
    // The private-field read is explained in durability-harness.ts; repeated
    // here rather than shared because this file needs it on a store it built
    // with its own faulty clock.
    dumpLiveLeases: () =>
      [
        ...(
          store.tasks as unknown as { leases: Map<string, never> }
        ).leases.values(),
      ] as ReturnType<typeof dumpSqliteLeases>,
    close: () => undefined,
  };
}

function sqliteInjectionHarness(): InjectionHarness {
  const rig = createFaultRig();
  const scratch = createSqliteScratch("inject");
  const store = new SqliteAssistantStore(scratch.path, {
    clock: rig.clock,
    ids: rig.ids,
  });
  return {
    store,
    rig,
    dumpAttempts: () => dumpSqliteAttempts(scratch.path),
    dumpLiveLeases: () => dumpSqliteLeases(scratch.path),
    close: () => {
      store.close();
      scratch.cleanup();
    },
  };
}

/**
 * Runs one injection point and reports what the store looked like afterwards.
 * Shared so the memory and sqlite cases assert against identical evidence.
 */
async function claimUnderFault(
  harness: InjectionHarness,
  point: { target: FaultTarget; nth: number },
): Promise<{
  threw: boolean;
  claimed: boolean;
  status: string | undefined;
  attemptCount: number | undefined;
  attempts: number;
  runningAttempts: number;
  leases: number;
  observedLeases: ObservedLease[];
}> {
  const { store, rig } = harness;
  await store.tasks.createTask({
    taskId: "victim",
    kind: "unit",
    scopeId: "scope-victim",
    payload: {},
  });

  rig.arm(point.target, point.nth);
  let threw = false;
  let claimed = false;
  const observedLeases: ObservedLease[] = [];
  try {
    const claim = await store.tasks.claimNext({
      ownerId: "worker-a",
      now: new Date(FIXED_NOW),
      scopesBusy: [],
    });
    claimed = claim !== null;
    if (claim) observedLeases.push({ ...claim.lease, observedAt: 1 });
  } catch {
    threw = true;
  }

  const task = await store.tasks.getTask("victim");
  const attempts = harness.dumpAttempts();
  return {
    threw,
    claimed,
    status: task?.status,
    attemptCount: task?.attemptCount,
    attempts: attempts.length,
    runningAttempts: attempts.filter((a) => a.status === "running").length,
    leases: harness.dumpLiveLeases().length,
    observedLeases,
  };
}

function describeInjectedClaimFailure(
  name: string,
  create: () => InjectionHarness,
): void {
  describe(`${name} — injected mid-claim failure`, () => {
    /** Injection points that actually interrupted a claim — see the anti-vacuity case. */
    const interrupted: string[] = [];

    for (const point of INJECTION_POINTS) {
      const label = `${point.target} #${point.nth}`;

      // The universal half: whatever happened, the durable state must still be
      // one the invariant checker accepts. Runs on BOTH adapters.
      it(`leaves a checkable store after a failure at ${label}`, async () => {
        const harness = create();
        try {
          const outcome = await claimUnderFault(harness, point);
          if (outcome.threw) interrupted.push(label);
          // Two attempts marked running, or two leases over one task, is the
          // double-claim shape — unacceptable on any adapter, atomic or not.
          expect(outcome.runningAttempts).toBeLessThanOrEqual(1);
          expect(outcome.leases).toBeLessThanOrEqual(1);
          if (outcome.threw) {
            // A broken claim may undo itself or may keep the task; it may never
            // land it, and it may never leave a lease over work it did not take.
            expect(["queued", "running"]).toContain(
              outcome.status ?? "missing",
            );
            if (outcome.status === "queued") {
              expect(outcome.attempts).toBe(0);
              expect(outcome.leases).toBe(0);
            }
            if (outcome.leases === 1) expect(outcome.status).toBe("running");
          }
          const view = await snapshotTaskInvariants({
            reader: harness.store.tasks,
            taskIds: ["victim"],
            observedLeases: outcome.observedLeases,
            dumpAttempts: harness.dumpAttempts,
            // A task the claim left `running` is graded against the lease dump
            // by the orphan check — which is exactly the state at stake here —
            // so the dump is deliberately supplied even for the adapter that
            // fails it.
            dumpLiveLeases: harness.dumpLiveLeases,
          });
          expect(
            checkTaskInvariants(view, {
              phase: "quiescent",
              label: `${name} ${label}`,
            }),
          ).toEqual([]);
        } finally {
          harness.close();
        }
      });

      it(`rolls the whole claim back on a failure at ${label}`, async () => {
        const harness = create();
        try {
          const outcome = await claimUnderFault(harness, point);
          if (outcome.threw) {
            expect(harness.rig.fired()).toBe(true);
            // Nothing the claim wrote survives: not the status, not the
            // attempt counter, not the attempt row, not the lease.
            expect(outcome.status).toBe("queued");
            expect(outcome.attemptCount).toBe(0);
            expect(outcome.attempts).toBe(0);
            expect(outcome.leases).toBe(0);
          } else {
            // The fault never reached this claim; it must then be a WHOLE
            // claim, not a partial one.
            expect(outcome.claimed).toBe(true);
            expect(outcome.status).toBe("running");
            expect(outcome.attemptCount).toBe(1);
            expect(outcome.runningAttempts).toBe(1);
            expect(outcome.leases).toBe(1);
          }
        } finally {
          harness.close();
        }
      });

      it(`hands the task to a later, clean claim after a failure at ${label}`, async () => {
        // A rollback that leaves the row unclaimable would be a different bug
        // wearing the same clothes: the point of rolling back is that the
        // work is still there for the next worker.
        const harness = create();
        try {
          const outcome = await claimUnderFault(harness, point);
          if (!outcome.threw) return;
          const claim = await harness.store.tasks.claimNext({
            ownerId: "worker-b",
            now: new Date(FIXED_NOW),
            scopesBusy: [],
          });
          expect(claim?.task.taskId).toBe("victim");
          expect(claim?.task.status).toBe("running");
          expect(claim?.attempt.attemptNumber).toBe(1);
          // A rolled-back claim must not have consumed a fencing token that
          // the surviving one then skips past — the counter is rolled back
          // with everything else, so the first real lease is token 1.
          expect(claim?.lease.fencingToken).toBe(1);
          await harness.store.tasks.transitionTask(
            "victim",
            ["running"],
            "completed",
            { finishedAt: FIXED_NOW },
          );
          expect((await harness.store.tasks.getTask("victim"))?.status).toBe(
            "completed",
          );
        } finally {
          harness.close();
        }
      });
    }

    it("interrupted the claim at several different steps", () => {
      // The whole file is vacuous if the rig never fires — every case would
      // fall into its "the fault never reached this claim" branch and assert
      // that a healthy claim is healthy. Declared last so it runs after the
      // cases that fill `interrupted` (bun runs an `it` in declaration order).
      expect(interrupted.length).toBeGreaterThanOrEqual(3);
    });
  });
}

describeInjectedClaimFailure(
  "SqliteAssistantStore (file)",
  sqliteInjectionHarness,
);
describeInjectedClaimFailure("MemoryAssistantStore", memoryInjectionHarness);
