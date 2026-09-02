/**
 * The runner's housekeeping, as opposed to its lifecycle: the claim filter a
 * multi-pool deployment needs, the queue-position bookkeeping that has to stay
 * bounded, what a recovered-and-abandoned attempt records about itself, and what
 * happens to an execution recovery supersedes.
 *
 * Same two rules as the main suite — time is injected, real waiting stays tiny.
 */
import { afterEach, describe, expect, it } from "bun:test";
import { CHAT_TURN_TASK_KIND } from "@agentkit/host";
import {
  createHarness,
  settle,
  waitFor,
  type Harness,
} from "./support/task-runner-harness.js";

const started: Array<{ harness: Harness; stop: () => Promise<void> }> = [];

afterEach(async () => {
  for (const entry of started.splice(0)) {
    entry.harness.worker.releaseAll();
    await entry.stop();
  }
});

async function start(
  harness: Harness,
  opts: { concurrency?: number; kinds?: string[] } = {},
): Promise<void> {
  const handle = await harness.runner.startWorker(harness.worker, {
    concurrency: opts.concurrency ?? 2,
    ownerId: "owner-1",
    ...(opts.kinds === undefined ? {} : { kinds: opts.kinds }),
  });
  started.push({ harness, stop: handle.stop });
}

describe("SingleProcessTaskRunner — StartWorkerOptions.kinds", () => {
  it("claims only the kinds this worker was started for", async () => {
    // The store has always been able to filter a claim by kind; nothing could
    // ASK it to, so the documented multi-pool deployment was unreachable and a
    // worker claimed work it had no executor for.
    const harness = createHarness();
    await harness.store.tasks.createTask({
      taskId: "turn-1",
      kind: CHAT_TURN_TASK_KIND,
      scopeId: "chat-1",
      payload: { chatId: "chat-1" },
    });
    await harness.store.tasks.createTask({
      taskId: "embed-1",
      kind: "index.embed",
      scopeId: "doc-1",
      payload: {},
    });
    await start(harness, { kinds: [CHAT_TURN_TASK_KIND] });

    await harness.runner.enqueue({ taskId: "turn-1", scopeId: "chat-1" });
    await harness.runner.enqueue({ taskId: "embed-1", scopeId: "doc-1" });
    await waitFor(
      async () =>
        (await harness.store.tasks.getTask("turn-1"))?.status === "completed",
      "the chat turn to run",
    );

    // Several poll cycles later the other pool's work is still untouched —
    // waiting for the process that registered an executor for it.
    await settle();
    expect((await harness.store.tasks.getTask("embed-1"))?.status).toBe(
      "queued",
    );
    expect(harness.worker.callsFor("embed-1").length).toBe(0);
  });
});

describe("SingleProcessTaskRunner — scope-lock bookkeeping", () => {
  it("drops a waiter when an enqueue finds its task is no longer queued", async () => {
    // `enqueue` records a queue position when the scope is busy. Nothing ever
    // removed it if the task was claimed by ANOTHER process (or cancelled
    // elsewhere), so `waitingByScope` grew for the life of the process.
    const harness = createHarness();
    await harness.seedTask("run-1", "chat-1");
    await harness.seedTask("run-2", "chat-1");
    harness.worker.script("run-1", [{ kind: "hold" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "run-1 to take the scope",
    );
    await harness.runner.enqueue({ taskId: "run-2", scopeId: "chat-1" });
    expect(harness.runner.scopeLock.waiting("chat-1")).toEqual(["run-2"]);

    // Somebody else settled it — here, a cancel; in a deployment, another
    // process's claim.
    await harness.store.tasks.transitionTask(
      "run-2",
      ["queued"],
      "cancelled",
      {},
    );
    await harness.runner.enqueue({ taskId: "run-2", scopeId: "chat-1" });
    expect(harness.runner.scopeLock.waiting("chat-1")).toEqual([]);
  });

  it("drops a waiter when a cancel arrives for a task that is already terminal", async () => {
    const harness = createHarness();
    await harness.seedTask("run-1", "chat-1");
    await harness.seedTask("run-2", "chat-1");
    harness.worker.script("run-1", [{ kind: "hold" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "run-1 to take the scope",
    );
    await harness.runner.enqueue({ taskId: "run-2", scopeId: "chat-1" });
    expect(harness.runner.scopeLock.waiting("chat-1")).toEqual(["run-2"]);

    await harness.store.tasks.transitionTask(
      "run-2",
      ["queued"],
      "cancelled",
      {},
    );
    // The cancel path's terminal fall-through: nothing to stop, but the place
    // in line is still there to clean up.
    await harness.runner.requestCancel("run-2");
    expect(harness.runner.scopeLock.waiting("chat-1")).toEqual([]);
  });
});

describe("SingleProcessTaskRunner — recovery bookkeeping", () => {
  it("records the abandoned attempt in poisonCount when it dead-letters", async () => {
    // `poisonCount` was minted, exposed over REST as "the dead-letter trigger",
    // and never written by anything. It rides the transition that lands the
    // task, because TASK_TRANSITIONS has no `running -> running` edge to carry
    // an increment while the task is still alive.
    const harness = createHarness({ leaseTtlMs: 1_000, maxAttempts: 1 });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "never" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "run-1 to start",
    );

    harness.clock.advance(1_500);
    const report = await harness.runner.recoverWithReport();
    expect(report.deadLettered).toBe(1);

    const run = await harness.store.tasks.getTask("run-1");
    expect(run?.status).toBe("failed");
    expect(run?.poisonCount).toBe(1);
    // The clean counter still counts attempts, not deaths — the two answer
    // different questions and are read by different code.
    expect(run?.attemptCount).toBe(1);

    harness.worker.releaseAll();
  });

  it("aborts the execution it supersedes instead of leaving it running", async () => {
    // Recovery re-dispatching a task whose local execution is still hanging
    // creates two executions of one task in one process. The store fences the
    // old one out of its own writes — but not out of the conversation records
    // it is mid-stream on, nor out of the provider call it is paying for. The
    // signal is what actually ends it.
    const harness = createHarness({ leaseTtlMs: 1_000, maxAttempts: 3 });
    await harness.seedTask("run-1");
    harness.worker.script("run-1", [{ kind: "await-abort" }, { kind: "hold" }]);
    await start(harness);

    await harness.runner.enqueue({ taskId: "run-1", scopeId: "chat-1" });
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 1,
      "the first execution to start",
    );
    // Parked on its signal: nothing has unwound yet.
    expect(harness.worker.timeline).toEqual(["start:run-1"]);

    harness.clock.advance(1_500);
    await harness.runner.recover();
    await waitFor(
      () => harness.worker.callsFor("run-1").length === 2,
      "a second attempt after recovery",
    );

    // THE ASSERTION: the superseded execution came back — it can only have been
    // the abort, because its `hold`-scripted successor is still blocked.
    await waitFor(
      () => harness.worker.timeline.includes("end:run-1"),
      "the superseded execution to unwind",
    );

    harness.worker.releaseAll();
  });
});
