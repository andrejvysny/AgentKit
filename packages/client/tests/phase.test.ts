/**
 * `runPhase`, as a table: every combination of status and log a UI can hold.
 */
import { describe, expect, test } from "bun:test";
import {
  CONTRACT_VERSION,
  type AiRunEvent,
  type RunStatusDto,
} from "@agentkit/contracts";
import {
  createRunPhaseTracker,
  isTerminalRunEvent,
  runPhase,
  type RunPhase,
} from "../src/index.js";

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
/** The host's pass boundary: everything before it belonged to the last pass. */
const retryPass = () =>
  event("run.warning", {
    code: "retry_pass",
    message: "Retrying without tools.",
    pass: 2,
    reason: "chat_only",
  });
const otherWarning = () =>
  event("run.warning", { code: "truncated", message: "cut off" });
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

  // --- provider-pass completion waits for durable task settlement ----------
  {
    name: "run.completed while the task is running is settling",
    status: "running",
    events: [started(), delta(), completed()],
    expected: "settling",
  },
  {
    name: "run.failed while the task is running is settling",
    status: "running",
    events: [started(), failed()],
    expected: "settling",
  },
  {
    name: "run.cancelled while the task is running is settling",
    status: "running",
    events: [started(), cancelled()],
    expected: "settling",
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
    expected: "settling",
  },

  // --- multi-pass: the LAST terminal wins ----------------------------------
  {
    name: "a failed pass the host retried into a completed one is completed",
    status: "completed",
    events: [
      started(),
      delta(),
      failed(),
      retryPass(),
      started(),
      delta(),
      completed(),
    ],
    expected: "completed",
  },
  {
    name: "a run typing its second pass is streaming, not the first's failure",
    status: "running",
    events: [started(), delta(), failed(), retryPass(), started(), delta()],
    expected: "streaming",
  },
  {
    name: "a second run.started is a boundary on its own — older hosts emit no warning",
    status: "running",
    events: [started(), delta(), failed(), started(), delta()],
    expected: "streaming",
  },
  {
    name: "the LAST pass's failure still awaits task settlement",
    status: "running",
    events: [started(), failed(), retryPass(), started(), failed()],
    expected: "settling",
  },
  {
    name: "a warning that is not retry_pass moves nothing",
    status: "running",
    events: [started(), delta(), completed(), otherWarning()],
    expected: "settling",
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

/**
 * The tracker is the incremental form of the same function, so the assertion
 * that matters is AGREEMENT: fold a log one event at a time and every prefix
 * must answer what `runPhase` answers for that prefix. Anything else is two
 * definitions of a phase, which is the thing having one function prevented.
 */
describe("createRunPhaseTracker", () => {
  const verification = () =>
    event("run.verification", { verdict: "ok", passes: 1 });

  test("every prefix of a log agrees with runPhase over that prefix", () => {
    const log = [started(), delta(), usage(), delta(), completed()];
    const tracker = createRunPhaseTracker();
    expect(tracker.phase()).toBe("queued");

    const folded = log.map((e) => tracker.observe(e));
    expect(folded).toEqual(
      log.map((_e, i) => runPhase({ events: log.slice(0, i + 1) })),
    );
    expect(tracker.phase()).toBe("settling");
    expect(tracker.outcome()).toBe("completed");
  });

  test("output alone is streaming; anything else is only running", () => {
    const running = createRunPhaseTracker();
    expect(running.observe(usage())).toBe("running");
    const streaming = createRunPhaseTracker();
    expect(streaming.observe(started())).toBe("streaming");
  });

  test("every prefix of a MULTI-PASS log agrees with runPhase too", () => {
    const log = [
      started(),
      delta(),
      failed(),
      retryPass(),
      started(),
      delta(),
      completed(),
    ];
    const tracker = createRunPhaseTracker();
    const folded = log.map((e) => tracker.observe(e));
    expect(folded).toEqual(
      log.map((_e, i) => runPhase({ events: log.slice(0, i + 1) })),
    );
    // Spelled out, because this is the sequence the bug lived in: the failure
    // stands until the boundary, and the run is live again after it.
    expect(folded).toEqual([
      "streaming",
      "streaming",
      "settling",
      "streaming",
      "streaming",
      "streaming",
      "settling",
    ]);
  });

  test("startedNewPass marks the boundary, and only the boundary", () => {
    const log = [
      started(),
      delta(),
      failed(),
      retryPass(),
      started(),
      otherWarning(),
      completed(),
    ];
    const tracker = createRunPhaseTracker();
    const boundaries = log.map((e) => {
      tracker.observe(e);
      return tracker.startedNewPass();
    });
    // The warning opens pass 2; the `run.started` that follows it is the same
    // boundary reported twice, which is what a consumer resetting its streamed
    // text wants — the reset is idempotent, and a host that emits no warning
    // still gets one.
    expect(boundaries).toEqual([false, false, false, true, true, false, false]);
  });

  test("a terminal event is the last word, whatever the log carries after it", () => {
    for (const [terminal, expected] of [
      [failed(), "failed"],
      [cancelled(), "cancelled"],
    ] as const) {
      const tracker = createRunPhaseTracker();
      tracker.observe(started());
      tracker.observe(terminal);
      // `run.verification` is appended AFTER the terminal event by the host's
      // correction harness; it must not move the phase back off it.
      tracker.observe(verification());
      expect(tracker.phase()).toBe("settling");
      expect(tracker.outcome()).toBe(expected);
    }
  });
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
