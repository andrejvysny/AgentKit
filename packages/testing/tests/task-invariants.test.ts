/**
 * The checker's own tests.
 *
 * A grader nobody grades is a green light with no bulb behind it: if
 * `checkTaskInvariants` quietly returned `[]` for everything, the randomized
 * schedule and the fault-injection suite over in `internal/reference-adapters`
 * would both pass forever and prove nothing. So every invariant it claims to
 * enforce gets a view here that breaks exactly that one — the same mutation the
 * adapters are being watched for, applied by hand — plus a clean baseline the
 * mutations are derived from, so a mutation that also broke something else
 * would show up as an extra message rather than hide inside the expected one.
 */
import { describe, expect, it } from "bun:test";
import { CONTRACT_VERSION, type TaskEventEnvelope } from "@agentkit/contracts";
import type { AttemptRecord, Lease, TaskRecord } from "@agentkit/host";
import {
  checkTaskInvariants,
  mulberry32,
  snapshotTaskInvariants,
  type ObservedLease,
  type TaskInvariantView,
} from "../src/index.js";

const T0 = "2026-01-01T00:00:00.000Z";
const T1 = "2026-01-01T00:00:01.000Z";

function task(over: Partial<TaskRecord> & { taskId: string }): TaskRecord {
  return {
    kind: "unit",
    scopeId: `scope-${over.taskId}`,
    status: "completed",
    priority: 0,
    enqueuedAt: T0,
    availableAt: T0,
    startedAt: T0,
    finishedAt: T1,
    payload: {},
    attemptCount: 1,
    poisonCount: 0,
    ...over,
  };
}

function attempt(over: Partial<AttemptRecord> & { attemptId: string; taskId: string }): AttemptRecord {
  return {
    attemptNumber: 1,
    status: "completed",
    ownerId: "worker-0",
    startedAt: T0,
    endedAt: T1,
    ...over,
  };
}

function lease(over: Partial<ObservedLease> & { taskId: string; observedAt: number }): ObservedLease {
  return {
    attemptId: `att-${over.taskId}-1`,
    ownerId: "worker-0",
    leaseToken: `lease-${over.taskId}-${over.observedAt}`,
    fencingToken: over.observedAt,
    expiresAt: T1,
    ...over,
  };
}

function event(taskId: string, seq: number, attemptId = `att-${taskId}-1`): TaskEventEnvelope {
  return {
    type: "unit.done",
    seq,
    eventId: `evt-${taskId}-${seq}`,
    timestamp: T0,
    contractVersion: CONTRACT_VERSION,
    attemptId,
  };
}

/**
 * A clean two-task view: `b` depends on `a`, both completed on one attempt
 * each, with a two-event log apiece and no live leases.
 */
function cleanView(): TaskInvariantView {
  const a = task({ taskId: "a" });
  const b = task({ taskId: "b", dependsOn: ["a"], parentTaskId: "a" });
  return {
    tasks: [b, a],
    events: new Map([
      ["a", [event("a", 0), event("a", 1)]],
      ["b", [event("b", 0), event("b", 1)]],
    ]),
    children: new Map([
      ["a", ["b"]],
      ["b", []],
    ]),
    observedLeases: [lease({ taskId: "a", observedAt: 1 }), lease({ taskId: "b", observedAt: 2 })],
    attempts: [
      attempt({ attemptId: "att-a-1", taskId: "a" }),
      attempt({ attemptId: "att-b-1", taskId: "b" }),
    ],
    attemptsAreComplete: true,
    liveLeases: [],
    inFlightTaskIds: new Set<string>(),
  };
}

/** Apply `mutate` to a clean view and return what the checker said about it. */
function violationsAfter(mutate: (view: TaskInvariantView) => void): string[] {
  const view = cleanView();
  mutate(view);
  return checkTaskInvariants(view, { phase: "quiescent" });
}

