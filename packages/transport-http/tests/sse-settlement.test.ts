import { describe, expect, it } from "bun:test";
import { CONTRACT_VERSION, type TaskEventEnvelope } from "@agentkit/contracts";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { resolveStreamOptions } from "../src/deps.js";
import { createRunEventStream } from "../src/sse.js";

const TASK_ID = "settlement-task";

function completedRun(): TaskEventEnvelope[] {
  return [0, 1].map((seq) => ({
    type: seq === 0 ? "run.started" : "run.completed",
    seq,
    eventId: `evt-${seq}`,
    runId: TASK_ID,
    timestamp: new Date(0).toISOString(),
    contractVersion: CONTRACT_VERSION,
    data: {},
  }));
}

async function seed(
  events: TaskEventEnvelope[],
  status = "completed",
): Promise<MemoryAssistantStore> {
  const store = new MemoryAssistantStore();
  await store.tasks.createTask({
    taskId: TASK_ID,
    kind: "chat.turn",
    scopeId: "chat-settlement",
    payload: {},
  });
  const lease = await store.tasks.acquireLease({
    taskId: TASK_ID,
    attemptId: "attempt",
    ownerId: "owner",
    ttlMs: 60_000,
  });
  await store.tasks.appendEvents(TASK_ID, events, {
    leaseToken: lease.leaseToken,
  });
  await store.tasks.transitionTask(TASK_ID, ["queued"], "running");
  if (status === "completed")
    await store.tasks.transitionTask(TASK_ID, ["running"], "completed");
  return store;
}

function options(): ReturnType<typeof resolveStreamOptions> {
  return resolveStreamOptions({ pollIntervalMs: 2 });
}

function parseFrames(text: string): Record<string, string>[] {
  return text
    .trim()
    .split("\n\n")
    .map((block) =>
      Object.fromEntries(
        block.split("\n").map((line) => {
          const colon = line.indexOf(":");
          return [line.slice(0, colon), line.slice(colon + 1).trimStart()];
        }),
      ),
    );
}

async function drain(stream: ReadableStream<Uint8Array>): Promise<string> {
  return new Response(stream).text();
}

describe("run stream settlement control frames", () => {
  it("marks completed logs only after every durable event", async () => {
    const store = await seed(completedRun());
    const frames = parseFrames(
      await drain(
        createRunEventStream({
          tasks: store.tasks,
          taskId: TASK_ID,
          startSeq: 0,
          options: options(),
        }),
      ),
    );
    expect(
      frames.filter((frame) => frame.id !== undefined).map((frame) => frame.id),
    ).toEqual(["evt-0", "evt-1"]);
    expect(frames.at(-1)).toEqual({
      event: "agentkit.stream.settled",
      data: "{}",
    });
  });
  it.each(["interrupted", "missing"] as const)(
    "marks an exhausted %s task as settled",
    async (status) => {
      const store = await seed(completedRun().slice(0, 2), "running");
      const task = await store.tasks.getTask(TASK_ID);
      store.tasks.getTask = async () =>
        status === "missing" ? null : { ...task!, status: "interrupted" };
      const frames = parseFrames(
        await drain(
          createRunEventStream({
            tasks: store.tasks,
            taskId: TASK_ID,
            startSeq: 0,
            options: options(),
          }),
        ),
      );
      expect(
        frames
          .filter((frame) => frame.id !== undefined)
          .map((frame) => frame.id),
      ).toEqual(["evt-0", "evt-1"]);
      expect(frames.at(-1)).toEqual({
        event: "agentkit.stream.settled",
        data: "{}",
      });
    },
  );

  it("never marks a terminal task settled when its final drain fails", async () => {
    const store = await seed(completedRun());
    const realList = store.tasks.listEvents.bind(store.tasks);
    let reads = 0;
    store.tasks.listEvents = async (taskId, options) => {
      if (++reads === 2) throw new Error("final drain failed");
      return realList(taskId, options);
    };
    const stream = createRunEventStream({
      tasks: store.tasks,
      taskId: TASK_ID,
      startSeq: 0,
      options: options(),
    });
    let text = "";
    const reader = stream.getReader();
    const reading = async (): Promise<void> => {
      try {
        for (;;) {
          const chunk = await reader.read();
          if (chunk.done) return;
          text += new TextDecoder().decode(chunk.value);
        }
      } finally {
        reader.releaseLock();
      }
    };
    await expect(reading()).rejects.toThrow("final drain failed");
    expect(text).not.toContain("agentkit.stream.settled");
  });
});
