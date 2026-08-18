/**
 * {@link ScopeLock} on its own — including the three places it deliberately
 * behaves differently from the task-system lock it was salvaged from.
 */
import { describe, expect, it } from "bun:test";
import { ScopeLock } from "../src/index.js";

describe("ScopeLock", () => {
  it("gives the scope to the first run and queues the rest in FIFO order", () => {
    const lock = new ScopeLock();
    expect(lock.tryAcquire("chat-1", "run-1")).toBe(true);
    expect(lock.tryAcquire("chat-1", "run-2")).toBe(false);
    expect(lock.tryAcquire("chat-1", "run-3")).toBe(false);

    expect(lock.hasActive("chat-1")).toBe(true);
    expect(lock.activeRun("chat-1")).toBe("run-1");
    expect(lock.waiting("chat-1")).toEqual(["run-2", "run-3"]);
    expect(lock.busyScopes()).toEqual(["chat-1"]);
  });

  it("reports positions: 0 active, 1+ waiting, -1 unknown", () => {
    const lock = new ScopeLock();
    lock.tryAcquire("chat-1", "run-1");
    lock.tryAcquire("chat-1", "run-2");
    lock.tryAcquire("chat-1", "run-3");

    expect(lock.getPosition("chat-1", "run-1")).toBe(0);
    expect(lock.getPosition("chat-1", "run-2")).toBe(1);
    expect(lock.getPosition("chat-1", "run-3")).toBe(2);
    expect(lock.getPosition("chat-1", "run-9")).toBe(-1);
    expect(lock.getPosition("chat-9", "run-1")).toBe(-1);
  });

  it("does not queue a run behind itself (re-entrant for the holder)", () => {
    // The salvaged version pushed the active run into its own wait list, so a
    // recovery pass re-dispatching a still-active run made it wait on itself.
    const lock = new ScopeLock();
    expect(lock.tryAcquire("chat-1", "run-1")).toBe(true);
    expect(lock.tryAcquire("chat-1", "run-1")).toBe(true);
    expect(lock.waiting("chat-1")).toEqual([]);
    expect(lock.getPosition("chat-1", "run-1")).toBe(0);
  });

  it("queues a run once no matter how many times it is offered", () => {
    const lock = new ScopeLock();
    lock.tryAcquire("chat-1", "run-1");
    lock.tryAcquire("chat-1", "run-2");
    lock.tryAcquire("chat-1", "run-2");
    expect(lock.waiting("chat-1")).toEqual(["run-2"]);
  });

  it("releases to a FREE scope while naming the successor", () => {
    // The successor is returned for the caller to dispatch, but the scope is
    // left free: marking it busy for a run nobody has claimed yet would make
    // `claimNext` skip it forever.
    const lock = new ScopeLock();
    lock.tryAcquire("chat-1", "run-1");
    lock.tryAcquire("chat-1", "run-2");

    expect(lock.release("chat-1", "run-1")).toBe("run-2");
    expect(lock.hasActive("chat-1")).toBe(false);
    expect(lock.busyScopes()).toEqual([]);
    expect(lock.waiting("chat-1")).toEqual([]);
    // The successor takes the scope when it is actually dispatched.
    expect(lock.tryAcquire("chat-1", "run-2")).toBe(true);
  });

  it("returns null when there is nobody waiting", () => {
    const lock = new ScopeLock();
    lock.tryAcquire("chat-1", "run-1");
    expect(lock.release("chat-1", "run-1")).toBeNull();
    expect(lock.hasActive("chat-1")).toBe(false);
  });

  it("treats a release from a non-holder as leaving the line", () => {
    const lock = new ScopeLock();
    lock.tryAcquire("chat-1", "run-1");
    lock.tryAcquire("chat-1", "run-2");

    expect(lock.release("chat-1", "run-2")).toBeNull();
    expect(lock.activeRun("chat-1")).toBe("run-1");
    expect(lock.waiting("chat-1")).toEqual([]);
  });

  it("removes a run from wherever it is, without promoting a successor", () => {
    const lock = new ScopeLock();
    lock.tryAcquire("chat-1", "run-1");
    lock.tryAcquire("chat-1", "run-2");

    lock.remove("chat-1", "run-2");
    expect(lock.waiting("chat-1")).toEqual([]);
    expect(lock.activeRun("chat-1")).toBe("run-1");

    lock.remove("chat-1", "run-1");
    expect(lock.hasActive("chat-1")).toBe(false);
  });

  it("keeps scopes independent", () => {
    const lock = new ScopeLock();
    lock.tryAcquire("chat-1", "run-1");
    expect(lock.tryAcquire("chat-2", "run-2")).toBe(true);
    expect(lock.busyScopes().sort()).toEqual(["chat-1", "chat-2"]);

    lock.clear();
    expect(lock.busyScopes()).toEqual([]);
  });
});
