/**
 * The routes, called as a client calls them: `new Request` in, `Response` out,
 * no socket.
 *
 * The idempotency assertions are the reason this file exists. Everything else
 * here is a projection with a status code; `submitMessage` is the one call
 * where getting it wrong answers a user twice.
 */
import { describe, expect, it } from "bun:test";
import type {
  ChatDto,
  MessagePageDto,
  RunDto,
  SubmitMessageResponse,
  VersionDto,
} from "@agentkit/contracts";
import { CONTRACT_VERSION, REST_API_VERSION } from "@agentkit/contracts";
import {
  createHandlerFixture,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

/** Every non-2xx body in this contract is a problem document; assert it is. */
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
  expect(body["type"]).toBe(`https://agentkit.dev/problems/${code}`);
  expect(typeof body["title"]).toBe("string");
}

describe("chats", () => {
  it("(a) creates a chat with 201 and reads it back", async () => {
    const { handler } = await createHandlerFixture();
    const created = await handler(
      request("POST", "/v1/chats", { body: { title: "Planning" } }),
    );
    expect(created.status).toBe(201);
    const chat = (await created.json()) as ChatDto;
    expect(chat.title).toBe("Planning");

    const fetched = await handler(request("GET", `/v1/chats/${chat.id}`));
    expect(fetched.status).toBe(200);
    expect(((await fetched.json()) as ChatDto).id).toBe(chat.id);

    const listed = await handler(request("GET", "/v1/chats"));
    const chats = (await listed.json()) as ChatDto[];
    expect(chats.some((row) => row.id === chat.id)).toBe(true);
  });

  it("(b) rejects a body whose field has the wrong type", async () => {
    const { handler } = await createHandlerFixture();
    const res = await handler(
      request("POST", "/v1/chats", { body: { title: 42 } }),
    );
    await expectProblem(res, 400, "invalid_request");
  });

  it("(c) rejects a body that is not JSON", async () => {
    const { handler } = await createHandlerFixture();
    const res = await handler(
      new Request("http://rest.test/v1/chats", {
        method: "POST",
        body: "{nope",
        headers: { "content-type": "application/json" },
      }),
    );
    await expectProblem(res, 400, "invalid_body");
  });

  it("(d) 404s an unknown chat", async () => {
    const { handler } = await createHandlerFixture();
    await expectProblem(
      await handler(request("GET", "/v1/chats/missing")),
      404,
      "not_found",
    );
  });

  it("(d2) pages messages forward through an opaque cursor", async () => {
    const { handler, store } = await createHandlerFixture();
    for (const content of ["one", "two", "three"]) {
      await store.conversations.appendMessage({
        chatId: TEST_CHAT_ID,
        role: "user",
        content,
      });
    }

    const first = (await (
      await handler(
        request("GET", `/v1/chats/${TEST_CHAT_ID}/messages?limit=2`),
      )
    ).json()) as MessagePageDto;
    expect(first.items.map((m) => m.content)).toEqual(["one", "two"]);
    expect(first.nextCursor).toBeDefined();
    // The store's ordering key is not published; the cursor is the only handle.
    expect(Object.keys(first.items[0] ?? {})).not.toContain("orderKey");

    const second = (await (
      await handler(
        request(
          "GET",
          `/v1/chats/${TEST_CHAT_ID}/messages?limit=2&cursor=${first.nextCursor ?? ""}`,
        ),
      )
    ).json()) as MessagePageDto;
    expect(second.items.map((m) => m.content)).toEqual(["three"]);
    expect(second.nextCursor).toBeUndefined();

    // A cursor this server never issued is an error, not a silent page one.
    await expectProblem(
      await handler(
        request("GET", `/v1/chats/${TEST_CHAT_ID}/messages?cursor=42`),
      ),
      400,
      "invalid_request",
    );
  });
});

