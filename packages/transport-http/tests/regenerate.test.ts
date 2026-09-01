/**
 * `POST /v1/chats/:chatId/messages/:messageId/regenerate`.
 *
 * The idempotency assertions are why this file is separate from the CRUD: this
 * is the second of the two routes that creates a run, and the one place where a
 * client's key has to mean something different from the same key on a submit.
 */
import { describe, expect, it } from "bun:test";
import type {
  MessageDto,
  MessagePageDto,
  SubmitMessageResponse,
} from "@agentkit/contracts";
import {
  createHandlerFixture,
  createLiveFixture,
  request,
  TEST_CHAT_ID,
  waitFor,
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
}

/** Submit one turn; the fixture's queue runs nothing, so it stays queued. */
async function submit(
  handler: (req: Request) => Promise<Response>,
  key: string,
): Promise<SubmitMessageResponse> {
  const res = await handler(
    request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
      body: { content: "why is the sky blue?" },
      headers: { "idempotency-key": key },
    }),
  );
  expect(res.status).toBe(201);
  return (await res.json()) as SubmitMessageResponse;
}

function regenerate(
  chatId: string,
  messageId: string,
  key: string,
  body: Record<string, unknown> = {},
): Request {
  return request(
    "POST",
    `/v1/chats/${chatId}/messages/${messageId}/regenerate`,
    { body, headers: { "idempotency-key": key } },
  );
}

