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

  it("(b) 404s a chat that does not exist", async () => {
    const { handler } = await createHandlerFixture();
    const res = await handler(request("GET", "/v1/chats/nope/tool-events"));
    expect(res.status).toBe(404);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
  });
});
