export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };
export type CaseName =
  | "safe_retry"
  | "uncertain_write"
  | "cancel_after_commit"
  | "stale_publication"
  | "verification"
  | "crash_recovery";
export interface RunnerCase {
  name: CaseName;
  schedule: string[];
  delta?: string;
  expected: { typescript: Json[]; rust: Json[] };
}
export interface RunnerVectors {
  formatVersion: 1;
  startMs: number;
  leaseTtlMs: number;
  retryBaseMs: number;
  scenarios: RunnerCase[];
}

const event = (seq: number, type: string, data: Json): Json => ({
  seq,
  type,
  data,
});
const started = (seq: number, attempt: number): Json =>
  event(seq, "fault.started", { attempt });
const committed = (seq: number, effect: number): Json =>
  event(seq, "fault.committed", { effect });
const point = (
  label: string,
  status: string,
  attempts: string[],
  effects: number,
  events: Json[],
  extra: Record<string, Json> = {},
): Json => ({
  label,
  status,
  attemptCount: attempts.length,
  attempts,
  effects,
  events,
  poisonCount: 0,
  deadLetter: false,
  failureReason: null,
  ...extra,
});
const same = (
  name: CaseName,
  schedule: string[],
  expected: Json[],
): RunnerCase => ({
  name,
  schedule,
  expected: { typescript: expected, rust: expected },
});

const retryEvents = [started(0, 1), started(1, 2), committed(2, 1)];
const retryGap = (durableRetryDue: Json): Json =>
  point("backoff", "running", ["failed"], 0, [started(0, 1)], {
    retryDue: 1200,
    durableRetryDue,
  });
const retryEarly = point("before_due", "running", ["failed"], 0, [
  started(0, 1),
]);
const retryDone = point(
  "settled",
  "completed",
  ["failed", "completed"],
  1,
  retryEvents,
);
const uncertainTs = point("settled", "completed", ["failed", "completed"], 2, [
  started(0, 1),
  committed(1, 1),
  started(2, 2),
  committed(3, 2),
]);
const uncertainRust = point(
  "settled",
  "failed",
  ["failed"],
  1,
  [started(0, 1), committed(1, 1)],
  { failureReason: "uncertain_outcome" },
);
const crashPoint = (label: string): Json =>
  point(label, "interrupted", ["failed"], 0, [started(0, 1)]);
const scenarios: RunnerCase[] = [
  {
    name: "safe_retry",
    schedule: [
      "execute at 1000; fail before dispatch with retryable=true",
      "observe retry deadline 1200",
      "advance to 1199; assert no dispatch",
      "advance to 1200; complete",
    ],
    delta:
      "TS stores the backoff deadline in runner memory and retains a live lease; Rust persists the deadline before releasing the attempt lease. The observable due time and dispatch trace agree.",
    expected: {
      typescript: [retryGap(null), retryEarly, retryDone],
      rust: [retryGap(1200), retryEarly, retryDone],
    },
  },
  {
    name: "uncertain_write",
    schedule: [
      "commit effect and evidence at 1000",
      "throw raw ECONNRESET after commit",
      "advance to 1200",
    ],
    delta:
      "Approved safety correction: frozen TS classifies raw ECONNRESET as transient and repeats the committed write; Rust requires explicit safe-to-retry evidence and fails uncertain_outcome without replay.",
    expected: { typescript: [uncertainTs], rust: [uncertainRust] },
  },
  same(
    "cancel_after_commit",
    [
      "commit external effect; hold result publication",
      "cancel running task",
      "publish committed evidence and acknowledge cancellation",
    ],
    [
      point("committed_before_cancel", "running", ["running"], 1, [
        started(0, 1),
      ]),
      point("settled", "cancelled", ["cancelled"], 1, [
        started(0, 1),
        committed(1, 1),
      ]),
    ],
  ),
  same(
    "stale_publication",
    [
      "start worker and hold publication",
      "mint replacement lease",
      "release stale worker; deny its event and terminal verdict",
    ],
    [
      point("stale_denied", "running", ["running"], 0, [started(0, 1)], {
        staleDenied: true,
      }),
    ],
  ),
  same(
    "verification",
    [
      "commit effect and event",
      "verify durable event while task is running",
      "append verification before runner settlement",
    ],
    [
      point(
        "settled",
        "completed",
        ["completed"],
        1,
        [
          committed(0, 1),
          event(1, "run.verification", {
            pass: 0,
            status: "pass",
            deficiencies: [],
          }),
        ],
        {
          verification: {
            status: "running",
            eventTypes: ["fault.committed"],
          },
        },
      ),
    ],
  ),
  same(
    "crash_recovery",
    [
      "child process executes safe failed attempt at 1000",
      "SIGKILL after retry intent is observable",
      "reopen at expiry 2000; manual recover",
      "reopen at 2100; recover twice without redispatch",
      "explicit resume at 2200; execute next attempt",
    ],
    [
      crashPoint("recovered"),
      crashPoint("reopened_again"),
      crashPoint("recovered_again"),
      point("settled", "completed", ["failed", "completed"], 1, retryEvents),
    ],
  ),
];

/** Reviewed expected observations, not a substitute implementation of either runner. */
export function buildRunnerVectors(): RunnerVectors {
  return {
    formatVersion: 1,
    startMs: 1000,
    leaseTtlMs: 1000,
    retryBaseMs: 200,
    scenarios: structuredClone(scenarios),
  };
}
