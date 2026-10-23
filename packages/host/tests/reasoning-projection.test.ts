import { expect, test } from "bun:test";
import { createEventStamper, type AiRunEventDraft } from "@agentkit/core";
import { createRunProjector } from "../src/index.js";
import { createHarness } from "./fakes.js";

async function fixture() {
  const { store, clock } = createHarness();
  await store.conversations.createChat({ id: "chat" });
  const task = await store.tasks.createTask({
    taskId: "run",
    kind: "test",
    scopeId: "chat",
    payload: {},
  });
  const assistant = await store.conversations.appendMessage({
    chatId: "chat",
    role: "assistant",
    content: "",
    metadata: { placeholder: true, hostOwned: "keep" },
  });
  const attempt = await store.tasks.createAttempt({
    taskId: "run",
    attemptId: "attempt",
    ownerId: "owner",
  });
  const lease = await store.tasks.acquireLease({
    taskId: "run",
    attemptId: attempt.attemptId,
    ownerId: "owner",
    ttlMs: 30_000,
  });
  const ctx = {
    task,
    attemptId: attempt.attemptId,
    leaseToken: lease.leaseToken,
  };
  const projector = createRunProjector({ store, clock });
  const input = { chatId: "chat", assistantMessageId: assistant.id };
  const state = projector.createState(input);
  const stamp = createEventStamper();
  const project = async (draft: AiRunEventDraft) => {
    const event = stamp(draft);
    await projector.project(ctx, state, event);
    return event;
  };
  const message = () => store.conversations.getMessage(assistant.id);
  return { store, ctx, projector, input, state, project, message };
}

const base = { runId: "run", timestamp: "2026-01-01T00:00:00Z" };

test("reasoning is coalesced, reconciled once per provider turn, and replayed idempotently", async () => {
  const f = await fixture();
  const first = await f.project({
    ...base,
    type: "run.message.reasoning-delta",
    data: { delta: "Think Ž🙂" },
  });
  await f.projector.project(f.ctx, f.state, first);
  await f.project({
    ...base,
    type: "run.message.completed",
    data: {
      content: "",
      toolCallCount: 1,
      reasoningContent: "Think Ž🙂 harder",
      toolCalls: [{ id: "call", name: "lookup", argumentsJson: "{}" }],
    },
  });
  await f.project({
    ...base,
    type: "run.message.reasoning-delta",
    data: { delta: "Then" },
  });
  await f.project({
    ...base,
    type: "run.message.completed",
    data: {
      content: "Answer",
      toolCallCount: 0,
      reasoningContent: "Then finish",
    },
  });
  expect((await f.message())?.metadata).toEqual({
    placeholder: true,
    hostOwned: "keep",
    reasoningContent: "Think Ž🙂 harderThen finish",
  });
  expect((await f.message())?.content).toBe("Answer");
  const state = f.projector.createState(f.input);
  await f.projector.replay(f.ctx, state);
  await f.projector.replay(f.ctx, state);
  expect((await f.message())?.metadata.reasoningContent).toBe(
    "Think Ž🙂 harderThen finish",
  );
  expect(
    (await f.store.tasks.listEvents("run")).filter(
      (event) => event.type === "run.message.reasoning-delta",
    ),
  ).toHaveLength(2);
});

test("partial reasoning flushes on failure and a retry boundary clears it", async () => {
  const f = await fixture();
  await f.project({
    ...base,
    type: "run.message.reasoning-delta",
    data: { delta: "Partial" },
  });
  await f.project({
    ...base,
    type: "run.failed",
    data: { errorMessage: "Disconnected" },
  });
  expect((await f.message())?.metadata.reasoningContent).toBe("Partial");
  await f.project({
    ...base,
    type: "run.warning",
    data: { code: "retry_pass", message: "Retry" },
  });
  expect((await f.message())?.metadata.reasoningContent).toBe("");
  await f.project({
    ...base,
    type: "run.message.completed",
    data: { content: "Done", toolCallCount: 0, reasoningContent: "New" },
  });
  expect((await f.message())?.metadata.reasoningContent).toBe("New");
});
