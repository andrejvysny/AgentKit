/**
 * `streamRun` as Server-Sent Events: replay the durable log, then follow it.
 *
 * THE ALGORITHM IS REPLAY-THEN-POLL ON A SEQ CURSOR, and the cursor is the
 * whole point. The obvious alternative — replay the log, then subscribe to a
 * live feed — has a gap nobody can close: events appended between the last row
 * the replay read and the moment the subscription attaches are in neither, and
 * the bug shows up only under load, as a UI missing one delta out of a thousand.
 * A cursor cannot have that race, because "what have I sent?" is a number the
 * reader owns and every read is defined relative to it. The cost is a poll
 * interval of latency; the benefit is that resume, replay and live-follow are
 * ONE code path instead of three that must agree.
 *
 * Resume works for the same reason. `Last-Event-ID` carries an
 * `AiRunEvent.eventId`; the stream finds its `seq` and starts one past it, so a
 * client that dropped its connection mid-run gets exactly the tail it missed —
 * no duplicates for the UI to dedupe, no gap for it to notice. An UNKNOWN id
 * replays from the beginning rather than failing: the run's log is the truth, a
 * client holding an id from another run (or from a run whose log was pruned)
 * cannot be resumed from, and a full replay is the only answer that leaves it
 * consistent. It is also cheap to make idempotent client-side, which a partial
 * stream is not.
 *
 * Closing: the stream ends when a terminal run event is emitted, and also when
 * the task is terminal but its log holds no terminal event (a crashed attempt,
 * a task cancelled before its worker ever wrote one) — otherwise that stream
 * would poll forever against a run that will never speak again.
 */
import type { TaskEventEnvelope } from "@agentkit/contracts";
import type { Logger, TaskStatus, TaskStore } from "@agentkit/host";
import type { RestStreamOptions } from "./deps.js";

/** Run events after which nothing else is coming. */
export const TERMINAL_RUN_EVENT_TYPES: ReadonlySet<string> = new Set([
  "run.completed",
  "run.failed",
  "run.cancelled",
]);

const TERMINAL_TASK_STATUSES: ReadonlySet<TaskStatus> = new Set<TaskStatus>([
  "completed",
  "failed",
  "cancelled",
]);

export const SSE_HEADERS: Readonly<Record<string, string>> = Object.freeze({
  "content-type": "text/event-stream",
  // `no-transform` matters as much as `no-cache`: a proxy that "helpfully"
  // buffers or recompresses an event stream turns live tokens into one blob
  // at the end.
  "cache-control": "no-cache, no-transform",
});

export interface RunEventStreamInput {
  tasks: TaskStore;
  taskId: string;
  /** Where to resume from: the seq of the first event to send. */
  startSeq: number;
  options: Required<RestStreamOptions>;
  /** The request's abort signal; aborting stops the timers and closes. */
  signal?: AbortSignal;
  logger?: Logger;
}

/**
 * The seq to start from, given a `Last-Event-ID` header.
 *
 * One full read of the log, because the port exposes no "find by eventId": an
 * id is a string the store dedups on, not an index. That read happens once per
 * connection, against a log a client is about to receive most of anyway.
 */
export async function resolveStartSeq(
  tasks: TaskStore,
  taskId: string,
  lastEventId: string | null,
): Promise<number> {
  if (lastEventId === null || lastEventId.trim() === "") return 0;
  const log = await tasks.listEvents(taskId);
  for (const event of log) {
    if (event.eventId === lastEventId) return event.seq + 1;
  }
  return 0;
}

/** One SSE frame carrying one event verbatim (`RunEventFrameDto`). */
export function frameFor(event: TaskEventEnvelope): string {
  return `id: ${event.eventId}\nevent: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`;
}

