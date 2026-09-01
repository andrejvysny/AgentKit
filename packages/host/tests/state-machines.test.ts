import { describe, expect, it } from "bun:test";
import {
  PROPOSAL_STATUSES,
  PROPOSAL_TRANSITIONS,
  assertProposalTransition,
  isProposalTransitionAllowed,
} from "../src/proposals/state-machine.js";
import {
  TASK_TRANSITIONS,
  assertTaskTransition,
  isTaskTransitionAllowed,
  type TaskStatus,
} from "../src/ports/task-store.js";
import {
  InvalidProposalTransitionError,
  InvalidTaskTransitionError,
} from "../src/errors.js";
import type { ProposalStatus } from "../src/ports/proposal-store.js";
import type { ToolCallingMode } from "../src/ports/settings-store.js";
import type { WritePolicyMode } from "../src/ports/write-policy.js";
import type {
  RunStatusDto,
  ToolCallingModeDto,
  WritePolicyModeDto,
} from "@agentkit/contracts";

const TASK_STATUSES: TaskStatus[] = [
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

describe("TASK_TRANSITIONS — exhaustive matrix", () => {
  it("accepts every legal pair and rejects every other pair", () => {
    let legal = 0;
    for (const from of TASK_STATUSES) {
      for (const to of TASK_STATUSES) {
        const allowed = TASK_TRANSITIONS[from].includes(to);
        if (allowed) {
          legal++;
          expect(isTaskTransitionAllowed(from, to)).toBe(true);
          expect(() => assertTaskTransition(from, to)).not.toThrow();
        } else {
          expect(isTaskTransitionAllowed(from, to)).toBe(false);
          expect(() => assertTaskTransition(from, to)).toThrow(
            InvalidTaskTransitionError,
          );
        }
      }
    }
    // queued(3) + running(4) + waiting_approval(4) = 11 legal of 36 pairs.
    // queued's third edge is `failed`, which exists only for the dependency
    // cascade in `claimNext` — see the table's own comment.
    expect(legal).toBe(11);
  });

  it("declares exactly the documented edges", () => {
    expect(TASK_TRANSITIONS.queued).toEqual(["running", "cancelled", "failed"]);
    expect(TASK_TRANSITIONS.running).toEqual([
      "waiting_approval",
      "completed",
      "failed",
      "cancelled",
    ]);
    expect(TASK_TRANSITIONS.waiting_approval).toEqual([
      "running",
      "completed",
      "failed",
      "cancelled",
    ]);
    for (const terminal of [
      "completed",
      "failed",
      "cancelled",
    ] as TaskStatus[]) {
      expect(TASK_TRANSITIONS[terminal]).toEqual([]);
    }
  });

  it("forbids same-state transitions — two workers must not both 'start' a task", () => {
    for (const status of TASK_STATUSES) {
      expect(isTaskTransitionAllowed(status, status)).toBe(false);
    }
  });

  it("freezes the table", () => {
    expect(Object.isFrozen(TASK_TRANSITIONS)).toBe(true);
    expect(Object.isFrozen(TASK_TRANSITIONS.running)).toBe(true);
  });
});

/**
 * `RunStatusDto` (contracts) and {@link TaskStatus} (host) are the same
 * enumeration written down twice: contracts sits BELOW host and cannot import
 * it, so `rest.ts` restates the union and points at this file for the check
 * that keeps the two honest.
 *
 * The check is the two assignments below, and it has to run in BOTH directions.
 * One direction alone only proves one union is a subset of the other, so a
 * status added on either side would slip through unnoticed — and the failure it
 * would cause is a serialized value no client's type ever admitted.
 */
describe("RunStatusDto ↔ TaskStatus — the mirrored enumeration", () => {
  it("is the same set of members in both directions, at compile time", () => {
    const hostStatuses: TaskStatus[] = TASK_STATUSES;
    const dtoStatuses: RunStatusDto[] = hostStatuses;
    const backToHost: TaskStatus[] = dtoStatuses;
    // The assignments above ARE the assertion; this keeps the test honest
    // about having executed, and pins the member count so an addition on
    // either side has to be made deliberately.
    expect(backToHost).toHaveLength(6);
    expect(new Set(backToHost).size).toBe(6);
  });
});

/**
 * The other two host unions `rest.ts` restates, checked the same way and for
 * the same reason: contracts cannot import host, so the only thing keeping the
 * copies honest is a bidirectional assignment that stops compiling the moment
 * either side gains a member.
 */
describe("WritePolicyModeDto / ToolCallingModeDto ↔ host — the mirrored enumerations", () => {
  it("write policy modes are the same set in both directions", () => {
    const hostModes: WritePolicyMode[] = [
      "auto_readonly_confirm_writes",
      "confirm_all_writes",
      "auto_all",
    ];
    const dtoModes: WritePolicyModeDto[] = hostModes;
    const backToHost: WritePolicyMode[] = dtoModes;
    expect(new Set(backToHost).size).toBe(3);
  });

  it("tool-calling modes are the same set in both directions", () => {
    const hostModes: ToolCallingMode[] = ["auto", "on", "off"];
    const dtoModes: ToolCallingModeDto[] = hostModes;
    const backToHost: ToolCallingMode[] = dtoModes;
    expect(new Set(backToHost).size).toBe(3);
  });
});
