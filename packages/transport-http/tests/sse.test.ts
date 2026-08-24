/**
 * The stream, exercised directly against a seeded event log.
 *
 * These are the properties a resuming client depends on and that no route-level
 * assertion can see: the frames come out in `seq` order carrying `eventId` as
 * the SSE id, a terminal event ends the stream, `Last-Event-ID` starts one past
 * the event it names, and neither an idle run nor an abandoned one leaves a
 * connection polling forever.
 */
import { describe, expect, it } from "bun:test";
import {
  CONTRACT_VERSION,
  type AiRunEvent,
  type TaskEventEnvelope,
} from "@agentkit/contracts";
import { MemoryAssistantStore } from "@agentkit/reference-adapters";
import { DEFAULT_STREAM_OPTIONS, type RestStreamOptions } from "../src/deps.js";
import { createRunEventStream, resolveStartSeq } from "../src/sse.js";

const TASK_ID = "task-stream";
const CHAT_ID = "chat-stream";

/** A run's worth of events: started, two deltas, completed. */
function completedRun(): AiRunEvent[] {
  return [
    event(0, "run.started", { model: "m1", toolCount: 0 }),
    event(1, "run.message.delta", { delta: "Hel" }),
    event(2, "run.message.delta", { delta: "lo" }),
    event(3, "run.message.completed", { content: "Hello", toolCallCount: 0 }),
    event(4, "run.completed", { iterations: 1 }),
  ];
}