export function createRunEventStream(
  input: RunEventStreamInput,
): ReadableStream<Uint8Array> {
  const { tasks, taskId, options, signal, logger } = input;
  const encoder = new TextEncoder();
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let wake: (() => void) | null = null;

  /** Stop the clock. Safe to call twice; every exit path calls it. */
  const stop = (): void => {
    closed = true;
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    const pending = wake;
    wake = null;
    pending?.();
  };

  const onAbort = (): void => stop();
  signal?.addEventListener("abort", onAbort, { once: true });
  if (signal?.aborted === true) stop();

  /** Sleep that an abort cuts short — a cancelled client must not linger. */
  const sleep = (ms: number): Promise<void> =>
    new Promise<void>((resolve) => {
      if (closed) {
        resolve();
        return;
      }
      wake = () => {
        wake = null;
        resolve();
      };
      timer = setTimeout(() => {
        timer = null;
        wake = null;
        resolve();
      }, ms);
    });

  return new ReadableStream<Uint8Array>({
    start(controller) {
      /** False once the stream is gone; every writer checks and bails. */
      const write = (frame: string): boolean => {
        if (closed) return false;
        try {
          controller.enqueue(encoder.encode(frame));
          return true;
        } catch {
          // The peer closed between our check and this enqueue. Not an error:
          // a client hanging up mid-run is the normal end of a stream.
          stop();
          return false;
        }
      };

      const pump = async (): Promise<void> => {
        // The reconnect hint goes out first, before any event, so a client that
        // loses the connection on the very next byte already knows the policy.
        if (!write(`retry: ${options.retryHintMs}\n\n`)) return;
        let cursor = input.startSeq;
        let lastWriteAt = Date.now();

        while (!closed) {
          const batch = await readAfter(tasks, taskId, cursor);
          if (batch.length > 0) {
            for (const event of batch) {
              if (event.seq < cursor) continue;
              if (!write(frameFor(event))) return;
              cursor = event.seq + 1;
              lastWriteAt = Date.now();
              if (TERMINAL_RUN_EVENT_TYPES.has(event.type)) return;
            }
            continue;
          }

          const task = await tasks.getTask(taskId);
          if (task === null || TERMINAL_TASK_STATUSES.has(task.status)) {
            // One last read. The worker appends its terminal event and THEN
            // transitions the task, so a status read that lands between the two
            // must not close over an event already written.
            for (const event of await readAfter(tasks, taskId, cursor)) {
              if (event.seq < cursor) continue;
              if (!write(frameFor(event))) return;
              cursor = event.seq + 1;
              if (TERMINAL_RUN_EVENT_TYPES.has(event.type)) return;
            }
            return;
          }

          await sleep(options.pollIntervalMs);
          if (closed) return;
          if (Date.now() - lastWriteAt >= options.heartbeatIntervalMs) {
            if (!write(": hb\n\n")) return;
            lastWriteAt = Date.now();
          }
        }
      };

      // NOT awaited: `start` resolving is what makes the body readable, and a
      // stream that only becomes readable when the run ends is not a stream.
      void pump()
        .catch((err: unknown) => {
          logger?.error("run event stream failed", {
            taskId,
            message: err instanceof Error ? err.message : String(err),
          });
        })
        .finally(() => {
          stop();
          signal?.removeEventListener("abort", onAbort);
          try {
            controller.close();
          } catch {
            // Already closed — an abort and a terminal event can land together.
          }
        });
    },
    cancel() {
      stop();
      signal?.removeEventListener("abort", onAbort);
    },
  });
}

/**
 * Events at or after `cursor`, in seq order.
 *
 * `ListEventsOptions.afterSeq` is EXCLUSIVE (`seq > afterSeq`), so a cursor of
 * N asks for `afterSeq: N - 1`; at cursor 0 the option is omitted rather than
 * passed as -1, since no store's contract says anything about negative input.
 */
async function readAfter(
  tasks: TaskStore,
  taskId: string,
  cursor: number,
): Promise<TaskEventEnvelope[]> {
  const events =
    cursor <= 0
      ? await tasks.listEvents(taskId)
      : await tasks.listEvents(taskId, { afterSeq: cursor - 1 });
  return [...events].sort((a, b) => a.seq - b.seq);
}
