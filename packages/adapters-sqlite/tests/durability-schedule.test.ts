/**
 * Seeded random schedules over both adapters, graded by the invariant checker.
 *
 * The rest of this directory tests scenarios someone thought of. This file
 * tests the ones nobody did: three workers claiming, writing, retrying,
 * crashing, recovering and cancelling against a graph of dependency chains,
 * fan-ins and lineage, in an order drawn from a seeded PRNG. What it asserts is
 * not "the right thing happened" — for a random schedule there is no single
 * right thing — but "the durable state is still one the port permits", which is
 * the only claim that survives arbitrary interleaving.
 *
 * REPRODUCING A FAILURE: every message carries its seed. `AGENTKIT_SEED=1337
 * bun test durability-schedule` re-runs exactly that schedule — the clock is
 * logical, the ids are counters, and each worker draws from its own stream, so
 * nothing about the run depends on the wall clock or on how fast the machine
 * is.
 *
 * See `packages/testing/src/task-invariants.ts` for what is checkable from
 * outside the port and what needs the adapter dump this directory supplies.
 */
import { describe, expect, it } from "bun:test";
import { checkTaskInvariants, runTaskSchedule } from "@agentkit/testing";
import {
  createMemoryHarness,
  createSqliteHarness,
  durabilitySeeds,
  type DurabilityHarness,
} from "./support/durability-harness.js";

const { seeds: SEEDS, custom: CUSTOM_SEED } = durabilitySeeds();

