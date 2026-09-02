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
   * Events seen so far — the whole log or just the ones this client received,
   * IN LOG ORDER.
   *
   * Order is load-bearing, which it did not use to be: a run can hold several
   * terminal events, one per pass (see {@link isPassBoundary}), and the phase
   * is decided by the LAST of them and by whether a pass boundary came after
   * it. Handing this a shuffled log reports a phase the run never had.
   */
  events?: readonly AiRunEvent[];
}

/** Events that prove the model has begun answering. */
const STREAMING_EVENT_TYPES: ReadonlySet<AiRunEvent["type"]> = new Set<
  AiRunEvent["type"]
>(["run.started", "run.message.delta"]);

/** The warning code the host writes immediately before a recovery pass. */
const RETRY_PASS_CODE = "retry_pass";

export function runPhase(input: RunPhaseInput): RunPhase {
  const events = input.events ?? [];

  // Folded through the tracker rather than scanned here: "last terminal wins,
  // and a pass boundary after it clears it" is one rule with one implementation,
  // and two copies of it would disagree the first time either changed.
  const tracker = createRunPhaseTracker();
  for (const event of events) tracker.observe(event);
  const fromEvents = tracker.phase();

  // A TERMINAL EVENT WINS over the status, and the ordering in the host is why:
  // the worker appends `run.completed` to the log and THEN transitions the task,
  // so a client that read the two in the other order holds a `running` status
  // next to a log that has already ended. Believing the status there would
  // strand a finished run in a spinner until the next poll.
  if (
    fromEvents === "completed" ||
    fromEvents === "failed" ||
    fromEvents === "cancelled"
  ) {
    return fromEvents;
  }

  const status = input.status;
  if (status === "completed" || status === "failed" || status === "cancelled") {
    return status;
  }
  // Checked before `streaming`: a run parked on an approval has produced output
  // and is still not running, and "typing" would be a lie about what the user
  // has to do next.
  if (status === "waiting_approval") return "waiting_approval";

  if (fromEvents === "streaming") return "streaming";

  if (status !== undefined) return status;
  // No status at all: the log is the only evidence. Something on it means a
  // worker picked the run up; nothing at all means it has not been seen.
  return events.length > 0 ? "running" : "queued";
}

/**
 * A PASS BOUNDARY: the point where the host abandons what it has said so far and
 * asks again.
 *
 * Two shapes, because one of them predates the vocabulary. The explicit one is
 * the `retry_pass` warning the host writes before a recovery or correction pass.
 * The implicit one is a SECOND `run.started`: the run loop emits it on the first
 * iteration of every `runChat` call, so a log that already showed output or a
 * terminal event and now shows another `run.started` is a log with a second pass
 * on it — which is what a client talking to an older host has to read.
 *
 * `seen` and `terminal` are the reader's state so far, which is what makes "a
 * second one" decidable at all.
 */
function isPassBoundary(
  event: AiRunEvent,
  state: { streaming: boolean; terminal: RunPhase | null },
): boolean {
  if (event.type === "run.warning" && event.data.code === RETRY_PASS_CODE) {
    return true;
  }
  return (
    event.type === "run.started" && (state.streaming || state.terminal !== null)
  );
}

/**
 * The same phase, maintained one event at a time.
 *
 * {@link runPhase} walks the whole log on every call, which is exactly what a
 * hook re-deriving the phase on each arriving delta must not do: a
 * thousand-token answer then costs a million comparisons for a value that can
 * only move forwards. Output is MONOTONIC — a run that has produced output never
 * un-produces it — so a follower can simply remember it.
 *
 * A TERMINAL EVENT IS NOT monotonic, and that is the whole reason this file
 * changed: the LAST terminal event wins, and a pass boundary after one clears it
 * because the run is live again. A log ending `run.failed`, `retry_pass`,
 * `run.started`, deltas… is `streaming`; the same log ending `run.completed` is
 * `completed`.
 *
 * The EVENT half of {@link runPhase} is this, literally — that function folds a
 * log through a tracker — so the two cannot drift. A caller that also holds a
 * `status` still wants {@link runPhase}.
 */
export interface RunPhaseTracker {
  /** Fold one event in, and hand back the phase as of it. */
  observe(event: AiRunEvent): RunPhase;
  /** The phase as of the last event observed. */
  phase(): RunPhase;
  /**
   * Whether the event last handed to {@link observe} opened a NEW PASS.
   *
   * What a consumer does with it is reset whatever it has streamed: the host
   * clears the stored answer at the same seam (`TurnRunner`'s `resetPass`), so a
   * UI that kept pass 1's half sentence in front of pass 2's answer is showing
   * text the conversation does not contain.
   */
  startedNewPass(): boolean;
}

export function createRunPhaseTracker(): RunPhaseTracker {
  let terminal: RunPhase | null = null;
  let streaming = false;
  let seen = false;
  let boundary = false;

  const phase = (): RunPhase => {
    if (terminal !== null) return terminal;
    if (streaming) return "streaming";
    return seen ? "running" : "queued";
  };

  return {
    observe(event: AiRunEvent): RunPhase {
      // Decided BEFORE this event moves the state, since "a second
      // `run.started`" is a question about what came before it.
      boundary = isPassBoundary(event, { streaming, terminal });
      // The previous pass's outcome is not this run's outcome any more. Output
      // is left alone: the run HAS produced output, and un-setting it would
      // report a run mid-answer as `running`.
      if (boundary) terminal = null;
      seen = true;
      if (STREAMING_EVENT_TYPES.has(event.type)) streaming = true;
      if (isTerminalRunEvent(event)) {
        // LAST terminal wins: a pass that failed and was retried into a pass
        // that completed is a completed run, and the first-wins reading
        // reported the failure of a turn the user is reading the answer to.
        terminal =
          event.type === "run.completed"
            ? "completed"
            : event.type === "run.failed"
              ? "failed"
              : "cancelled";
      }
      return phase();
    },

    startedNewPass(): boolean {
      return boundary;
    },
    phase,
  };
}
