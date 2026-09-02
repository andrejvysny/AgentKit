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
 *
 * BOTH ENDS OF THE PIPE ARE BOUNDED, and by the same number
 * (`RestStreamOptions.readBatchSize`). Reads take a `limit`, so replaying a
 * long log walks it a batch at a time instead of materialising it whole — and a
 * cursor at 0 no longer re-reads the entire log on every poll. Writes wait on
 * `controller.desiredSize`: when the consumer is behind and the queue is full,
 * the pump PAUSES — until the stream's own `pull` says a slot opened — rather
 * than enqueueing into a buffer nobody is draining. A slow client then costs one
 * batch of memory instead of a whole run's worth, and it costs it in the store's
 * pages rather than in this process' heap.
 *
 * Pausing is the only backpressure answer available here, because the two
 * alternatives are both wrong for a durable log: dropping frames breaks the
 * `seq` contiguity a resuming client checks, and reordering them is worse. The
 * cursor advances only on a frame that was actually enqueued, so a pause is
 * indistinguishable from a slow run — and a client that gives up mid-pause
 * resumes from exactly the last id it saw. Heartbeats are skipped while
 * saturated for the same reason they exist: a `: hb` proves the connection is
 * alive, and a connection with frames still queued on it is proving that
 * already.
 */
import type { TaskEventEnvelope } from "@agentkit/contracts";
import type { Logger, TaskStatus, TaskStore } from "@agentkit/host";
import { DEFAULT_STREAM_OPTIONS, type RestStreamOptions } from "./deps.js";

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
 * A scan of the log, because the port exposes no "find by eventId": an id is a
 * string the store dedups on, not an index. The scan happens once per
 * connection, against a log the client is about to receive most of anyway — but
 * it walks in `batchSize` pages rather than loading the whole log, so the one
 * read that precedes a stream is bounded by the same number the stream itself
 * is. A resume against a very long run should not cost more before the first
 * frame than the whole replay costs after it.
 */
export async function resolveStartSeq(
  tasks: TaskStore,
  taskId: string,
  lastEventId: string | null,
  batchSize: number = DEFAULT_STREAM_OPTIONS.readBatchSize,
): Promise<number> {
  if (lastEventId === null || lastEventId.trim() === "") return 0;
  let cursor = 0;
  for (;;) {
    const batch = await readAfter(tasks, taskId, cursor, batchSize);
    for (const event of batch) {
      if (event.eventId === lastEventId) return event.seq + 1;
      cursor = Math.max(cursor, event.seq + 1);
    }
    // Short or empty means the log ended without the id: an event from another
    // run, or from one whose log was pruned. Replay from the beginning.
    if (batch.length === 0 || batch.length < batchSize) return 0;
  }
}

/**
 * One SSE frame carrying one event verbatim (`RunEventFrameDto`).
 *
 * `id` and `event` are stripped of CR and LF before they are interpolated.
 * They are the only two fields written RAW — `data` goes through
 * `JSON.stringify`, which escapes both — and a newline in either one ends the
 * field, then the frame, and lets whatever follows be read by the client as
 * further SSE fields of its own. Neither value is this adapter's: `eventId` and
 * `type` are strings the store handed back, ultimately written by whatever
 * appended to the log.
 *
 * Stripped rather than rejected: a frame is being written to an open stream,
 * where there is no status code left to refuse with, and dropping the event
 * would break the `seq` continuity the client uses to detect loss.
 */
export function frameFor(event: TaskEventEnvelope): string {
  const id = stripNewlines(event.eventId);
  const type = stripNewlines(event.type);
  return `id: ${id}\nevent: ${type}\ndata: ${JSON.stringify(event)}\n\n`;
}

function stripNewlines(value: string): string {
  return value.replace(/[\r\n]/g, "");
}

