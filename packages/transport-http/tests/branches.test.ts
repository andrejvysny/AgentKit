/**
 * The three branching routes, called as a client calls them.
 *
 * The interesting assertions are the negative ones. `forkChat` has two distinct
 * failures a client must be able to tell apart — a chat that is not there (404)
 * and a fork point that is not a fork point (400) — and collapsing them into one
 * status is how a client ends up retrying a request that will never succeed.
 */
import { describe, expect, it } from "bun:test";
import type { ChatDto, MessageDto, MessagePageDto } from "@agentkit/contracts";
import type { AssistantStore } from "@agentkit/host";
import {
  createHandlerFixture,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

async function expectProblem(
  res: Response,
  status: number,
  code: string,
): Promise<void> {
  expect(res.status).toBe(status);
  expect(res.headers.get("content-type")).toBe("application/problem+json");
  const body = (await res.json()) as Record<string, unknown>;
  expect(body["code"]).toBe(code);
  expect(body["status"]).toBe(status);
}

/**
 * A chat shaped `u1 → a1 → u2`, with a second answer `a2` branched under `u1`
 * and left INACTIVE — the off-path record every negative test below needs.
 */
async function seedTree(store: AssistantStore) {
  const u1 = await store.conversations.appendMessage({
    chatId: TEST_CHAT_ID,
    role: "user",
    content: "u1",
  });
  const a1 = await store.conversations.appendMessage({
    chatId: TEST_CHAT_ID,
    role: "assistant",
    content: "a1",
  });
  const u2 = await store.conversations.appendMessage({
    chatId: TEST_CHAT_ID,
    role: "user",
    content: "u2",
  });
  const a2 = await store.conversations.appendMessage({
    chatId: TEST_CHAT_ID,
    role: "assistant",
    content: "a2",
    parentMessageId: u1.id,
  });
  // Back to the first branch, leaving a2 off the path.
  await store.conversations.activatePath(u2.id);
  return { u1, a1, u2, a2 };
}

describe("message DTO", () => {
  it("(a) carries the branching fields, and omits depth", async () => {
    const { handler, store } = await createHandlerFixture();
    const { u1, a1 } = await seedTree(store);
    const res = await handler(
      request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`),
    );
    const page = (await res.json()) as MessagePageDto;
    const root = page.items.find((m) => m.id === u1.id);
    const child = page.items.find((m) => m.id === a1.id);
    expect(root?.parentMessageId).toBe(undefined);
    expect(root?.branchIndex).toBe(0);
    expect(root?.active).toBe(true);
    expect(child?.parentMessageId).toBe(u1.id);
    expect("depth" in (child ?? {})).toBe(false);
    // Only the active path is published — the branched-away answer is not here.
    expect(page.items.map((m) => m.content)).toEqual(["u1", "a1", "u2"]);
  });
});

describe("listSiblings", () => {
  it("(a) lists same-parent messages including self, branchIndex ascending", async () => {
    const { handler, store } = await createHandlerFixture();
    const { a1, a2 } = await seedTree(store);
    const res = await handler(request("GET", `/v1/messages/${a1.id}/siblings`));
    expect(res.status).toBe(200);
    const items = (await res.json()) as MessageDto[];
    expect(items.map((m) => m.id)).toEqual([a1.id, a2.id]);
    expect(items.map((m) => m.branchIndex)).toEqual([0, 1]);
    expect(items.map((m) => m.active)).toEqual([true, false]);
  });

  it("(b) 404s an unknown message", async () => {
    const { handler } = await createHandlerFixture();
    await expectProblem(
      await handler(request("GET", "/v1/messages/msg-nope/siblings")),
      404,
      "not_found",
    );
  });
});

describe("activateBranch", () => {
  it("(a) switches the path and answers with it", async () => {
    const { handler, store } = await createHandlerFixture();
    const { u1, a2 } = await seedTree(store);
    const res = await handler(
      request("POST", `/v1/messages/${a2.id}/activate`),
    );
    expect(res.status).toBe(200);
    const page = (await res.json()) as MessagePageDto;
    expect(page.items.map((m) => m.id)).toEqual([u1.id, a2.id]);
    expect(page.nextCursor).toBe(undefined);

    // And the switch stuck: the plain read agrees with what activate returned.
    const listed = await handler(
      request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`),
    );
    const after = (await listed.json()) as MessagePageDto;
    expect(after.items.map((m) => m.id)).toEqual(page.items.map((m) => m.id));
  });

  it("(b) 404s an unknown message", async () => {
    const { handler } = await createHandlerFixture();
    await expectProblem(
      await handler(request("POST", "/v1/messages/msg-nope/activate")),
      404,
      "not_found",
    );
  });
});