describe("checkTaskInvariants", () => {
  it("accepts a healthy view", () => {
    expect(checkTaskInvariants(cleanView(), { phase: "quiescent" })).toEqual([]);
  });

  it("labels every violation when a label is given", () => {
    const found = checkTaskInvariants(
      { ...cleanView(), tasks: [task({ taskId: "a", status: "failed", finishedAt: undefined })] },
      { phase: "quiescent", label: "seed 42" },
    );
    expect(found.length).toBeGreaterThan(0);
    expect(found.every((v) => v.startsWith("[seed 42] "))).toBe(true);
  });

  it("catches two attempts of one task marked running", () => {
    const found = violationsAfter((view) => {
      const attempts = [...view.attempts];
      attempts[0] = attempt({ attemptId: "att-a-1", taskId: "a", status: "running", endedAt: undefined });
      attempts.push(
        attempt({ attemptId: "att-a-2", taskId: "a", attemptNumber: 2, status: "running", endedAt: undefined }),
      );
      view.attempts = attempts;
      view.tasks = [view.tasks[0]!, task({ taskId: "a", status: "running", finishedAt: undefined, attemptCount: 2 })];
      view.liveLeases = [{ ...lease({ taskId: "a", observedAt: 3 }) } as Lease];
    });
    expect(found.join("\n")).toContain("attempts marked running");
  });

  it("catches a running task that no live lease owns", () => {
    // The orphan a non-atomic claim leaves behind: nothing can finish it, and
    // `expireStaleLeases` cannot even see it.
    const found = violationsAfter((view) => {
      view.tasks = [
        view.tasks[0]!,
        task({ taskId: "a", status: "running", finishedAt: undefined }),
      ];
      view.attempts = [
        attempt({ attemptId: "att-a-1", taskId: "a", status: "running", endedAt: undefined }),
        view.attempts[1]!,
      ];
    });
    expect(found.join("\n")).toContain("no live lease owns it");
  });

  it("catches a live lease left over a task that already landed", () => {
    const found = violationsAfter((view) => {
      view.liveLeases = [lease({ taskId: "a", observedAt: 9 })];
    });
    expect(found.join("\n")).toContain("still holds a live lease");
  });

  it("catches a fencing token that did not increase between two leases of one task", () => {
    const found = violationsAfter((view) => {
      view.observedLeases = [
        lease({ taskId: "a", observedAt: 1, fencingToken: 7 }),
        lease({ taskId: "a", observedAt: 2, fencingToken: 7 }),
      ];
    });
    expect(found.join("\n")).toContain("it must strictly increase");
  });

  it("catches one fencing token issued to two different tasks", () => {
    const found = violationsAfter((view) => {
      view.observedLeases = [
        lease({ taskId: "a", observedAt: 1, fencingToken: 4 }),
        lease({ taskId: "b", observedAt: 2, fencingToken: 4 }),
      ];
    });
    expect(found.join("\n")).toContain("was issued twice");
  });

  it("catches a gap in a task's event sequence", () => {
    const found = violationsAfter((view) => {
      view.events = new Map(view.events).set("a", [event("a", 0), event("a", 2)]);
    });
    expect(found.join("\n")).toContain("not gapless from 0");
  });

  it("catches an event log that does not start at 0", () => {
    const found = violationsAfter((view) => {
      view.events = new Map(view.events).set("a", [event("a", 1), event("a", 2)]);
    });
    expect(found.join("\n")).toContain("not gapless from 0");
  });

  it("catches a repeated eventId", () => {
    const found = violationsAfter((view) => {
      const duplicate = { ...event("a", 1), eventId: "evt-a-0" };
      view.events = new Map(view.events).set("a", [event("a", 0), duplicate]);
    });
    expect(found.join("\n")).toContain("duplicate eventId");
  });

  it("catches an event stamped with an attempt that is not the task's", () => {
    // The zombie writer that got past the lease check.
    const found = violationsAfter((view) => {
      view.events = new Map(view.events).set("a", [
        event("a", 0),
        event("a", 1, "att-b-1"),
      ]);
    });
    expect(found.join("\n")).toContain("which is not an attempt of this task");
  });

  it("catches a terminal task with no finishedAt", () => {
    const found = violationsAfter((view) => {
      view.tasks = [view.tasks[0]!, task({ taskId: "a", finishedAt: undefined })];
    });
    expect(found.join("\n")).toContain("has no finishedAt");
  });

  it("catches a finishedAt stamped before the task landed", () => {
    const found = violationsAfter((view) => {
      view.tasks = [
        view.tasks[0]!,
        task({ taskId: "a", status: "running", finishedAt: T1 }),
      ];
    });
    expect(found.join("\n")).toContain("already carries finishedAt");
  });

  it("catches a dead-letter mark on a task that was never landed", () => {
    const found = violationsAfter((view) => {
      view.tasks = [
        view.tasks[0]!,
        task({
          taskId: "a",
          status: "running",
          finishedAt: undefined,
          deadLetteredAt: T1,
          deadLetterReason: "poison",
        }),
      ];
    });
    expect(found.join("\n")).toContain("the poison row alone stops nothing");
  });

  it("catches a half-written dead-letter mark", () => {
    const found = violationsAfter((view) => {
      view.tasks = [view.tasks[0]!, task({ taskId: "a", deadLetteredAt: T1 })];
    });
    expect(found.join("\n")).toContain("half-written dead-letter mark");
  });

  it("catches a dead-letter on a task that never burned an attempt", () => {
    const found = violationsAfter((view) => {
      view.tasks = [
        view.tasks[0]!,
        task({
          taskId: "a",
          status: "failed",
          attemptCount: 0,
          startedAt: undefined,
          deadLetteredAt: T1,
          deadLetterReason: "poison",
        }),
      ];
      view.attempts = [view.attempts[1]!];
      view.observedLeases = [view.observedLeases[1]!];
    });
    expect(found.join("\n")).toContain("poison is earned by attempts");
  });

  it("catches a task that ran ahead of an unfinished dependency", () => {
    const found = violationsAfter((view) => {
      view.tasks = [
        view.tasks[0]!,
        task({ taskId: "a", status: "running", finishedAt: undefined }),
      ];
      view.attempts = [
        attempt({ attemptId: "att-a-1", taskId: "a", status: "running", endedAt: undefined }),
        view.attempts[1]!,
      ];
      view.liveLeases = [lease({ taskId: "a", observedAt: 1 })];
    });
    expect(found.join("\n")).toContain(
      "task b is completed but its dependency a is running",
    );
  });

  it("catches a task that ran ahead of a dead-lettered dependency", () => {
    const found = violationsAfter((view) => {
      view.tasks = [
        view.tasks[0]!,
        task({ taskId: "a", deadLetteredAt: T1, deadLetterReason: "poison" }),
      ];
    });
    expect(found.join("\n")).toContain("was dead-lettered");
  });

  it("does NOT complain about a settled task whose dependency never finished", () => {
    // The settle path is the store doing its job — `failed` and `cancelled`
    // dependents are allowed to have an unfinished dependency, and a checker
    // that flagged them would make every dependency-failure test red.
    const found = violationsAfter((view) => {
      view.tasks = [
        task({ taskId: "b", dependsOn: ["a"], parentTaskId: "a", status: "failed", attemptCount: 0, startedAt: undefined, error: "dependency_failed: a" }),
        task({ taskId: "a", status: "failed" }),
      ];
      view.events = new Map([["a", [event("a", 0)]], ["b", []]]);
      view.attempts = [attempt({ attemptId: "att-a-1", taskId: "a", status: "failed" })];
      view.observedLeases = [lease({ taskId: "a", observedAt: 1 })];
    });
    expect(found).toEqual([]);
  });

  it("catches attempt numbers with a hole in them", () => {
    const found = violationsAfter((view) => {
      view.tasks = [view.tasks[0]!, task({ taskId: "a", attemptCount: 2 })];
      view.attempts = [
        attempt({ attemptId: "att-a-1", taskId: "a", attemptNumber: 1 }),
        attempt({ attemptId: "att-a-3", taskId: "a", attemptNumber: 3 }),
        view.attempts[1]!,
      ];
    });
    expect(found.join("\n")).toContain("expected [1, 2]");
  });

  it("catches attemptCount disagreeing with the attempt rows", () => {
    const found = violationsAfter((view) => {
      view.tasks = [view.tasks[0]!, task({ taskId: "a", attemptCount: 4 })];
    });
    expect(found.join("\n")).toContain("but 1 attempt rows exist");
  });

  it("catches a queued task that somehow has an attempt", () => {
    const found = violationsAfter((view) => {
      view.tasks = [
        view.tasks[0]!,
        task({ taskId: "a", status: "queued", finishedAt: undefined, attemptCount: 0, startedAt: undefined }),
      ];
    });
    expect(found.join("\n")).toContain("a queued task was never claimed");
  });

  it("catches listChildren disagreeing with parentTaskId", () => {
    const found = violationsAfter((view) => {
      view.children = new Map([
        ["a", []],
        ["b", []],
      ]);
    });
    expect(found.join("\n")).toContain("does not include it");
  });

  it("catches a snapshot that calls itself quiescent while work is in flight", () => {
    const found = violationsAfter((view) => {
      view.inFlightTaskIds = new Set(["a"]);
    });
    expect(found.join("\n")).toContain("claims quiescence");
  });

  it("stands down on the cross-read checks in the in-flight phase", () => {
    // Not leniency for its own sake: a mid-run view is assembled over dozens of
    // awaits, so `attemptCount` and the attempt dump come from different
    // moments. Grading that pairing would report the reader's own lag.
    const view = cleanView();
    view.tasks = [view.tasks[0]!, task({ taskId: "a", attemptCount: 4 })];
    view.inFlightTaskIds = new Set(["a"]);
    expect(checkTaskInvariants(view, { phase: "in-flight" })).toEqual([]);
    expect(
      checkTaskInvariants(view, { phase: "quiescent" }).join("\n"),
    ).toContain("but 1 attempt rows exist");
  });

  it("still grades fencing, seq and dependencies in the in-flight phase", () => {
    // The other half of the split: the tear-proof invariants must NOT be
    // waived, or a mid-run spot check would be decoration.
    const view = cleanView();
    view.observedLeases = [
      lease({ taskId: "a", observedAt: 1, fencingToken: 5 }),
      lease({ taskId: "a", observedAt: 2, fencingToken: 2 }),
    ];
    view.events = new Map(view.events).set("b", [event("b", 0), event("b", 3)]);
    const found = checkTaskInvariants(view, { phase: "in-flight" }).join("\n");
    expect(found).toContain("it must strictly increase");
    expect(found).toContain("not gapless from 0");
  });

  it("skips the completeness-dependent checks without an adapter dump", () => {
    // A driver that only observed its own attempts cannot prove a task has no
    // OTHER running attempt, so the checker must not pretend otherwise.
    const view = cleanView();
    view.attemptsAreComplete = false;
    view.attempts = [];
    view.tasks = [view.tasks[0]!, task({ taskId: "a", attemptCount: 4 })];
    expect(checkTaskInvariants(view, { phase: "quiescent" })).toEqual([]);
  });
});