describe("submitMessage", () => {
  it("(e) refuses a submit with no Idempotency-Key", async () => {
    const { handler } = await createHandlerFixture();
    const res = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content: "Hi" },
      }),
    );
    await expectProblem(res, 400, "idempotency_key_required");
  });

  it("(f) is 201 first, 200 on replay, and identical either way", async () => {
    const { handler, runner } = await createHandlerFixture();
    const send = (): Promise<Response> =>
      handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "Hi" },
          headers: { "idempotency-key": "key-1" },
        }),
      );

    const first = await send();
    expect(first.status).toBe(201);
    const created = (await first.json()) as SubmitMessageResponse;

    const replay = await send();
    expect(replay.status).toBe(200);
    expect(await replay.json()).toEqual(created);

    // The replay re-pokes the queue: the rescue for a first submit that
    // committed and then died before it could enqueue.
    expect(runner.enqueued).toEqual([created.runId, created.runId]);

    // And it wrote the turn exactly once.
    const page = (await (
      await handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`))
    ).json()) as MessagePageDto;
    expect(page.items.filter((m) => m.role === "user").length).toBe(1);
  });

  it("(g) gives a different key a different run", async () => {
    const { handler } = await createHandlerFixture();
    const send = (key: string): Promise<Response> =>
      handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "Hi" },
          headers: { "idempotency-key": key },
        }),
      );
    const one = (await (await send("key-a")).json()) as SubmitMessageResponse;
    const two = (await (await send("key-b")).json()) as SubmitMessageResponse;
    expect(one.runId).not.toBe(two.runId);
    expect(one.runId.startsWith("task_ik_")).toBe(true);
  });

  it("(h) validates the body before creating anything", async () => {
    const { handler, store } = await createHandlerFixture();
    const res = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { model: "m1" },
        headers: { "idempotency-key": "key-bad" },
      }),
    );
    await expectProblem(res, 400, "invalid_request");
    expect(await store.conversations.listMessages(TEST_CHAT_ID)).toEqual([]);
  });

  it("(i) 404s a submit to a chat that does not exist", async () => {
    const { handler, store } = await createHandlerFixture();
    const res = await handler(
      request("POST", "/v1/chats/nope/messages", {
        body: { content: "Hi" },
        headers: { "idempotency-key": "key-c" },
      }),
    );
    await expectProblem(res, 404, "not_found");
    // No orphan task row: the chat is checked before the submit transaction.
    expect(store.tasks.tasks.size).toBe(0);
  });
});

describe("runs", () => {
  it("(j) projects a queued run and cancels it", async () => {
    const { handler } = await createHandlerFixture();
    const submitted = (await (
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "Hi" },
          headers: { "idempotency-key": "key-run" },
        }),
      )
    ).json()) as SubmitMessageResponse;

    const read = await handler(request("GET", `/v1/runs/${submitted.runId}`));
    expect(read.status).toBe(200);
    const run = (await read.json()) as RunDto;
    expect(run).toMatchObject({
      runId: submitted.runId,
      chatId: TEST_CHAT_ID,
      scopeId: TEST_CHAT_ID,
      status: "queued",
    });
    expect(typeof run.createdAt).toBe("string");
    // Queue internals stay behind the contract.
    expect(Object.keys(run)).not.toContain("payload");
    expect(Object.keys(run)).not.toContain("attemptCount");

    const cancelled = await handler(
      request("POST", `/v1/runs/${submitted.runId}/cancel`),
    );
    expect(cancelled.status).toBe(202);
    expect(((await cancelled.json()) as RunDto).status).toBe("cancelled");
  });

  it("(k) 404s an unknown run and a task that is not a chat turn", async () => {
    const { handler, store } = await createHandlerFixture();
    await expectProblem(
      await handler(request("GET", "/v1/runs/missing")),
      404,
      "not_found",
    );
    await store.tasks.createTask({
      taskId: "task-index-1",
      kind: "notes.reindex",
      scopeId: "notes",
      payload: { path: "/notes" },
    });
    await expectProblem(
      await handler(request("GET", "/v1/runs/task-index-1")),
      404,
      "not_found",
    );
  });
});

describe("catalogue", () => {
  it("(l) never publishes a provider credential", async () => {
    const { handler } = await createHandlerFixture();
    const res = await handler(request("GET", "/v1/providers"));
    expect(res.status).toBe(200);
    const text = await res.text();
    expect(text).not.toContain("sk-should-never-be-published");
    const providers = JSON.parse(text) as Record<string, unknown>[];
    expect(providers[0]).toEqual({
      id: "p1",
      label: "Mock",
      kind: "openai-compatible",
      baseUrl: "http://localhost:1234",
      defaultModel: "m1",
      enabled: true,
    });
  });

  it("(m) lists a provider's models and 404s an unknown provider", async () => {
    const { handler } = await createHandlerFixture();
    const models = (await (
      await handler(request("GET", "/v1/providers/p1/models"))
    ).json()) as { modelId: string }[];
    expect(models.map((m) => m.modelId)).toEqual(["m1"]);
    await expectProblem(
      await handler(request("GET", "/v1/providers/nope/models")),
      404,
      "not_found",
    );
  });

  it("(n) reports both versions", async () => {
    const { handler } = await createHandlerFixture();
    const version = (await (
      await handler(request("GET", "/v1/version"))
    ).json()) as VersionDto;
    expect(version).toEqual({
      contractVersion: CONTRACT_VERSION,
      restApiVersion: REST_API_VERSION,
    });
  });

  it("(o) 501s listTools without a catalogue, serves it with one", async () => {
    const bare = await createHandlerFixture();
    await expectProblem(
      await bare.handler(request("GET", "/v1/tools")),
      501,
      "not_implemented",
    );

    const wired = await createHandlerFixture({
      toolCatalog: async () => [
        {
          name: "notes_read",
          version: "1.0.0",
          effect: "read",
          capability: "notes.read",
          description: "Read a note.",
          inputSchema: { type: "object", properties: {} },
        },
      ],
    });
    const res = await wired.handler(request("GET", "/v1/tools"));
    expect(res.status).toBe(200);
    expect(((await res.json()) as { name: string }[])[0]?.name).toBe(
      "notes_read",
    );
  });

  it("(p) 501s the proposal decision routes when no service is wired", async () => {
    const { handler, store } = await createHandlerFixture();
    // The proposal must exist, or the 404 would mask the 501 we are asserting.
    await store.proposals.create({
      id: "prp-1",
      chatId: TEST_CHAT_ID,
      scopeKey: TEST_CHAT_ID,
      toolName: "notes_append",
      kind: "notes.append",
      risk: "medium",
      envelope: { summary: "Append a note" },
      operations: [{ op: "append" }],
      warnings: [],
      truncated: false,
      createdAt: new Date(0).toISOString(),
    });
    await expectProblem(
      await handler(
        request("POST", "/v1/proposals/prp-1/approve", { body: {} }),
      ),
      501,
      "not_implemented",
    );

    const listed = await handler(
      request("GET", `/v1/chats/${TEST_CHAT_ID}/proposals`),
    );
    expect(listed.status).toBe(200);
    const proposals = (await listed.json()) as Record<string, unknown>[];
    expect(proposals[0]).toMatchObject({
      id: "prp-1",
      status: "pending",
      summary: "Append a note",
    });
    // The write's host-shaped body never crosses the wire.
    expect(Object.keys(proposals[0] ?? {})).not.toContain("operations");
    expect(Object.keys(proposals[0] ?? {})).not.toContain("envelope");
  });
});

describe("authenticate", () => {
  it("(q) returns the host's own response verbatim", async () => {
    const { handler } = await createHandlerFixture({
      authenticate: async (req) =>
        req.headers.get("authorization") === "Bearer ok"
          ? { userId: "u1" }
          : new Response("nope", { status: 401 }),
    });
    expect((await handler(request("GET", "/v1/version"))).status).toBe(401);
    const allowed = await handler(
      request("GET", "/v1/version", {
        headers: { authorization: "Bearer ok" },
      }),
    );
    expect(allowed.status).toBe(200);
  });
});
