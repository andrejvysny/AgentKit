/**
 * The retry taxonomy, asserted directly.
 *
 * The case that matters most is the last block: an unrecognised failure is
 * TERMINAL. task-system marked every non-abort failure retryable, so a bad API
 * key was retried until the attempt budget ran out; this suite is what stops
 * that from coming back by accident.
 */
import { describe, expect, it } from "bun:test";
import {
  DuplicateTaskError,
  ExecutorNotFoundError,
  InvalidTaskTransitionError,
  LeaseLostError,
  SeqConflictError,
} from "@agentkit/host";
import { classifyExecutionError } from "../src/index.js";

describe("classifyExecutionError — cancellation", () => {
  it("reads an AbortController's DOMException as cancelled", async () => {
    const controller = new AbortController();
    controller.abort();
    let thrown: unknown;
    try {
      controller.signal.throwIfAborted();
    } catch (err) {
      thrown = err;
    }
    expect(classifyExecutionError(thrown)).toEqual({
      kind: "cancelled",
      reason: "aborted",
    });
  });

  it("reads a hand-rolled AbortError as cancelled", () => {
    const err = new Error("The operation was aborted.");
    err.name = "AbortError";
    expect(classifyExecutionError(err).kind).toBe("cancelled");
  });

  it("reads a wrapped AbortError as cancelled", () => {
    const inner = new Error("aborted");
    inner.name = "AbortError";
    const err = new Error("provider call failed", { cause: inner });
    expect(classifyExecutionError(err).kind).toBe("cancelled");
  });
});

describe("classifyExecutionError — transient", () => {
  it("treats a lost lease as transient: the work never got a verdict", () => {
    expect(classifyExecutionError(new LeaseLostError("gone"))).toEqual({
      kind: "transient",
      reason: "lease_lost",
    });
  });

  it.each([
    "ECONNRESET",
    "ECONNREFUSED",
    "ETIMEDOUT",
    "EPIPE",
  ])("treats %s as transient", (code) => {
    const err = Object.assign(new Error(`connect ${code} 10.0.0.1:443`), {
      code,
    });
    expect(classifyExecutionError(err)).toEqual({
      kind: "transient",
      reason: `network:${code}`,
    });
  });

  it("treats an undici-style 'fetch failed' as transient", () => {
    expect(classifyExecutionError(new Error("fetch failed")).kind).toBe(
      "transient",
    );
  });

  it("treats 'socket hang up' as transient", () => {
    expect(classifyExecutionError(new Error("socket hang up")).kind).toBe(
      "transient",
    );
  });

  it("treats a 5xx status field as transient", () => {
    const err = Object.assign(new Error("upstream exploded"), { status: 503 });
    expect(classifyExecutionError(err)).toEqual({
      kind: "transient",
      reason: "http_503",
    });
  });

  it("treats a 5xx in the message as transient", () => {
    expect(classifyExecutionError(new Error("HTTP 502 Bad Gateway"))).toEqual({
      kind: "transient",
      reason: "http_5xx",
    });
  });

  it("treats 429 as transient, by field and by message", () => {
    const withField = Object.assign(new Error("slow down"), {
      response: { status: 429 },
    });
    expect(classifyExecutionError(withField).kind).toBe("transient");
    expect(classifyExecutionError(new Error("429 Too Many Requests"))).toEqual({
      kind: "transient",
      reason: "http_429",
    });
  });

  it("honours an explicit retryable flag over every heuristic below it", () => {
    const err = Object.assign(new Error("provider hiccup"), {
      retryable: true,
    });
    expect(classifyExecutionError(err)).toEqual({
      kind: "transient",
      reason: "retryable_flag",
    });
  });
});

describe("classifyExecutionError — terminal", () => {
  it("treats a seq conflict as terminal: retrying cannot fix a numbering bug", () => {
    expect(classifyExecutionError(new SeqConflictError("gap"))).toEqual({
      kind: "terminal",
      reason: "seq_conflict",
    });
  });

  it("treats the wiring failures as terminal, by code and not by message", () => {
    // A kind nobody registered, an id already taken, a payload the executor
    // cannot read: all three reproduce identically on every attempt.
    expect(
      classifyExecutionError(new ExecutorNotFoundError("no executor")),
    ).toEqual({ kind: "terminal", reason: "executor_not_found" });
    expect(classifyExecutionError(new DuplicateTaskError("taken"))).toEqual({
      kind: "terminal",
      reason: "duplicate_task",
    });
    expect(
      classifyExecutionError(new InvalidTaskTransitionError("illegal")),
    ).toEqual({ kind: "terminal", reason: "invalid_task_transition" });
  });

  it.each([401, 403, 400, 422])("treats HTTP %i as terminal", (status) => {
    const err = Object.assign(new Error("rejected"), { status });
    expect(classifyExecutionError(err)).toEqual({
      kind: "terminal",
      reason: `http_${status}`,
    });
  });

  it.each([
    "401 unauthorized: bad api key",
    "invalid request: model not found",
    "403 forbidden",
  ])("treats %p as terminal", (message) => {
    expect(classifyExecutionError(new Error(message)).kind).toBe("terminal");
  });

  it("honours an explicit retryable:false", () => {
    const err = Object.assign(new Error("ECONNRESET but do not retry"), {
      retryable: false,
    });
    expect(classifyExecutionError(err)).toEqual({
      kind: "terminal",
      reason: "non_retryable_flag",
    });
  });

  it("DEFAULTS an unrecognised failure to terminal", () => {
    // The task-system regression, pinned: no evidence of a transient fault
    // means no retry. A host that knows better sets `retryable`.
    expect(classifyExecutionError(new Error("kaboom"))).toEqual({
      kind: "terminal",
      reason: "unclassified",
    });
    expect(classifyExecutionError("a thrown string").kind).toBe("terminal");
    expect(classifyExecutionError(undefined).kind).toBe("terminal");
    expect(classifyExecutionError({ weird: true }).kind).toBe("terminal");
  });
});
