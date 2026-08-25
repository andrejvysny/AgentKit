/**
 * Adapter-specific plumbing for the durability suite: what `@agentkit/testing`
 * is not allowed to know.
 *
 * The invariant checker and the schedule driver are deliberately ignorant of
 * which store they are grading — they take public reads plus, optionally, a
 * dump of the attempt and lease tables. THIS file is where the dumps come from,
 * and it is the right place for them: `TaskStore` has no `listAttempts` and no
 * `listLeases`, and adding either so a test can assert would put a method in
 * the port that only tests call. A test layer, which already knows it is
 * holding a `MemoryAssistantStore` or a `SqliteAssistantStore`, can read the
 * same rows without asking the port to grow.
 *
 * - MEMORY: `MemoryTaskStore.attempts` is public. Leases are private, and are
 *   read here through one narrow, guarded cast — the alternative was to widen
 *   the adapter's API for the benefit of this file.
 * - SQLITE: a SECOND, read-only `bun:sqlite` connection on the same file, which
 *   is how `sqlite-specific.test.ts` already inspects rows the store class does
 *   not surface. WAL means it sees everything the writer has committed.
 */
import { Database } from "bun:sqlite";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  TaskService,
  type AssistantStore,
  type AttemptRecord,
  type IdGenerator,
  type Lease,
  type TaskRunner,
  type WorkerHandle,
} from "@agentkit/host";
import {
  createLogicalClock,
  type LogicalClock,
  type TaskScheduleTarget,
} from "@agentkit/testing";
import { MemoryAssistantStore, SqliteAssistantStore } from "../../src/index.js";

/** Logical ms a claim's lease is granted for. Small: the clock is fake. */
export const DURABILITY_LEASE_TTL_MS = 1_000;

/**
 * Counter-based ids, so a failure names something a human can grep for and two
 * runs of one seed produce byte-identical records. `defaultIds` would be
 * correct and useless here — a UUID in a violation message identifies nothing.
 */
export function createSequentialIds(): IdGenerator {
  const counters = new Map<string, number>();
  const next = (kind: string): string => {
    const n = (counters.get(kind) ?? 0) + 1;
    counters.set(kind, n);
    return `${kind}-${n}`;
  };
  return {
    taskId: () => next("task"),
    attemptId: () => next("att"),
    eventId: () => next("evt"),
    proposalId: () => next("prp"),
    operationId: () => next("op"),
    messageId: () => next("msg"),
  };
}

/**
 * The `TaskRunner` half of `TaskService`, reduced to the one thing the driver
 * needs from it: remembering which tasks were asked to stop.
 *
 * `TaskService.cancelTask` does NOT force a running task terminal — it asks
 * through the runner and lets the worker land it. So a stub that records the
 * request is exactly the seam a cooperative cancel needs, and the driver's
 * worker reads it back.
 */
export class RecordingTaskRunner implements TaskRunner {
  readonly cancelRequests = new Set<string>();

  async enqueue(): Promise<void> {
    // The driver IS the dispatch loop; there is no queue to poke.
  }

  async requestCancel(taskId: string): Promise<void> {
    this.cancelRequests.add(taskId);
  }

  async recover(): Promise<void> {
    // Recovery is part of the schedule under test, not a service call.
  }

  async startWorker(): Promise<WorkerHandle> {
    throw new Error("the schedule driver is the worker");
  }
}

export interface DurabilityHarness {
  target: TaskScheduleTarget;
  clock: LogicalClock;
  ids: IdGenerator;
  leaseTtlMs: number;
  close(): void;
}

/** Wires `TaskService` + the recording runner over `handles[0]`. */
function targetFor(
  handles: readonly AssistantStore[],
  ids: IdGenerator,
  clock: LogicalClock,
  dumps: Pick<TaskScheduleTarget, "dumpAttempts" | "dumpLiveLeases">,
): TaskScheduleTarget {
  const runner = new RecordingTaskRunner();
  const store = handles[0]!;
  const service = new TaskService({ store, taskRunner: runner, ids, clock });
  return {
    handles,
    submitTask: (spec) => service.submitTask(spec),
    cancelTask: (taskId) => service.cancelTask(taskId),
    cancelRequested: (taskId) => runner.cancelRequests.has(taskId),
    ...dumps,
  };
}

export function createMemoryHarness(): DurabilityHarness {
  const clock = createLogicalClock();
  const ids = createSequentialIds();
  const store = new MemoryAssistantStore({
    clock,
    ids,
    leaseTtlMs: DURABILITY_LEASE_TTL_MS,
  });
  return {
    target: targetFor([store], ids, clock, {
      dumpAttempts: () => [...store.tasks.attempts.values()],
      dumpLiveLeases: () => memoryLeases(store),
    }),
    clock,
    ids,
    leaseTtlMs: DURABILITY_LEASE_TTL_MS,
    close: () => undefined,
  };
}

/**
 * `MemoryTaskStore.leases` is `private`, which is a compile-time marker over a
 * perfectly real Map. Reading it here — guarded, so a rename fails as "no lease
 * dump" rather than as a `TypeError` in the middle of an unrelated assertion —
 * buys the "a terminal task must not still hold a live lease" check on the
 * memory adapter, which is precisely the shape of the bug
 * `claim-next-concurrency.test.ts` was written for.
 */