describe("snapshotTaskInvariants", () => {
  it("reads newest first and takes the dumps last", async () => {
    // Both orderings are load-bearing (see the function's doc): dependents
    // before dependencies, and every read before the attempt dump.
    const order: string[] = [];
    const view = await snapshotTaskInvariants({
      reader: {
        getTask: async (taskId) => {
          order.push(`get:${taskId}`);
          return task({ taskId });
        },
        listEvents: async (taskId) => {
          order.push(`events:${taskId}`);
          return [event(taskId, 0)];
        },
        listChildren: async () => [],
      },
      taskIds: ["a", "b", "c"],
      dumpAttempts: () => {
        order.push("dump");
        return [];
      },
    });
    expect(order).toEqual([
      "get:c",
      "events:c",
      "get:b",
      "events:b",
      "get:a",
      "events:a",
      "dump",
    ]);
    expect(view.tasks.map((t) => t.taskId)).toEqual(["c", "b", "a"]);
    expect(view.attemptsAreComplete).toBe(true);
  });

  it("reports no dump when the adapter cannot supply one", async () => {
    const view = await snapshotTaskInvariants({
      reader: {
        getTask: async (taskId) => task({ taskId }),
        listEvents: async () => [],
        listChildren: async () => [],
      },
      taskIds: ["a"],
    });
    expect(view.attemptsAreComplete).toBe(false);
    expect(view.liveLeases).toBeUndefined();
  });

  it("drops a task that is not in the store", async () => {
    const view = await snapshotTaskInvariants({
      reader: {
        getTask: async (taskId) => (taskId === "gone" ? null : task({ taskId })),
        listEvents: async () => [],
        listChildren: async () => [],
      },
      taskIds: ["a", "gone"],
    });
    expect(view.tasks.map((t) => t.taskId)).toEqual(["a"]);
  });
});

describe("mulberry32", () => {
  it("is a pure function of its seed", () => {
    const first = Array.from({ length: 8 }, mulberry32(4242));
    const draw = mulberry32(4242);
    expect(Array.from({ length: 8 }, () => draw())).toEqual(
      Array.from({ length: 8 }, mulberry32(4242)),
    );
    expect(first.length).toBe(8);
  });

  it("gives different seeds different streams, all inside [0, 1)", () => {
    const a = Array.from({ length: 64 }, mulberry32(1));
    const b = Array.from({ length: 64 }, mulberry32(2));
    expect(a).not.toEqual(b);
    expect(a.every((n) => n >= 0 && n < 1)).toBe(true);
    // A generator that collapsed to a constant would still satisfy the bounds.
    expect(new Set(a).size).toBeGreaterThan(50);
  });
});
