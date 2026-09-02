/**
 * `drainRun` — the events a live stream cannot see.
 *
 * The host's correction harness appends `run.verification` to the durable log
 * after a pass has already emitted its terminal event. A stream that closed
 * before the append — the task went terminal, so the server said goodbye — never
 * receives it, however long its subscriber waits. This is the one resumed pass
 * that does.
 *
 * The trailing event is appended here directly rather than by wiring the
 * harness: what is under test is the CLIENT's ability to pick up a log that
 * grew past its terminal event, and the shortest thing that produces one is an
 * append. How the host comes to write it is the host's test.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  CONTRACT_VERSION,
  type AiRunEvent,
  type TaskEventEnvelope,
} from "@agentkit/contracts";
import { createAgentKitClient } from "../src/index.js";
import {
  startTestServer,
  TEST_CHAT_ID,
  waitFor,
  type TestServer,
} from "./support/server.js";

let server: TestServer;
let client: ReturnType<typeof createAgentKitClient>;

beforeEach(async () => {
  server = await startTestServer();
  client = createAgentKitClient({ baseUrl: server.baseUrl });
});

afterEach(async () => {
  await server.stop();
});

/** Append one event to a finished run's log, the way a late pass would. */
async function appendTrailing(
  runId: string,
  event: Omit<AiRunEvent, "contractVersion" | "eventId" | "seq">,
): Promise<AiRunEvent> {
  const seq = await server.store.tasks.nextSeq(runId);
  const full = {
    ...event,
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-trailing-${seq}`,
    seq,
  } as AiRunEvent;
  const lease = await server.store.tasks.acquireLease({
    taskId: runId,
    attemptId: `att-trailing-${seq}`,
    ownerId: "trailing-writer",
    ttlMs: 60_000,
  });
  await server.store.tasks.appendEvents(runId, [full as TaskEventEnvelope], {
    leaseToken: lease.leaseToken,
  });
  return full;
}

describe("drainRun", () => {
  test("picks up an event appended after the terminal one", async () => {
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "verify me" },
    );
    const runId = submitted.result.runId;

    const live: AiRunEvent[] = [];
    for await (const event of client.streamRun(runId)) live.push(event);
    expect(live.at(-1)?.type).toBe("run.completed");

    // The harness's late word, written after the stream had already closed.
    const trailing = await appendTrailing(runId, {
      type: "run.verification",
      runId,
      timestamp: new Date().toISOString(),
      data: { pass: 1, status: "partial", deficiencies: ["one thing"] },
    });

    const drained = await client.drainRun(runId, live.at(-1)?.eventId);
    expect(drained.map((e) => e.eventId)).toEqual([trailing.eventId]);
    expect(drained[0]?.type).toBe("run.verification");
    expect(drained[0]?.seq).toBe(live.length);
  });

  test("a stream re-opened after the append replays it, the log being the truth", async () => {
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "verify me" },
    );
    const runId = submitted.result.runId;
    const live: AiRunEvent[] = [];
    for await (const event of client.streamRun(runId)) live.push(event);
    await appendTrailing(runId, {
      type: "run.verification",
      runId,
      timestamp: new Date().toISOString(),
      data: { pass: 1, status: "pass", deficiencies: [] },
    });

    // The stream that was open at the time never saw it — it had already
    // closed, because the TASK was terminal.
    expect(live.some((e) => e.type === "run.verification")).toBe(false);

    // A stream opened now does see it: what the server replays is the log, and
    // a terminal run event in the middle of it is not a stopping point.
    const again: AiRunEvent[] = [];
    for await (const event of client.streamRun(runId)) again.push(event);
    expect(again.map((e) => e.type)).toEqual([
      ...live.map((e) => e.type),
      "run.verification",
    ]);
  });

  test("without a lastEventId it returns the whole log", async () => {
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "hi" },
    );
    const runId = submitted.result.runId;
    await waitFor(
      async () => (await client.getRun({ runId })).status === "completed",
      "the run to complete",
    );

    const stored = await server.store.tasks.listEvents(runId);
    const drained = await client.drainRun(runId);
    expect(drained.map((e) => e.eventId)).toEqual(
      [...stored].sort((a, b) => a.seq - b.seq).map((e) => e.eventId),
    );
  });

  test("an unknown run is the same typed 404 as everywhere else", async () => {
    await expect(client.drainRun("run-nope")).rejects.toMatchObject({
      status: 404,
      code: "not_found",
    });
  });
});