function memoryLeases(store: MemoryAssistantStore): Lease[] {
  const leases = (store.tasks as unknown as { leases?: unknown }).leases;
  if (!(leases instanceof Map)) {
    throw new Error(
      "MemoryTaskStore.leases is no longer a Map; update durability-harness.ts",
    );
  }
  return [...(leases as Map<string, Lease>).values()].map((l) => ({ ...l }));
}

/** A temp dir holding one sqlite file, removed when `close()` runs. */
export interface SqliteScratch {
  path: string;
  cleanup(): void;
}

export function createSqliteScratch(label: string): SqliteScratch {
  const dir = mkdtempSync(join(tmpdir(), `agentkit-durability-${label}-`));
  return {
    path: join(dir, "store.sqlite"),
    cleanup: () => rmSync(dir, { recursive: true, force: true }),
  };
}

interface AttemptRow {
  attempt_id: string;
  task_id: string;
  attempt_number: number;
  status: string;
  owner_id: string;
  started_at: string;
  ended_at: string | null;
  error: string | null;
}

interface LeaseRow {
  task_id: string;
  attempt_id: string;
  owner_id: string;
  lease_token: string;
  fencing_token: number;
  expires_at: string;
}

/**
 * Reads the attempt and lease tables through a fresh read-only connection.
 *
 * A new connection per dump rather than one long-lived reader: a WAL reader
 * pins the snapshot it opened, and a stale one would report the state the
 * suite had ten operations ago as if it were now.
 */
export function dumpSqliteAttempts(path: string): AttemptRecord[] {
  const db = new Database(path, { readonly: true });
  try {
    const rows = db.query(`SELECT * FROM task_attempts`).all() as AttemptRow[];
    return rows.map((row) => ({
      attemptId: row.attempt_id,
      taskId: row.task_id,
      attemptNumber: row.attempt_number,
      status: row.status as AttemptRecord["status"],
      ownerId: row.owner_id,
      startedAt: row.started_at,
      ...(row.ended_at === null ? {} : { endedAt: row.ended_at }),
      ...(row.error === null ? {} : { error: row.error }),
    }));
  } finally {
    db.close();
  }
}

export function dumpSqliteLeases(path: string): Lease[] {
  const db = new Database(path, { readonly: true });
  try {
    const rows = db.query(`SELECT * FROM leases`).all() as LeaseRow[];
    return rows.map((row) => ({
      taskId: row.task_id,
      attemptId: row.attempt_id,
      ownerId: row.owner_id,
      leaseToken: row.lease_token,
      fencingToken: row.fencing_token,
      expiresAt: row.expires_at,
    }));
  } finally {
    db.close();
  }
}

/**
 * A file-backed sqlite harness. `handleCount` > 1 opens that many INDEPENDENT
 * `SqliteAssistantStore` instances on the SAME file — separate connections,
 * separate per-instance claim mutexes, one set of tables.
 *
 * The clock and the id generator are shared across handles on purpose: two
 * connections minting the same `att-3` would collide on a primary key, and a
 * collision is not the thing this topology is meant to find.
 */
export function createSqliteHarness(handleCount = 1): DurabilityHarness {
  const clock = createLogicalClock();
  const ids = createSequentialIds();
  const scratch = createSqliteScratch(`h${handleCount}`);
  const handles = Array.from(
    { length: handleCount },
    () =>
      new SqliteAssistantStore(scratch.path, {
        clock,
        ids,
        leaseTtlMs: DURABILITY_LEASE_TTL_MS,
      }),
  );
  return {
    target: targetFor(handles, ids, clock, {
      dumpAttempts: () => dumpSqliteAttempts(scratch.path),
      dumpLiveLeases: () => dumpSqliteLeases(scratch.path),
    }),
    clock,
    ids,
    leaseTtlMs: DURABILITY_LEASE_TTL_MS,
    close: () => {
      for (const handle of handles) handle.close();
      scratch.cleanup();
    },
  };
}

/**
 * The seeds a normal `bun test` run grades, or the single seed named by
 * `AGENTKIT_SEED` when someone is reproducing a failure.
 *
 * Three fixed seeds rather than a fresh random one per run: a suite whose
 * failures cannot be reproduced from the output is a suite that reports
 * mysteries. Every assertion below folds the seed into its message, so a red
 * run tells you exactly what to put in `AGENTKIT_SEED`.
 */
export interface DurabilitySeeds {
  seeds: number[];
  /** True when AGENTKIT_SEED overrode the default set — see {@link durabilitySeeds}. */
  custom: boolean;
}

export function durabilitySeeds(): DurabilitySeeds {
  const requested = process.env["AGENTKIT_SEED"];
  if (requested !== undefined && requested.trim() !== "") {
    const parsed = Number.parseInt(requested, 10);
    // A reproduction run grades ONE schedule, so the cross-seed coverage
    // assertions (which expect the default set to have hit every path) stand
    // down rather than failing on a seed that was never meant to hit them.
    if (!Number.isNaN(parsed)) return { seeds: [parsed], custom: true };
  }
  return { seeds: [1, 7, 1337], custom: false };
}