function describeSchedule(name: string, create: () => DurabilityHarness): void {
  describe(`${name} — seeded random schedule`, () => {
    // Accumulated across the whole seed set and asserted at the end: any ONE
    // seed may miss a path, but a seed set that never crashes a worker or never
    // burns a retry budget is a seed set grading an easier system than the one
    // shipped.
    const totals = {
      claimed: 0,
      retries: 0,
      crashes: 0,
      recovered: 0,
      deadLettered: 0,
      cancelled: 0,
      settledByDependency: 0,
      multiAttempt: 0,
    };

    for (const seed of SEEDS) {
      it(`holds every task invariant under seed ${seed}`, async () => {
        const harness = create();
        try {
          const result = await runTaskSchedule({
            target: harness.target,
            seed,
            clock: harness.clock,
            ids: harness.ids,
            leaseTtlMs: harness.leaseTtlMs,
            workers: 3,
            tasks: 24,
            steps: 40,
          });

          // Mid-run spot checks first: a violation that only exists WHILE work
          // is in flight is invisible once everything has drained, and it is
          // the more interesting kind — two live claims on one task, a fencing
          // token that went backwards between two attempts.
          expect(result.spotCheckViolations).toEqual([]);

          // Then the strict form, with nothing executing.
          expect(
            checkTaskInvariants(result.view, {
              phase: "quiescent",
              label: `${name} seed ${seed}`,
            }),
          ).toEqual([]);

          // A task the schedule can neither finish nor settle is a queue that
          // needs a human — the exact failure `dependsOn`'s acyclicity rule and
          // the lazy settle on the claim path exist to make impossible.
          expect(result.undrained).toEqual([]);

          // Guard against a schedule that silently degenerates into "claim
          // nothing, assert nothing" — every invariant above is vacuous if the
          // workers never touched the store.
          expect(result.stats.created).toBe(24);
          expect(result.stats.claimed).toBeGreaterThan(12);
          expect(result.stats.completed).toBeGreaterThan(0);
          expect(result.view.observedLeases.length).toBeGreaterThanOrEqual(
            result.stats.claimed,
          );

          totals.claimed += result.stats.claimed;
          totals.retries += result.stats.retries;
          totals.crashes += result.stats.crashes;
          totals.recovered += result.stats.recovered;
          totals.deadLettered += result.stats.deadLettered;
          totals.cancelled += result.view.tasks.filter(
            (t) => t.status === "cancelled",
          ).length;
          totals.settledByDependency += result.view.tasks.filter(
            (t) => t.attemptCount === 0 && t.status !== "completed",
          ).length;
          totals.multiAttempt += result.view.tasks.filter(
            (t) => t.attemptCount > 1,
          ).length;
        } finally {
          harness.close();
        }
      });
    }

    it("holds every invariant under a tighter retry budget (4 workers, 30 tasks)", async () => {
      // A second shape rather than a fourth seed: `maxAttempts: 2` makes the
      // poison path common instead of incidental, which is what puts the
      // dead-letter invariants (a poisoned task must be terminal, and must have
      // earned it with attempts) under real load.
      //
      // The seed is a COVERAGE choice, not an oracle: what the case asserts is
      // the invariants, and the `deadLettered` floor below only keeps the
      // schedule from grading an easier system than the one shipped. Which seed
      // reaches the poison path depends on the interleaving, so a change to the
      // adapters' queueing can move it — this one dead-letters on both
      // reference adapters with room to spare.
      const harness = create();
      try {
        const result = await runTaskSchedule({
          target: harness.target,
          seed: 31337,
          clock: harness.clock,
          ids: harness.ids,
          leaseTtlMs: harness.leaseTtlMs,
          workers: 4,
          tasks: 30,
          steps: 45,
          maxAttempts: 2,
        });
        expect(result.spotCheckViolations).toEqual([]);
        expect(
          checkTaskInvariants(result.view, {
            phase: "quiescent",
            label: `${name} stress seed 31337`,
          }),
        ).toEqual([]);
        expect(result.undrained).toEqual([]);
        expect(result.stats.deadLettered).toBeGreaterThan(0);
        expect(
          result.view.tasks.filter((t) => t.deadLetteredAt !== undefined)
            .length,
        ).toBe(result.stats.deadLettered);

        totals.claimed += result.stats.claimed;
        totals.retries += result.stats.retries;
        totals.crashes += result.stats.crashes;
        totals.recovered += result.stats.recovered;
        totals.deadLettered += result.stats.deadLettered;
        totals.cancelled += result.view.tasks.filter(
          (t) => t.status === "cancelled",
        ).length;
        totals.settledByDependency += result.view.tasks.filter(
          (t) => t.attemptCount === 0 && t.status !== "completed",
        ).length;
        totals.multiAttempt += result.view.tasks.filter(
          (t) => t.attemptCount > 1,
        ).length;
      } finally {
        harness.close();
      }
    });

    it.skipIf(CUSTOM_SEED)(
      "exercised every durable path the invariants are meant to grade",
      () => {
        // Declared after the seed cases so it runs after them (bun executes an
        // `it` in declaration order). Without this the suite could go green on a
        // schedule that never crashed a worker, never retried, and never let a
        // dependency settle a dependent — a green that means nothing.
        expect(totals.claimed).toBeGreaterThan(40);
        expect(totals.retries).toBeGreaterThan(0);
        expect(totals.crashes).toBeGreaterThan(0);
        expect(totals.recovered).toBeGreaterThan(0);
        expect(totals.deadLettered).toBeGreaterThan(0);
        expect(totals.cancelled).toBeGreaterThan(0);
        expect(totals.settledByDependency).toBeGreaterThan(0);
        expect(totals.multiAttempt).toBeGreaterThan(0);
      },
    );

    it(`replays a seed identically: same schedule, same durable state`, async () => {
      // Determinism is not a nicety here — it is what makes the seed in a
      // failure message worth printing. Two runs of one seed must agree on
      // every task's final status and on the whole event log, or a red build
      // could not be reproduced from its own output.
      const shapes: string[] = [];
      for (let run = 0; run < 2; run += 1) {
        const harness = create();
        try {
          const result = await runTaskSchedule({
            target: harness.target,
            seed: 99,
            clock: harness.clock,
            ids: harness.ids,
            leaseTtlMs: harness.leaseTtlMs,
            workers: 3,
            tasks: 12,
            steps: 20,
          });
          shapes.push(
            JSON.stringify({
              tasks: result.view.tasks.map((t) => [
                t.taskId,
                t.status,
                t.attemptCount,
              ]),
              events: [...result.view.events].map(([taskId, events]) => [
                taskId,
                events.map((e) => `${e.seq}:${e.type}`),
              ]),
              stats: result.stats,
            }),
          );
        } finally {
          harness.close();
        }
      }
      expect(shapes[0]).toBe(shapes[1]!);
    });
  });
}

describeSchedule("MemoryAssistantStore", createMemoryHarness);
describeSchedule("SqliteAssistantStore (file)", () => createSqliteHarness(1));
