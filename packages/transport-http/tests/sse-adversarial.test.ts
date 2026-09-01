/**
 * The stream under the conditions that break naive SSE: a log too long to hold,
 * a consumer too slow to keep up, and a client that keeps hanging up.
 *
 * `sse.test.ts` covers the happy shapes — order, resume, close, heartbeat. This
 * file is about the two BOUNDS the stream promises and the ways they interact
 * with everything else: reads are paged by `readBatchSize`, and writes wait on
 * the queue instead of filling it. Both bounds are invisible to a correct
 * client, which is exactly why they need tests that can see them — the store is
 * instrumented so an assertion can ask "how much of the log did the pump
 * actually pull?", which is the only proxy for peak memory available from
 * outside the stream.
 *
 * Every timing here is deterministic in the sense that matters: nothing waits
 * on a real network, and the assertions are about counts and ordering, with
 * clocks used only as generous ceilings on "did this hang".
 */
import { describe, expect, it } from "bun:test";
import {
  CONTRACT_VERSION,
  type AiRunEvent,
  type TaskEventEnvelope,
} from "@agentkit/contracts";
import {
  defaultClock,
  defaultIds,
  type ListEventsOptions,
} from "@agentkit/host";
import { MemoryTaskStore } from "@agentkit/adapters-memory";
import { DEFAULT_STREAM_OPTIONS, type RestStreamOptions } from "../src/deps.js";
import { createRunEventStream, resolveStartSeq } from "../src/sse.js";

const TASK_ID = "task-adversarial";
const SCOPE_ID = "chat-adversarial";

/**
 * A reference store that remembers what the stream asked it for.
 *
 * `eventsRead` is the interesting number: the pump can only be holding what it
 * has pulled, so a bound on the envelopes handed out is a bound on the memory
 * the stream can be sitting on.
 */
class CountingTaskStore extends MemoryTaskStore {
  readonly reads: ListEventsOptions[] = [];
  eventsRead = 0;

  override async listEvents(
    taskId: string,
    opts?: ListEventsOptions,
  ): Promise<TaskEventEnvelope[]> {
    this.reads.push({ ...opts });
    const rows = await super.listEvents(taskId, opts);
    this.eventsRead += rows.length;
    return rows;
  }
}