function event(
  seq: number,
  type: AiRunEvent["type"],
  data: Record<string, unknown>,
): AiRunEvent {
  return {
    type,
    runId: TASK_ID,
    timestamp: new Date(seq * 1000).toISOString(),
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-${seq}`,
    seq,
    data,
  } as AiRunEvent;
}

/** A store holding one task whose log is `events`, left in `status`. */
async function seed(
  events: AiRunEvent[],
  status: "queued" | "running" | "completed" = "completed",
): Promise<MemoryAssistantStore> {
  const store = new MemoryAssistantStore();
  await store.conversations.createChat({ id: CHAT_ID });
  await store.tasks.createTask({
    taskId: TASK_ID,
    kind: "chat.turn",
    scopeId: CHAT_ID,
    payload: { chatId: CHAT_ID },
  });
  if (events.length > 0) {
    const lease = await store.tasks.acquireLease({
      taskId: TASK_ID,
      attemptId: "att-1",
      ownerId: "owner",
      ttlMs: 60_000,
    });
    await store.tasks.appendEvents(TASK_ID, events as TaskEventEnvelope[], {
      leaseToken: lease.leaseToken,
    });
  }
  if (status !== "queued") {
    await store.tasks.transitionTask(TASK_ID, ["queued"], "running");
  }
  if (status === "completed") {
    await store.tasks.transitionTask(TASK_ID, ["running"], "completed");
  }
  return store;
}

function options(overrides: RestStreamOptions = {}): Required<RestStreamOptions> {
  return { ...DEFAULT_STREAM_OPTIONS, pollIntervalMs: 2, ...overrides };
}

interface Frame {
  id?: string;
  event?: string;
  data?: string;
  comment?: string;
  retry?: string;
}

function parseFrames(text: string): Frame[] {
  return text
    .split("\n\n")
    .filter((block) => block.trim() !== "")
    .map((block) => {
      const frame: Frame = {};
      for (const line of block.split("\n")) {
        if (line.startsWith(":")) frame.comment = line.slice(1).trim();
        else if (line.startsWith("id: ")) frame.id = line.slice(4);
        else if (line.startsWith("event: ")) frame.event = line.slice(7);
        else if (line.startsWith("data: ")) frame.data = line.slice(6);
        else if (line.startsWith("retry: ")) frame.retry = line.slice(7);
      }
      return frame;
    });
}

async function drain(stream: ReadableStream<Uint8Array>): Promise<string> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let out = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    out += decoder.decode(value, { stream: true });
  }
  return out;
}

describe("createRunEventStream", () => {
  it("(a) replays a finished log in seq order and closes on the terminal event", async () => {
    const store = await seed(completedRun());
    const text = await drain(
      createRunEventStream({
        tasks: store.tasks,
        taskId: TASK_ID,
        startSeq: 0,
        options: options(),
      }),
    );
    const frames = parseFrames(text);
    expect(frames[0]?.retry).toBe(String(DEFAULT_STREAM_OPTIONS.retryHintMs));

    const events = frames.slice(1);
    expect(events.map((f) => f.id)).toEqual([
      "evt-0",
      "evt-1",
      "evt-2",
      "evt-3",
      "evt-4",
    ]);
    expect(events.map((f) => f.event)).toEqual([
      "run.started",
      "run.message.delta",
      "run.message.delta",
      "run.message.completed",
      "run.completed",
    ]);
    // The frame body is the event verbatim — `RunEventFrameDto` is `AiRunEvent`.
    const first = JSON.parse(events[0]?.data ?? "{}") as AiRunEvent;
    expect(first).toEqual(completedRun()[0] as AiRunEvent);
  });

  it("(b) resumes one past the event Last-Event-ID names", async () => {
    const store = await seed(completedRun());
    const startSeq = await resolveStartSeq(store.tasks, TASK_ID, "evt-2");
    expect(startSeq).toBe(3);
    const frames = parseFrames(
      await drain(
        createRunEventStream({
          tasks: store.tasks,
          taskId: TASK_ID,
          startSeq,
          options: options(),
        }),
      ),
    ).slice(1);
    expect(frames.map((f) => f.id)).toEqual(["evt-3", "evt-4"]);
  });

  it("(c) replays from the start when Last-Event-ID is unknown", async () => {
    const store = await seed(completedRun());
    // Documented choice: an id this run's log does not contain cannot be
    // resumed from, and a full replay is the only answer that leaves the
    // client consistent.
    expect(await resolveStartSeq(store.tasks, TASK_ID, "evt-from-another-run"))
      .toBe(0);
    expect(await resolveStartSeq(store.tasks, TASK_ID, null)).toBe(0);
  });

  it("(d) closes a terminal task whose log has no terminal event", async () => {
    // A crashed attempt: the run ended, nothing said so in the log. Without
    // this rule the stream would poll a run that will never speak again.
    const store = await seed(completedRun().slice(0, 2), "completed");
    const frames = parseFrames(
      await drain(
        createRunEventStream({
          tasks: store.tasks,
          taskId: TASK_ID,
          startSeq: 0,
          options: options(),
        }),
      ),
    ).slice(1);
    expect(frames.map((f) => f.id)).toEqual(["evt-0", "evt-1"]);
  });

  it("(e) writes a heartbeat comment while a live run is idle", async () => {
    const store = await seed(completedRun().slice(0, 1), "running");
    const controller = new AbortController();
    const stream = createRunEventStream({
      tasks: store.tasks,
      taskId: TASK_ID,
      startSeq: 0,
      options: options({ pollIntervalMs: 2, heartbeatIntervalMs: 5 }),
      signal: controller.signal,
    });
    const reader = stream.getReader();
    const decoder = new TextDecoder();
    let text = "";
    const deadline = Date.now() + 2_000;
    while (!text.includes(": hb") && Date.now() < deadline) {
      const { done, value } = await reader.read();
      if (done) break;
      text += decoder.decode(value, { stream: true });
    }
    expect(text).toContain("id: evt-0");
    expect(text).toContain(": hb");

    controller.abort();
    await reader.cancel();
  });

  it("(f) stops cleanly when the request is aborted", async () => {
    const store = await seed(completedRun().slice(0, 1), "running");
    const controller = new AbortController();
    const stream = createRunEventStream({
      tasks: store.tasks,
      taskId: TASK_ID,
      startSeq: 0,
      options: options({ pollIntervalMs: 2, heartbeatIntervalMs: 60_000 }),
      signal: controller.signal,
    });
    const reader = stream.getReader();
    // The retry hint and the one replayed event arrive first.
    await reader.read();
    controller.abort();

    // Whatever was already queued drains, then the stream ends — it does not
    // hang on the poll timer the abort was supposed to clear.
    const finished = (async () => {
      for (;;) {
        const { done } = await reader.read();
        if (done) return true;
      }
    })();
    const raced = await Promise.race([
      finished,
      new Promise<boolean>((resolve) => setTimeout(() => resolve(false), 1_000)),
    ]);
    expect(raced).toBe(true);
  });

  it("(g) follows a run that is still being written", async () => {
    const store = await seed(completedRun().slice(0, 1), "running");
    const lease = await store.tasks.acquireLease({
      taskId: TASK_ID,
      attemptId: "att-2",
      ownerId: "owner",
      ttlMs: 60_000,
    });
    const stream = createRunEventStream({
      tasks: store.tasks,
      taskId: TASK_ID,
      startSeq: 0,
      options: options({ pollIntervalMs: 2 }),
    });
    const drained = drain(stream);
    // Appended AFTER the stream caught up: the cursor picks them up on the
    // next poll, which is the whole point of not subscribing.
    await new Promise((resolve) => setTimeout(resolve, 10));
    await store.tasks.appendEvents(
      TASK_ID,
      completedRun().slice(1) as TaskEventEnvelope[],
      { leaseToken: lease.leaseToken },
    );
    const frames = parseFrames(await drained).slice(1);
    expect(frames.map((f) => f.id)).toEqual([
      "evt-0",
      "evt-1",
      "evt-2",
      "evt-3",
      "evt-4",
    ]);
  });
});