describe("regenerateMessage", () => {
  it("(a) 400s without an Idempotency-Key", async () => {
    const { handler } = await createHandlerFixture();
    const first = await submit(handler, "k1");
    const res = await handler(
      request(
        "POST",
        `/v1/chats/${TEST_CHAT_ID}/messages/${first.assistantMessageId}/regenerate`,
        { body: {} },
      ),
    );
    await expectProblem(res, 400, "idempotency_key_required");
  });

  it("(b) 201s a new branch under the same question, leaving the old answer inactive", async () => {
    const f = await createHandlerFixture();
    const first = await submit(f.handler, "k1");

    const res = await f.handler(
      regenerate(TEST_CHAT_ID, first.assistantMessageId, "r1"),
    );
    expect(res.status).toBe(201);
    const again = (await res.json()) as SubmitMessageResponse;
    // The question is named, not re-created: the same user message id comes
    // back, because a regenerate re-answers rather than re-asks.
    expect(again.userMessageId).toBe(first.userMessageId);
    expect(again.assistantMessageId).not.toBe(first.assistantMessageId);
    expect(again.runId).not.toBe(first.runId);

    const siblings = (await (
      await f.handler(
        request("GET", `/v1/messages/${first.assistantMessageId}/siblings`),
      )
    ).json()) as MessageDto[];
    expect(siblings.map((m) => m.id)).toEqual([
      first.assistantMessageId,
      again.assistantMessageId,
    ]);
    expect(siblings.map((m) => m.branchIndex)).toEqual([0, 1]);
    expect(siblings.map((m) => m.active)).toEqual([false, true]);

    // And the conversation now reads as the new branch.
    const page = (await (
      await f.handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`))
    ).json()) as MessagePageDto;
    expect(page.items.map((m) => m.id)).toEqual([
      first.userMessageId,
      again.assistantMessageId,
    ]);
  });

  it("(c) replays one key: 200 with the identical body, and no second branch", async () => {
    const f = await createHandlerFixture();
    const first = await submit(f.handler, "k1");

    const once = (await (
      await f.handler(regenerate(TEST_CHAT_ID, first.assistantMessageId, "r1"))
    ).json()) as SubmitMessageResponse;
    const replay = await f.handler(
      regenerate(TEST_CHAT_ID, first.assistantMessageId, "r1"),
    );
    expect(replay.status).toBe(200);
    expect((await replay.json()) as SubmitMessageResponse).toEqual(once);

    const siblings = (await (
      await f.handler(
        request("GET", `/v1/messages/${first.assistantMessageId}/siblings`),
      )
    ).json()) as MessageDto[];
    expect(siblings.length).toBe(2);
  });

  it("(d) a submit and a regenerate under ONE key land on different runs", async () => {
    const f = await createHandlerFixture();
    const first = await submit(f.handler, "shared-key");

    // The two derivations are domain-separated by their id PREFIX, so even a
    // key crafted to collide inside the hash cannot make a regenerate answer
    // with a submit's run — which would hand a caller a turn it never made.
    const again = (await (
      await f.handler(
        regenerate(TEST_CHAT_ID, first.assistantMessageId, "shared-key"),
      )
    ).json()) as SubmitMessageResponse;
    expect(again.runId).not.toBe(first.runId);
    expect(again.runId.startsWith("task_rk_")).toBe(true);
    expect(first.runId.startsWith("task_ik_")).toBe(true);
  });

  it("(e) two different targets under one key are two different runs", async () => {
    const f = await createHandlerFixture();
    const first = await submit(f.handler, "k1");
    const second = await submit(f.handler, "k2");

    const a = (await (
      await f.handler(regenerate(TEST_CHAT_ID, first.assistantMessageId, "r"))
    ).json()) as SubmitMessageResponse;
    const b = (await (
      await f.handler(regenerate(TEST_CHAT_ID, second.assistantMessageId, "r"))
    ).json()) as SubmitMessageResponse;
    expect(a.runId).not.toBe(b.runId);
  });

  it("(f) 404s an unknown target and one in another chat; 400s a question", async () => {
    const f = await createHandlerFixture();
    const first = await submit(f.handler, "k1");
    await f.store.conversations.createChat({ id: "chat-2" });

    await expectProblem(
      await f.handler(regenerate(TEST_CHAT_ID, "no-such-message", "r1")),
      404,
      "not_found",
    );
    await expectProblem(
      await f.handler(regenerate("chat-2", first.assistantMessageId, "r2")),
      404,
      "not_found",
    );
    // A user message answered no question, so there is nothing to ask again.
    await expectProblem(
      await f.handler(regenerate(TEST_CHAT_ID, first.userMessageId, "r3")),
      400,
      "invalid_regenerate",
    );
  });

  it("(g) 400s a mistyped body field", async () => {
    const f = await createHandlerFixture();
    const first = await submit(f.handler, "k1");
    await expectProblem(
      await f.handler(
        regenerate(TEST_CHAT_ID, first.assistantMessageId, "r1", {
          model: 42,
        }),
      ),
      400,
      "invalid_request",
    );
  });

  it("(h) runs the new branch to completion over the real queue", async () => {
    const live = await createLiveFixture();
    try {
      const first = (await (
        await live.handler(
          request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
            body: { content: "why is the sky blue?" },
            headers: { "idempotency-key": "k1" },
          }),
        )
      ).json()) as SubmitMessageResponse;
      await waitFor(
        async () =>
          (await live.store.tasks.getTask(first.runId))?.status === "completed",
        "the first turn to complete",
      );

      live.provider.setScript([
        { steps: [{ kind: "text", content: "A different answer." }] },
      ]);
      const again = (await (
        await live.handler(
          regenerate(TEST_CHAT_ID, first.assistantMessageId, "r1"),
        )
      ).json()) as SubmitMessageResponse;
      await waitFor(
        async () =>
          (await live.store.tasks.getTask(again.runId))?.status === "completed",
        "the regenerated branch to complete",
      );

      const page = (await (
        await live.handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`))
      ).json()) as MessagePageDto;
      expect(page.items.map((m) => m.content)).toEqual([
        "why is the sky blue?",
        "A different answer.",
      ]);

      // The first answer is still there, off-path, and switching back to it is
      // an ordinary branch activation — which is the whole reason a regenerate
      // appends rather than rewrites.
      const switched = (await (
        await live.handler(
          request("POST", `/v1/messages/${first.assistantMessageId}/activate`),
        )
      ).json()) as MessagePageDto;
      expect(switched.items.map((m) => m.content)).toEqual([
        "why is the sky blue?",
        "Hello from the mock.",
      ]);
    } finally {
      await live.stop();
    }
  });
});
