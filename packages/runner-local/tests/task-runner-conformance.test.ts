/**
 * {@link SingleProcessTaskRunner} against @agentkit/testing's shared
 * `TaskRunner` contract — once over the in-memory store, once over the sqlite
 * one.
 *
 * Running it twice is the point. The runner's promises (enqueue idempotency,
 * recovery from an expired lease, cancellation reaching a running worker, the
 * concurrency budget) are all statements about what ends up IN THE STORE, and
 * the two stores reach those states by completely different means — Map writes
 * versus a transactional `UPDATE ... WHERE status = ?`. A runner that passes
 * over Maps and fails over SQL has a race, not a bug in one adapter.
 */
import { describe, expect, it } from "bun:test";
import { Database } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { SqliteAssistantStore } from "@agentkit/adapters-sqlite";
import {
  describeTaskRunnerConformance,
  type TaskRunnerConformanceHarness,
} from "@agentkit/testing";
import { SingleProcessTaskRunner } from "../src/index.js";
import { createTestClock } from "./support/task-runner-harness.js";

/** Short enough that a test can step over it, long enough that no real wait crosses it. */
const LEASE_TTL_MS = 1_000;
const MAX_ATTEMPTS = 3;

/** The runner knobs both harnesses share. */
function runnerFor(
  store: MemoryAssistantStore | SqliteAssistantStore,
  clock: ReturnType<typeof createTestClock>,
) {
  return new SingleProcessTaskRunner({
    store,
    clock,
    pollMs: 5,
    // Far past any test's real lifetime: renewal must not quietly rescue a
    // lease this suite advances the clock past on purpose.
    heartbeatMs: 60_000,
    leaseTtlMs: LEASE_TTL_MS,
    maxAttempts: MAX_ATTEMPTS,
    // The backoff is measured against the same frozen clock the suite steps by
    // hand; the contract below is about WHETHER a retry happens, not when.
    retryBackoff: { baseMs: 0, jitterRatio: 0 },
  });
}

describeTaskRunnerConformance({
  name: "SingleProcessTaskRunner over MemoryAssistantStore",
  test: { describe, it, expect },
  create: async (): Promise<TaskRunnerConformanceHarness> => {
    const clock = createTestClock();
    const store = new MemoryAssistantStore({ clock, leaseTtlMs: LEASE_TTL_MS });
    return {
      runner: runnerFor(store, clock),
      store,
      clock,
      maxAttempts: MAX_ATTEMPTS,
      leaseTtlMs: LEASE_TTL_MS,
      seedTask: async ({ taskId, scopeId }) => {
        await store.tasks.createTask({
          taskId,
          kind: "conformance.kind",
          scopeId,
          payload: {},
        });
      },
      attemptsFor: async (taskId) =>
        [...store.tasks.attempts.values()]
          .filter((attempt) => attempt.taskId === taskId)
          .map((attempt) => ({
            attemptNumber: attempt.attemptNumber,
            status: attempt.status,
          })),
    };
  },
});

describeTaskRunnerConformance({
  name: "SingleProcessTaskRunner over SqliteAssistantStore",
  test: { describe, it, expect },
  create: async (): Promise<TaskRunnerConformanceHarness> => {
    const clock = createTestClock();
    // A file rather than ":memory:", because attempt history has no port method
    // and the only honest way to read it back is a second handle on the same
    // database — which ":memory:" cannot give.
    const dir = mkdtempSync(join(tmpdir(), "agentkit-runner-conformance-"));
    const path = join(dir, "store.sqlite");
    const store = new SqliteAssistantStore(path, {
      clock,
      leaseTtlMs: LEASE_TTL_MS,
    });
    const reader = new Database(path, { readonly: true });
    return {
      runner: runnerFor(store, clock),
      store,
      clock,
      maxAttempts: MAX_ATTEMPTS,
      leaseTtlMs: LEASE_TTL_MS,
      seedTask: async ({ taskId, scopeId }) => {
        await store.tasks.createTask({
          taskId,
          kind: "conformance.kind",
          scopeId,
          payload: {},
        });
      },
      attemptsFor: async (taskId) =>
        reader
          .query(
            "SELECT attempt_number, status FROM task_attempts WHERE task_id = ?",
          )
          .all(taskId)
          .map((row) => {
            const typed = row as { attempt_number: number; status: string };
            return {
              attemptNumber: typed.attempt_number,
              status: typed.status,
            };
          }),
      close: () => {
        reader.close();
        store.close();
        rmSync(dir, { recursive: true, force: true });
      },
    };
  },
});
