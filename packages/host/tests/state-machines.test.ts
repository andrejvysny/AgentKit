import { describe, expect, it } from "bun:test";
import {
  PROPOSAL_STATUSES,
  PROPOSAL_TRANSITIONS,
  assertProposalTransition,
  isProposalTransitionAllowed,
} from "../src/proposals/state-machine.js";
import {
  RUN_TRANSITIONS,
  assertRunTransition,
  isRunTransitionAllowed,
  type RunStatus,
} from "../src/ports/run-store.js";
import {
  InvalidProposalTransitionError,
  InvalidRunTransitionError,
} from "../src/errors.js";
import type { ProposalStatus } from "../src/ports/proposal-store.js";

const RUN_STATUSES: RunStatus[] = [
  "queued",
  "running",
  "waiting_approval",
  "completed",
  "failed",
  "cancelled",
];

describe("PROPOSAL_TRANSITIONS — exhaustive matrix", () => {
  it("accepts every legal pair and rejects every other pair", () => {
    let legal = 0;
    let illegal = 0;
    for (const from of PROPOSAL_STATUSES) {
      for (const to of PROPOSAL_STATUSES) {
        const allowed = PROPOSAL_TRANSITIONS[from].includes(to);
        if (allowed) {
          legal++;
          expect(isProposalTransitionAllowed(from, to)).toBe(true);
          expect(() => assertProposalTransition(from, to)).not.toThrow();
        } else {
          illegal++;
          expect(isProposalTransitionAllowed(from, to)).toBe(false);
          expect(() => assertProposalTransition(from, to)).toThrow(
            InvalidProposalTransitionError,
          );
        }
      }
    }
    // 7 statuses → 49 pairs; the table declares 6 legal ones.
    expect(legal).toBe(6);
    expect(illegal).toBe(43);
  });

  it("declares exactly the documented edges", () => {
    expect(PROPOSAL_TRANSITIONS.pending).toEqual([
      "approved",
      "rejected",
      "invalidated",
    ]);
    expect(PROPOSAL_TRANSITIONS.approved).toEqual(["applying"]);
    expect(PROPOSAL_TRANSITIONS.applying).toEqual(["applied", "failed"]);
    for (const terminal of [
      "applied",
      "failed",
      "rejected",
      "invalidated",
    ] as ProposalStatus[]) {
      expect(PROPOSAL_TRANSITIONS[terminal]).toEqual([]);
    }
  });

  it("forbids same-state transitions — re-entering `applying` would re-apply", () => {
    for (const status of PROPOSAL_STATUSES) {
      expect(isProposalTransitionAllowed(status, status)).toBe(false);
    }
  });

  it("carries the offending pair on the thrown error", () => {
    try {
      assertProposalTransition("applied", "applying");
      throw new Error("expected a throw");
    } catch (err) {
      expect(err).toBeInstanceOf(InvalidProposalTransitionError);
      const typed = err as InvalidProposalTransitionError;
      expect(typed.code).toBe("invalid_proposal_transition");
      expect(typed.details).toEqual({
        from: "applied",
        to: "applying",
        allowed: [],
      });
    }
  });

  it("freezes the table so a consumer cannot widen it at runtime", () => {
    expect(Object.isFrozen(PROPOSAL_TRANSITIONS)).toBe(true);
    expect(Object.isFrozen(PROPOSAL_TRANSITIONS.pending)).toBe(true);
    expect(() => {
      (PROPOSAL_TRANSITIONS as Record<string, unknown>).applied = ["applying"];
    }).toThrow();
  });
});

describe("RUN_TRANSITIONS — exhaustive matrix", () => {
  it("accepts every legal pair and rejects every other pair", () => {
    let legal = 0;
    for (const from of RUN_STATUSES) {
      for (const to of RUN_STATUSES) {
        const allowed = RUN_TRANSITIONS[from].includes(to);
        if (allowed) {
          legal++;
          expect(isRunTransitionAllowed(from, to)).toBe(true);
          expect(() => assertRunTransition(from, to)).not.toThrow();
        } else {
          expect(isRunTransitionAllowed(from, to)).toBe(false);
          expect(() => assertRunTransition(from, to)).toThrow(
            InvalidRunTransitionError,
          );
        }
      }
    }
    // queued(2) + running(4) + waiting_approval(4) = 10 legal of 36 pairs.
    expect(legal).toBe(10);
  });

  it("declares exactly the documented edges", () => {
    expect(RUN_TRANSITIONS.queued).toEqual(["running", "cancelled"]);
    expect(RUN_TRANSITIONS.running).toEqual([
      "waiting_approval",
      "completed",
      "failed",
      "cancelled",
    ]);
    expect(RUN_TRANSITIONS.waiting_approval).toEqual([
      "running",
      "completed",
      "failed",
      "cancelled",
    ]);
    for (const terminal of ["completed", "failed", "cancelled"] as RunStatus[]) {
      expect(RUN_TRANSITIONS[terminal]).toEqual([]);
    }
  });

  it("forbids same-state transitions — two workers must not both 'start' a run", () => {
    for (const status of RUN_STATUSES) {
      expect(isRunTransitionAllowed(status, status)).toBe(false);
    }
  });

  it("freezes the table", () => {
    expect(Object.isFrozen(RUN_TRANSITIONS)).toBe(true);
    expect(Object.isFrozen(RUN_TRANSITIONS.running)).toBe(true);
  });
});
