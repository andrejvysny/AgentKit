/**
 * `listToolEvents`, the one route with no record behind it.
 *
 * It is derived twice over — messages name the runs, the runs' logs hold the
 * `run.tool.*` events — so the assertions here are about the derivation: that
 * the chain finds the events at all, that each stage of a call survives as its
 * own row, and that the slim/full payload split is not collapsed on the way out.
 */
import { describe, expect, it } from "bun:test";
import {
  CONTRACT_VERSION,
  type TaskEventEnvelope,
  type ToolEventDto,
} from "@agentkit/contracts";
import {
  createHandlerFixture,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

const RUN_ID = "task-tools";

function toolEvent(
  seq: number,
  type: string,
  data: Record<string, unknown>,
): TaskEventEnvelope {
  return {
    type,
    seq,
    eventId: `evt-${seq}`,
    timestamp: new Date(seq * 1000).toISOString(),
    contractVersion: CONTRACT_VERSION,
    ...({ runId: RUN_ID, data } as object),
  } as TaskEventEnvelope;
}

describe("listToolEvents", () => {
  it("(a) projects each stage of a tool call from the run log", async () => {
    const { handler, store } = await createHandlerFixture();
    await store.tasks.createTask({
      taskId: RUN_ID,
      kind: "chat.turn",
      scopeId: TEST_CHAT_ID,
      payload: { chatId: TEST_CHAT_ID },
    });
    await store.conversations.appendMessage({
      chatId: TEST_CHAT_ID,
      runId: RUN_ID,
      role: "assistant",
      content: "",
    });
    const lease = await store.tasks.acquireLease({
      taskId: RUN_ID,
      attemptId: "att-1",
      ownerId: "owner",
      ttlMs: 60_000,
    });
    await store.tasks.appendEvents(
      RUN_ID,
      [
        toolEvent(0, "run.started", { model: "m1", toolCount: 1 }),
        toolEvent(1, "run.tool.requested", {
          toolCallId: "call-1",
          toolName: "notes_append",
          argumentsJson: '{"text":"hi"}',
        }),
        toolEvent(2, "run.tool.running", {
          toolCallId: "call-1",
          toolName: "notes_append",
        }),
        toolEvent(3, "run.tool.succeeded", {
          toolCallId: "call-1",
          toolName: "notes_append",
          resultJson: '{"ok":true,"data":{"noteId":"n1"}}',
          modelResultJson: '{"ok":true,"summary":"appended"}',
          summary: "appended",
          sources: [],
          warnings: ["clipped"],
          truncated: false,
        }),
      ],
      { leaseToken: lease.leaseToken },
    );

    const res = await handler(
      request("GET", `/v1/chats/${TEST_CHAT_ID}/tool-events`),
    );
    expect(res.status).toBe(200);
    const items = (await res.json()) as ToolEventDto[];

    // `run.started` is not a tool event; the three stages of the call are.
    expect(items.map((row) => row.status)).toEqual([
      "requested",
      "running",
      "succeeded",
    ]);
    expect(items.map((row) => row.id)).toEqual(["evt-1", "evt-2", "evt-3"]);
    for (const row of items) {
      expect(row.runId).toBe(RUN_ID);
      expect(row.chatId).toBe(TEST_CHAT_ID);
      expect(row.toolCallId).toBe("call-1");
      expect(row.toolName).toBe("notes_append");
    }
    expect(items[0]?.argumentsJson).toBe('{"text":"hi"}');
    // The slim envelope the model saw and the full payload for the UI both
    // survive, and are still distinguishable.
    expect(items[2]?.resultJson).toBe('{"ok":true,"data":{"noteId":"n1"}}');
    expect(items[2]?.modelResultJson).toBe('{"ok":true,"summary":"appended"}');
    expect(items[2]?.warnings).toEqual(["clipped"]);
    expect(items[2]?.truncated).toBe(false);

    // `limit` takes the END of the history — a bounded slice of "what happened"
    // means the most recent, still oldest-first.
    const limited = (await (
      await handler(
        request("GET", `/v1/chats/${TEST_CHAT_ID}/tool-events?limit=1`),
      )
    ).json()) as ToolEventDto[];
    expect(limited.map((row) => row.id)).toEqual(["evt-3"]);
  });

  it("(c) reads only the newest runs a bounded `limit` can be answered from", async () => {
    const { handler, store } = await createHandlerFixture();
    // Twelve turns, each with its own run and one tool call. The old walk
    // listed every message, then read every run's log in full, then sliced the
    // tail — so `?limit=1` cost twelve log reads to return one row, and a real
    // chat's thousand turns cost a thousand.
    const runIds = Array.from({ length: 12 }, (_, i) => `run-${i}`);
    for (const [index, runId] of runIds.entries()) {
      await store.tasks.createTask({
        taskId: runId,
        kind: "chat.turn",
        scopeId: TEST_CHAT_ID,
        payload: { chatId: TEST_CHAT_ID },
      });
      await store.conversations.appendMessage({
        chatId: TEST_CHAT_ID,
        role: "user",
        content: `q${index}`,
      });
      await store.conversations.appendMessage({
        chatId: TEST_CHAT_ID,
        runId,
        role: "assistant",
        content: "",
      });
      const lease = await store.tasks.acquireLease({
        taskId: runId,
        attemptId: `att-${index}`,
        ownerId: "owner",
        ttlMs: 60_000,
      });
      await store.tasks.appendEvents(
        runId,
        [
          toolEvent(0, "run.started", { model: "m1", toolCount: 1 }),
          toolEvent(1, "run.tool.succeeded", {
            toolCallId: `call-${index}`,
            toolName: "notes_append",
          }),
        ].map((event) => ({ ...event, eventId: `${runId}-${event.seq}` })),
        { leaseToken: lease.leaseToken },
      );
    }

    const readRuns: string[] = [];
    const realListEvents = store.tasks.listEvents.bind(store.tasks);
    store.tasks.listEvents = async (taskId, opts) => {
      readRuns.push(taskId);
      return realListEvents(taskId, opts);
    };

    const items = (await (
      await handler(
        request("GET", `/v1/chats/${TEST_CHAT_ID}/tool-events?limit=2`),
      )
    ).json()) as ToolEventDto[];

    // The most recent two, still oldest-first — the same answer as before.
    expect(items.map((row) => row.toolCallId)).toEqual(["call-10", "call-11"]);
    // …reached by reading the two newest runs' logs and stopping, rather than
    // all twelve.
    expect(new Set(readRuns)).toEqual(new Set(["run-11", "run-10"]));
  });

  it("(d) answers a chat with no `limit` from a bounded default", async () => {
    const { handler, store } = await createHandlerFixture();
    await store.tasks.createTask({
      taskId: RUN_ID,
      kind: "chat.turn",
      scopeId: TEST_CHAT_ID,
      payload: { chatId: TEST_CHAT_ID },
    });
    await store.conversations.appendMessage({
      chatId: TEST_CHAT_ID,
      runId: RUN_ID,
      role: "assistant",
      content: "",
    });
    const lease = await store.tasks.acquireLease({
      taskId: RUN_ID,
      attemptId: "att-1",
      ownerId: "owner",
      ttlMs: 60_000,
    });
    await store.tasks.appendEvents(
      RUN_ID,
      Array.from({ length: 250 }, (_, i) =>
        toolEvent(i, "run.tool.succeeded", {
          toolCallId: `call-${i}`,
          toolName: "notes_append",
        }),
      ),
      { leaseToken: lease.leaseToken },
    );

    const items = (await (
      await handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/tool-events`))
    ).json()) as ToolEventDto[];
    // 200 rows, and they are the LAST 200 — "no limit" is a default, not "all".
    expect(items.length).toBe(200);
    expect(items[0]?.toolCallId).toBe("call-50");
    expect(items[199]?.toolCallId).toBe("call-249");

    // And a limit past the ceiling is refused rather than silently clamped.
    const over = await handler(
      request("GET", `/v1/chats/${TEST_CHAT_ID}/tool-events?limit=5000`),
    );
    expect(over.status).toBe(400);
  });

  it("(b) 404s a chat that does not exist", async () => {
    const { handler } = await createHandlerFixture();
    const res = await handler(request("GET", "/v1/chats/nope/tool-events"));
    expect(res.status).toBe(404);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
  });
});