function event(seq: number, type: AiRunEvent["type"]): AiRunEvent {
  return {
    type,
    runId: TASK_ID,
    timestamp: new Date(seq * 1000).toISOString(),
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-${seq}`,
    seq,
    data: type === "run.completed" ? { iterations: 1 } : { delta: `d${seq}` },
  } as AiRunEvent;
}

/** `count` deltas at seq 0..count-1. */
function deltas(count: number, from = 0): AiRunEvent[] {
  return Array.from({ length: count }, (_, i) =>
    event(from + i, "run.message.delta"),
  );
}

interface Log {
  tasks: CountingTaskStore;
  append(events: AiRunEvent[]): Promise<void>;
  /** Append `run.completed` at `seq` and settle the task, as a worker does. */
  finish(seq: number): Promise<void>;
}

/** A running task whose log already holds `seeded`. */
async function seed(seeded: AiRunEvent[]): Promise<Log> {
  const tasks = new CountingTaskStore(defaultClock, defaultIds);
  await tasks.createTask({
    taskId: TASK_ID,
    kind: "chat.turn",
    scopeId: SCOPE_ID,
    payload: { chatId: SCOPE_ID },
  });
  const lease = await tasks.acquireLease({
    taskId: TASK_ID,
    attemptId: "att-1",
    ownerId: "owner",
    ttlMs: 600_000,
  });
  const append = async (events: AiRunEvent[]): Promise<void> => {
    if (events.length === 0) return;
    await tasks.appendEvents(TASK_ID, events as TaskEventEnvelope[], {
      leaseToken: lease.leaseToken,
    });
  };
  await append(seeded);
  await tasks.transitionTask(TASK_ID, ["queued"], "running");
  // The counters start at zero AFTER seeding: the assertions are about what the
  // stream read, not about what the fixture wrote.
  tasks.reads.length = 0;
  tasks.eventsRead = 0;
  return {
    tasks,
    append,
    finish: async (seq: number) => {
      await append([event(seq, "run.completed")]);
      await tasks.transitionTask(TASK_ID, ["running"], "completed");
    },
  };
}

function options(
  overrides: RestStreamOptions = {},
): Required<RestStreamOptions> {
  return {
    ...DEFAULT_STREAM_OPTIONS,
    pollIntervalMs: 2,
    heartbeatIntervalMs: 60_000,
    ...overrides,
  };
}

interface Frame {
  id?: string;
  event?: string;
  comment?: string;
  retry?: string;
}

function parseFrame(block: string): Frame {
  const frame: Frame = {};
  for (const line of block.split("\n")) {
    if (line.startsWith(":")) frame.comment = line.slice(1).trim();
    else if (line.startsWith("id: ")) frame.id = line.slice(4);
    else if (line.startsWith("event: ")) frame.event = line.slice(7);
    else if (line.startsWith("retry: ")) frame.retry = line.slice(7);
  }
  return frame;
}

interface FrameStream {
  /** The next frame, or null once the stream closes. */
  next(): Promise<Frame | null>;
  /** The next frame carrying an event, skipping the hint and heartbeats. */
  nextEvent(): Promise<Frame | null>;
  cancel(): Promise<void>;
}

/**
 * Frame-at-a-time reading, because these tests need to stop mid-stream.
 *
 * `drain`-style helpers consume as fast as the producer writes, which is the
 * one thing a backpressure test must not do.
 */
function frameStream(stream: ReadableStream<Uint8Array>): FrameStream {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  const pending: Frame[] = [];
  let buffer = "";
  let ended = false;

  const next = async (): Promise<Frame | null> => {
    for (;;) {
      const ready = pending.shift();
      if (ready !== undefined) return ready;
      if (ended) return null;
      const { done, value } = await reader.read();
      if (done === true || value === undefined) {
        ended = true;
        return null;
      }
      buffer += decoder.decode(value, { stream: true });
      let split = buffer.indexOf("\n\n");
      while (split !== -1) {
        pending.push(parseFrame(buffer.slice(0, split)));
        buffer = buffer.slice(split + 2);
        split = buffer.indexOf("\n\n");
      }
    }
  };

  return {
    next,
    nextEvent: async () => {
      for (;;) {
        const frame = await next();
        if (frame === null || frame.id !== undefined) return frame;
      }
    },
    cancel: async () => {
      await reader.cancel();
    },
  };
}

const wait = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

/** The seq every `evt-N` id encodes, for a contiguity assertion. */
function seqsOf(frames: Frame[]): number[] {
  return frames.map((f) => Number((f.id ?? "evt--1").slice(4)));
}

describe("createRunEventStream under load", () => {
  it("(a) replays a long log in batches, exactly once and in order", async () => {
    const log = await seed(deltas(1_000));
    await log.finish(1_000);
    // A poll interval two orders of magnitude above the test's budget: if a
    // full batch were mistaken for "caught up" the pump would sleep between
    // every one of the 63 batches and this would take twelve seconds.
    const stream = createRunEventStream({
      tasks: log.tasks,
      taskId: TASK_ID,
      startSeq: 0,
      options: options({ readBatchSize: 16, pollIntervalMs: 200 }),
    });

    const started = Date.now();
    const frames = frameStream(stream);
    const received: Frame[] = [];
    for (;;) {
      const frame = await frames.next();
      if (frame === null) break;
      if (frame.id !== undefined) received.push(frame);
    }
    const elapsed = Date.now() - started;

    expect(received.length).toBe(1_001);
    expect(seqsOf(received)).toEqual(
      Array.from({ length: 1_001 }, (_, i) => i),
    );
    expect(received[1_000]?.event).toBe("run.completed");
    expect(elapsed).toBeLessThan(2_000);

    // Every read was bounded, and the log was walked once rather than re-read
    // from the top on each pass.
    expect(log.tasks.reads.every((r) => r.limit === 16)).toBe(true);
    expect(log.tasks.reads.length).toBeLessThanOrEqual(
      Math.ceil(1_001 / 16) + 3,
    );
    expect(log.tasks.eventsRead).toBeLessThanOrEqual(1_001 + 16);
  });

  it("(b) stops pulling the log when the consumer stops reading", async () => {
    const log = await seed(deltas(400));
    const stream = createRunEventStream({
      tasks: log.tasks,
      taskId: TASK_ID,
      startSeq: 0,
      options: options({ readBatchSize: 8, pollIntervalMs: 5 }),
    });
    const frames = frameStream(stream);

    // One frame, then nothing. The queue fills to its high-water mark and the
    // pump parks in the pause loop.
    expect((await frames.next())?.retry).toBe(
      String(DEFAULT_STREAM_OPTIONS.retryHintMs),
    );
    await wait(60);

    // The bound: one batch queued, at most one more in the pump's own hands.
    // Unbounded, this would be all 400.
    expect(log.tasks.eventsRead).toBeLessThanOrEqual(8 * 3);
    const readWhileParked = log.tasks.eventsRead;

    // Draining releases it, and nothing was dropped or reordered while parked.
    const received: Frame[] = [];
    while (received.length < 400) {
      const frame = await frames.nextEvent();
      if (frame === null) break;
      received.push(frame);
      if (received.length % 50 === 0) await wait(1);
    }
    expect(seqsOf(received)).toEqual(Array.from({ length: 400 }, (_, i) => i));
    expect(log.tasks.eventsRead).toBeGreaterThan(readWhileParked);
    await frames.cancel();
  });

  it("(c) survives a reconnect storm with no gap and no duplicate", async () => {
    const log = await seed(deltas(120));
    await log.finish(120);

    const received: Frame[] = [];
    let lastId: string | null = null;
    let cycles = 0;
    let terminal = false;
    while (!terminal && cycles < 60) {
      cycles += 1;
      const startSeq = await resolveStartSeq(log.tasks, TASK_ID, lastId, 4);
      const controller = new AbortController();
      const frames = frameStream(
        createRunEventStream({
          tasks: log.tasks,
          taskId: TASK_ID,
          startSeq,
          options: options({ readBatchSize: 4 }),
          signal: controller.signal,
        }),
      );
      const segment: Frame[] = [];
      while (segment.length < 4) {
        const frame = await frames.nextEvent();
        if (frame === null) break;
        segment.push(frame);
        lastId = frame.id ?? lastId;
        if (frame.event === "run.completed") terminal = true;
      }
      controller.abort();
      await frames.cancel();

      // Within one connection the frames are contiguous — a resumed segment
      // that skipped or repeated would show up here, not only in the union.
      const seqs = seqsOf(segment);
      expect(seqs).toEqual(seqs.map((_, i) => (seqs[0] ?? 0) + i));
      received.push(...segment);
    }

    expect(terminal).toBe(true);
    expect(cycles).toBeGreaterThanOrEqual(25);
    expect(seqsOf(received)).toEqual(Array.from({ length: 121 }, (_, i) => i));
  });

  it("(d) picks up events appended while it is streaming, batched", async () => {
    const log = await seed(deltas(5));
    const stream = createRunEventStream({
      tasks: log.tasks,
      taskId: TASK_ID,
      startSeq: 0,
      options: options({ readBatchSize: 2 }),
    });
    const frames = frameStream(stream);
    const received: Frame[] = [];

    // Five seeded, then twenty more mid-stream, then the terminal — each write
    // lands while the pump is somewhere in its batch/poll cycle.
    for (let i = 0; i < 5; i += 1) {
      const frame = await frames.nextEvent();
      if (frame !== null) received.push(frame);
    }
    await log.append(deltas(20, 5));
    await log.finish(25);

    for (;;) {
      const frame = await frames.nextEvent();
      if (frame === null) break;
      received.push(frame);
    }
    expect(seqsOf(received)).toEqual(Array.from({ length: 26 }, (_, i) => i));
    expect(received[25]?.event).toBe("run.completed");
  });

  it("(e) skips heartbeats while behind and resumes them once caught up", async () => {
    const log = await seed(deltas(40));
    const frames = frameStream(
      createRunEventStream({
        tasks: log.tasks,
        taskId: TASK_ID,
        startSeq: 0,
        options: options({
          readBatchSize: 8,
          pollIntervalMs: 2,
          heartbeatIntervalMs: 1,
        }),
      }),
    );

    await frames.next();
    // Long enough for dozens of heartbeat intervals to pass with the queue
    // full: not one of them may take a slot an event is waiting for.
    await wait(40);
    for (let i = 0; i < 8; i += 1) {
      expect((await frames.next())?.comment).toBeUndefined();
    }

    // Caught up, the keepalive comes back — skipping while saturated must not
    // wedge it.
    const received: Frame[] = [];
    const deadline = Date.now() + 2_000;
    let sawHeartbeat = false;
    while (!sawHeartbeat && Date.now() < deadline) {
      const frame = await frames.next();
      if (frame === null) break;
      if (frame.id !== undefined) received.push(frame);
      if (frame.comment === "hb") sawHeartbeat = true;
    }
    expect(sawHeartbeat).toBe(true);
    // The backlog came out whole before the first keepalive: nothing queued
    // behind a comment, nothing lost to one.
    expect(seqsOf(received)).toEqual(
      Array.from({ length: 32 }, (_, i) => i + 8),
    );
    await frames.cancel();
  });

  it("(f) an abort during a backpressure pause closes without waiting out the timer", async () => {
    const log = await seed(deltas(40));
    const controller = new AbortController();
    const frames = frameStream(
      createRunEventStream({
        tasks: log.tasks,
        taskId: TASK_ID,
        startSeq: 0,
        // A pause the abort must CUT: sit through it and this test takes five
        // seconds instead of a few milliseconds.
        options: options({ readBatchSize: 4, pollIntervalMs: 5_000 }),
        signal: controller.signal,
      }),
    );

    await frames.next();
    await wait(20);
    controller.abort();

    const finished = (async () => {
      for (;;) {
        if ((await frames.next()) === null) return true;
      }
    })();
    const raced = await Promise.race([finished, wait(1_000).then(() => false)]);
    expect(raced).toBe(true);
  });

  it("(g) an abort raced against the terminal event settles either way", async () => {
    const log = await seed(deltas(4));
    const controller = new AbortController();
    const frames = frameStream(
      createRunEventStream({
        tasks: log.tasks,
        taskId: TASK_ID,
        startSeq: 0,
        options: options({ readBatchSize: 2 }),
        signal: controller.signal,
      }),
    );
    await frames.nextEvent();

    // Both in the same tick: whichever the pump notices first, the stream must
    // end, and it must end without throwing.
    const settled = log.finish(4);
    controller.abort();
    await settled;

    const finished = (async () => {
      for (;;) {
        if ((await frames.next()) === null) return true;
      }
    })();
    expect(await Promise.race([finished, wait(1_000).then(() => false)])).toBe(
      true,
    );
  });
});
