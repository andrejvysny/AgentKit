import { describe, expect, it } from "bun:test";
import type { SubmitMessageResponse } from "@agentkit/contracts";
import {
  createHandlerFixture,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

async function setup() {
  const environment = await createHandlerFixture();
  const root = await environment.store.conversations.appendMessage({
    chatId: TEST_CHAT_ID,
    role: "user",
    content: "Original",
  });
  const answer = await environment.store.conversations.appendMessage({
    chatId: TEST_CHAT_ID,
    role: "assistant",
    content: "Old answer",
  });
  const submit = (
    key: string,
    parent: { parentMessageId?: string | null } = {},
  ) =>
    environment.handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        headers: { "Idempotency-Key": key },
        body: { content: "Rewritten", ...parent },
      }),
    );
  return { ...environment, root, answer, submit };
}

describe("explicit root submit", () => {
  it("creates a sibling root and replays its key without extra records", async () => {
    const f = await setup();
    const accepted = await f.submit("root", { parentMessageId: null });
    expect(accepted.status).toBe(201);
    const first = (await accepted.json()) as SubmitMessageResponse;
    const user = await f.store.conversations.getMessage(first.userMessageId);
    expect(user?.parentMessageId).toBeUndefined();
    expect(user?.depth).toBe(0);
    expect(
      (await f.store.conversations.getMessage(first.assistantMessageId))
        ?.parentMessageId,
    ).toBe(first.userMessageId);
    expect(
      (await f.store.conversations.listMessages(TEST_CHAT_ID)).map(
        (message) => message.id,
      ),
    ).toEqual([first.userMessageId, first.assistantMessageId]);
    expect(
      (await f.store.conversations.listSiblings(first.userMessageId)).map(
        (message) => message.id,
      ),
    ).toEqual([f.root.id, first.userMessageId]);
    const replay = await f.submit("root", { parentMessageId: null });
    expect(replay.status).toBe(200);
    expect(await replay.json()).toEqual(first);
    expect(
      await f.store.conversations.listSiblings(first.userMessageId),
    ).toHaveLength(2);
  });

  it("does not collapse an explicit root key into an omitted-parent request", async () => {
    const f = await setup();
    expect((await f.submit("root", { parentMessageId: null })).status).toBe(
      201,
    );
    const replay = await f.submit("root");
    expect(replay.status).toBe(422);
    expect(await replay.json()).toMatchObject({
      code: "idempotency_key_mismatch",
    });
  });

  it("does not collapse an append key into an explicit root request", async () => {
    const f = await setup();
    const accepted = await f.submit("append");
    expect(accepted.status).toBe(201);
    const first = (await accepted.json()) as SubmitMessageResponse;
    expect(
      (await f.store.conversations.getMessage(first.userMessageId))
        ?.parentMessageId,
    ).toBe(f.answer.id);
    expect((await f.submit("append", { parentMessageId: null })).status).toBe(
      422,
    );
    expect((await f.submit("append")).status).toBe(200);
  });
});
