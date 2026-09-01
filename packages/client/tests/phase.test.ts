/**
 * `runPhase`, as a table: every combination of status and log a UI can hold.
 */
import { describe, expect, test } from "bun:test";
import {
  CONTRACT_VERSION,
  type AiRunEvent,
  type RunStatusDto,
} from "@agentkit/contracts";
import { isTerminalRunEvent, runPhase, type RunPhase } from "../src/index.js";

let seq = 0;

function event(
  type: AiRunEvent["type"],
  data: Record<string, unknown> = {},
): AiRunEvent {
  seq += 1;
  return {
    type,
    runId: "run-1",
    timestamp: new Date(seq * 1000).toISOString(),
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-${seq}`,
    seq,
    data,
  } as AiRunEvent;
}

const started = () => event("run.started", { model: "m", toolCount: 0 });
const delta = () => event("run.message.delta", { delta: "tok" });
const usage = () =>
  event("run.usage", {
    callId: "c",
    attempt: 1,
    step: 0,
    model: "m",
    source: "stream",
    finalForCall: true,
  });
const completed = () => event("run.completed", { iterations: 1 });
const failed = () => event("run.failed", { errorMessage: "boom" });
const cancelled = () => event("run.cancelled", {});

interface Row {
  name: string;
  status?: RunStatusDto;
  events?: AiRunEvent[];
  expected: RunPhase;
}

const TABLE: Row[] = [
  // --- status alone ---------------------------------------------------------
  { name: "queued, nothing on the log", status: "queued", expected: "queued" },
  {
    name: "running, nothing on the log yet — thinking, not typing",
    status: "running",
    expected: "running",
  },
  {
    name: "waiting_approval mirrors straight through",
    status: "waiting_approval",
    expected: "waiting_approval",
  },
  { name: "completed", status: "completed", expected: "completed" },
  { name: "failed", status: "failed", expected: "failed" },
  { name: "cancelled", status: "cancelled", expected: "cancelled" },

  // --- the derived phase ----------------------------------------------------
  {
    name: "running + run.started is streaming",
    status: "running",
    events: [started()],
    expected: "streaming",
  },
  {
    name: "running + a delta is streaming",
    status: "running",
    events: [started(), delta()],
    expected: "streaming",
  },
  {
    name: "a stale queued status still streams once the log has started",
    status: "queued",
    events: [started()],
    expected: "streaming",
  },
  {
    name: "an event that is not output does not make it streaming",
    status: "running",
    events: [usage()],
    expected: "running",
  },
  {
    name: "waiting_approval beats streaming — the user has to act",
    status: "waiting_approval",
    events: [started(), delta()],
    expected: "waiting_approval",
  },

  // --- terminal events win over the status ---------------------------------
  {
    name: "run.completed on the log beats a status that has not caught up",
    status: "running",
    events: [started(), delta(), completed()],
    expected: "completed",
  },
  {
    name: "run.failed on the log beats a running status",
    status: "running",
    events: [started(), failed()],
    expected: "failed",
  },
  {
    name: "run.cancelled on the log beats a running status",
    status: "running",
    events: [started(), cancelled()],
    expected: "cancelled",
  },
  {
    name: "a terminal status with no terminal event still terminates",
    status: "failed",
    events: [started(), delta()],
    expected: "failed",
  },

  // --- neither, or only one -------------------------------------------------
  { name: "nothing at all is queued", expected: "queued" },
  { name: "an empty event list is queued", events: [], expected: "queued" },
  {
    name: "events but no status: output means streaming",
    events: [started()],
    expected: "streaming",
  },
  {
    name: "events but no status and no output means running",
    events: [usage()],
    expected: "running",
  },
  {
    name: "a terminal event with no status at all",
    events: [completed()],
    expected: "completed",
  },
];

describe("runPhase", () => {
  for (const row of TABLE) {
    test(row.name, () => {
      expect(
        runPhase({
          ...(row.status === undefined ? {} : { status: row.status }),
          ...(row.events === undefined ? {} : { events: row.events }),
        }),
      ).toBe(row.expected);
    });
  }
});

describe("isTerminalRunEvent", () => {
  const terminal: AiRunEvent["type"][] = [
    "run.completed",
    "run.failed",
    "run.cancelled",
  ];
  const nonTerminal: AiRunEvent["type"][] = [
    "run.started",
    "run.message.delta",
    "run.message.completed",
    "run.tool.requested",
    "run.tool.running",
    "run.tool.succeeded",
    "run.tool.failed",
    "run.warning",
    "run.usage",
    // The one that can arrive AFTER the terminal event, and must not be
    // mistaken for one.
    "run.verification",
  ];

  for (const type of terminal) {
    test(`${type} is terminal`, () => {
      expect(isTerminalRunEvent({ type })).toBe(true);
    });
  }
  for (const type of nonTerminal) {
    test(`${type} is not terminal`, () => {
      expect(isTerminalRunEvent({ type })).toBe(false);
    });
  }
});