export function createRunEventStream(
  input: RunEventStreamInput,
): ReadableStream<Uint8Array> {
  const { tasks, taskId, options, signal, logger } = input;
  const encoder = new TextEncoder();
  let closed = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let wake: (() => void) | null = null;
  /** Set while the pump is parked on a full queue; `pull` is what resolves it. */
  let capacity: (() => void) | null = null;

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
    const parked = capacity;
    capacity = null;
    parked?.();
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

  const batchLimit = options.readBatchSize;

  return new ReadableStream<Uint8Array>(
    {
      start(controller) {
        /** Room for another frame? Null means errored — let the enqueue say so. */
        const hasRoom = (): boolean => (controller.desiredSize ?? 1) > 0;

        /**
         * Park until the consumer takes something.
         *
         * Signalled by `pull` rather than by a timer, and the difference is not
         * a micro-optimisation: a poll would hold the pump for a whole interval
         * after the reader had already made room, so a long replay to a reader
         * that runs one tick behind the writer would be rationed at one
         * high-water mark per interval — seconds to replay what should take
         * milliseconds. `pull` fires on the read that frees the slot.
         */
        const roomAvailable = (): Promise<void> =>
          new Promise<void>((resolve) => {
            if (closed) {
              resolve();
              return;
            }
            capacity = resolve;
          });

        /**
         * Enqueue one frame, waiting out a full queue first. False once the
         * stream is gone; every writer checks and bails.
         */
        const write = async (frame: string): Promise<boolean> => {
          // The pause: not a drop, not a reorder, and not an unbounded buffer.
          // `stop` resolves the parked promise, so an abort here exits on the
          // same tick it exits everywhere else.
          while (!closed && !hasRoom()) await roomAvailable();
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
          // The reconnect hint goes out first, before any event, so a client
          // that loses the connection on the very next byte already knows the
          // policy.
          if (!(await write(`retry: ${options.retryHintMs}\n\n`))) return;
          let cursor = input.startSeq;
          let lastWriteAt = Date.now();

          /**
           * Send everything the log holds from `cursor` on, a batch at a time.
           *
           * A batch that came back FULL is not evidence of having caught up —
           * it is evidence of a backlog the limit truncated — so it loops
           * straight into the next read with no sleep and no status check. Only
           * a short batch means the log has been walked to its end.
           */
          const drain = async (): Promise<
            "closed" | "terminal" | "current"
          > => {
            for (;;) {
              const batch = await readAfter(tasks, taskId, cursor, batchLimit);
              for (const event of batch) {
                if (event.seq < cursor) continue;
                if (!(await write(frameFor(event)))) return "closed";
                cursor = event.seq + 1;
                lastWriteAt = Date.now();
                if (TERMINAL_RUN_EVENT_TYPES.has(event.type)) return "terminal";
              }
              if (batch.length === 0 || batch.length < batchLimit) {
                return closed ? "closed" : "current";
              }
            }
          };

          while (!closed) {
            if ((await drain()) !== "current") return;

            const task = await tasks.getTask(taskId);
            if (task === null || TERMINAL_TASK_STATUSES.has(task.status)) {
              // One last read. The worker appends its terminal event and THEN
              // transitions the task, so a status read that lands between the
              // two must not close over an event already written.
              await drain();
              return;
            }

            await sleep(options.pollIntervalMs);
            if (closed) return;
            // Skipped while the consumer is behind: the queued frames are
            // already proof the connection is alive, and a keepalive that has
            // to wait its turn behind them is not keeping anything alive.
            if (
              Date.now() - lastWriteAt >= options.heartbeatIntervalMs &&
              hasRoom()
            ) {
              if (!(await write(": hb\n\n"))) return;
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
              // Already closed — an abort and a terminal event can land
              // together.
            }
          });
      },
      /** The consumer took a chunk: whatever is parked in `write` may go on. */
      pull() {
        const parked = capacity;
        capacity = null;
        parked?.();
      },
      cancel() {
        stop();
        signal?.removeEventListener("abort", onAbort);
      },
    },
    // The queue is the write-side half of the same bound the reads use: at most
    // one batch of frames may sit in front of a consumer that has stopped
    // taking them, and `desiredSize` turns that into the signal the pump waits
    // on. The default strategy would be a high-water mark of ONE chunk, which
    // is not backpressure but a stall — a pump that must round-trip the event
    // loop between every frame cannot keep a fast client fed.
    new CountQueuingStrategy({ highWaterMark: batchLimit }),
  );
}

/**
 * At most `limit` events at or after `cursor`, in seq order.
 *
 * `ListEventsOptions.afterSeq` is EXCLUSIVE (`seq > afterSeq`), so a cursor of
 * N asks for `afterSeq: N - 1`; at cursor 0 the option is omitted rather than
 * passed as -1, since no store's contract says anything about negative input.
 *
 * `limit` is read as "the first `limit` in seq order", which is the only
 * reading that makes a paged walk of an ordered log terminate correctly — a
 * store returning an arbitrary subset would let the cursor skip past a gap it
 * never sent. The local sort is belt to that braces: it fixes an unordered
 * return, but nothing can recover a page the store chose badly.
 */
async function readAfter(
  tasks: TaskStore,
  taskId: string,
  cursor: number,
  limit: number,
): Promise<TaskEventEnvelope[]> {
  const events =
    cursor <= 0
      ? await tasks.listEvents(taskId, { limit })
      : await tasks.listEvents(taskId, { afterSeq: cursor - 1, limit });
  return [...events].sort((a, b) => a.seq - b.seq);
}