describe("forkChat", () => {
  it("(a) copies the active-path prefix into a new chat with 201", async () => {
    const { handler, store } = await createHandlerFixture();
    const { a1 } = await seedTree(store);
    const res = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/fork`, {
        body: { fromMessageId: a1.id },
      }),
    );
    expect(res.status).toBe(201);
    const chat = (await res.json()) as ChatDto;
    expect(chat.id).not.toBe(TEST_CHAT_ID);

    const listed = await handler(
      request("GET", `/v1/chats/${chat.id}/messages`),
    );
    const page = (await listed.json()) as MessagePageDto;
    expect(page.items.map((m) => m.content)).toEqual(["u1", "a1"]);
    expect(page.items.map((m) => m.parentMessageId)).toEqual([
      undefined,
      page.items[0]?.id,
    ]);
    // The source chat is untouched.
    const sourcePage = (await (
      await handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`))
    ).json()) as MessagePageDto;
    expect(sourcePage.items.map((m) => m.content)).toEqual(["u1", "a1", "u2"]);
  });

  it("(b) 400s an off-path fork point with invalid_fork_point", async () => {
    const { handler, store } = await createHandlerFixture();
    const { a2 } = await seedTree(store);
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/fork`, {
          body: { fromMessageId: a2.id },
        }),
      ),
      400,
      "invalid_fork_point",
    );
  });

  it("(c) 400s an unknown fork point, and 404s an unknown chat", async () => {
    const { handler, store } = await createHandlerFixture();
    const { a1 } = await seedTree(store);
    // The chat exists, the message does not: the request is answerable and
    // wrong, which is a 400.
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/fork`, {
          body: { fromMessageId: "msg-nope" },
        }),
      ),
      400,
      "invalid_fork_point",
    );
    // The chat does not exist: nothing to fork FROM, which is a 404.
    await expectProblem(
      await handler(
        request("POST", "/v1/chats/chat-nope/fork", {
          body: { fromMessageId: a1.id },
        }),
      ),
      404,
      "not_found",
    );
  });

  it("(d) rejects a body with no fromMessageId", async () => {
    const { handler } = await createHandlerFixture();
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/fork`, { body: {} }),
      ),
      400,
      "invalid_request",
    );
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/fork`, {
          body: { fromMessageId: "   " },
        }),
      ),
      400,
      "invalid_request",
    );
  });
});

describe("submitMessage — parentMessageId", () => {
  it("(a) branches the turn under the named message", async () => {
    const { handler, store } = await createHandlerFixture();
    const { u1, u2 } = await seedTree(store);
    const res = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content: "a different question", parentMessageId: u1.id },
        headers: { "idempotency-key": "branch-1" },
      }),
    );
    expect(res.status).toBe(201);

    const page = (await (
      await handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`))
    ).json()) as MessagePageDto;
    expect(page.items.map((m) => m.content)).toEqual([
      "u1",
      "a different question",
      "",
    ]);
    // The branch it replaced is off the path, not gone.
    expect(page.items.some((m) => m.id === u2.id)).toBe(false);
    const siblings = (await (
      await handler(request("GET", `/v1/messages/${u2.id}/siblings`))
    ).json()) as MessageDto[];
    expect(siblings.length).toBe(1);
  });

  it("(b) 404s a parentMessageId that is unknown or in another chat", async () => {
    const { handler, store } = await createHandlerFixture();
    await seedTree(store);
    const other = await store.conversations.createChat({});
    const elsewhere = await store.conversations.appendMessage({
      chatId: other.id,
      role: "user",
      content: "elsewhere",
    });
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "hi", parentMessageId: "msg-nope" },
          headers: { "idempotency-key": "branch-2" },
        }),
      ),
      404,
      "not_found",
    );
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "hi", parentMessageId: elsewhere.id },
          headers: { "idempotency-key": "branch-3" },
        }),
      ),
      404,
      "not_found",
    );
  });

  it("(c) rejects a non-string parentMessageId before anything is written", async () => {
    const { handler, store } = await createHandlerFixture();
    await seedTree(store);
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "hi", parentMessageId: 7 },
          headers: { "idempotency-key": "branch-4" },
        }),
      ),
      400,
      "invalid_request",
    );
    const page = (await (
      await handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`))
    ).json()) as MessagePageDto;
    expect(page.items.length).toBe(3);
  });
});
