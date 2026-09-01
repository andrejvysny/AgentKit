/**
 * `runPhase` — the one thing every consumer of this API writes for itself, in
 * one place instead of three.
 *
 * A UI does not render `RunStatusDto`. It renders "waiting in the queue",
 * "thinking", "typing", "needs your approval", "done" — and the distinction
 * between the middle two exists nowhere in the status vocabulary, because it is
 * not a state the server holds: a run is `running` from the moment a worker
 * claims it until it finishes, whether or not a single token has arrived. The
 * evidence for "typing" is in the EVENT LOG (`run.started`, then deltas), which
 * is why this function takes both and neither alone is enough.
 *
 * That is also why it is here rather than in `@agentkit/contracts`. `streaming`
 * is a client-side derivation over two server facts, not a fact the server
 * publishes; putting it in the wire contract would oblige every server to
 * compute it, and they would each compute it slightly differently.
 *
 * ## Mapping the states consuming apps already have
 *
 * Both consumer frontends carry their own run-state enums, invented before this
 * API existed. Each of their extra states is a DERIVED phase here — they are
 * views of the same two inputs, not information this contract is missing:
 *
 * | Consumer state | Phase                | Where it comes from                          |
 * | -------------- | -------------------- | -------------------------------------------- |
 * | `waiting`      | `queued`             | `status: "queued"` — accepted, no worker yet. |
 * | `streaming`    | `streaming`          | `status: "running"` + a `run.started`/delta.  |
 * | (none)         | `running`            | `status: "running"`, nothing on the log yet.  |
 * | `paused`       | `waiting_approval`   | `status: "waiting_approval"` — a staged write.|
 * | `done`         | `completed`          | `run.completed`, or the terminal status.      |
 * | `error`        | `failed`             | `run.failed`, or the terminal status.         |
 * | `aborted`      | `cancelled`          | `run.cancelled`, or the terminal status.      |
 *
 * A consumer migrating to this client replaces its enum with {@link RunPhase}
 * and its state machine with one call: the phase is a pure function of what the
 * server said, so two tabs looking at the same run cannot disagree about it.
 */
import type { AiRunEvent, RunStatusDto } from "@agentkit/contracts";
import { isTerminalRunEvent } from "./stream.js";

export type RunPhase =
  | "queued"
  | "running"
  | "streaming"
  | "waiting_approval"
  | "completed"
  | "failed"
  | "cancelled";

export interface RunPhaseInput {
  /** The run's `status`, from `getRun` or a `RunDto` a list handed back. */
  status?: RunStatusDto;
  /**
   * Events seen so far — the whole log or just the ones this client received.
   * Order does not matter: only the presence of a terminal event and of the
   * first sign of output is read.
   */
  events?: readonly AiRunEvent[];
}

/** Events that prove the model has begun answering. */
const STREAMING_EVENT_TYPES: ReadonlySet<AiRunEvent["type"]> = new Set<
  AiRunEvent["type"]
>(["run.started", "run.message.delta"]);

export function runPhase(input: RunPhaseInput): RunPhase {
  const events = input.events ?? [];

  // A TERMINAL EVENT WINS over the status, and the ordering in the host is why:
  // the worker appends `run.completed` to the log and THEN transitions the task,
  // so a client that read the two in the other order holds a `running` status
  // next to a log that has already ended. Believing the status there would
  // strand a finished run in a spinner until the next poll.
  for (const event of events) {
    if (!isTerminalRunEvent(event)) continue;
    if (event.type === "run.completed") return "completed";
    if (event.type === "run.failed") return "failed";
    return "cancelled";
  }

  const status = input.status;
  if (status === "completed" || status === "failed" || status === "cancelled") {
    return status;
  }
  // Checked before `streaming`: a run parked on an approval has produced output
  // and is still not running, and "typing" would be a lie about what the user
  // has to do next.
  if (status === "waiting_approval") return "waiting_approval";

  const streaming = events.some((event) =>
    STREAMING_EVENT_TYPES.has(event.type),
  );
  if (streaming) return "streaming";

  if (status !== undefined) return status;
  // No status at all: the log is the only evidence. Something on it means a
  // worker picked the run up; nothing at all means it has not been seen.
  return events.length > 0 ? "running" : "queued";
}
