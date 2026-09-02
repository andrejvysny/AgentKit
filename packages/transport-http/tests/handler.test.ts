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
  AiContentPart,
  ChatDto,
  MessagePageDto,
  RunDto,
  SubmitMessageResponse,
  VersionDto,
} from "@agentkit/contracts";
import { CONTRACT_VERSION, REST_API_VERSION } from "@agentkit/contracts";
import type { SubmitMessageInput } from "@agentkit/host";
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

  it("(g2) 422s a key replayed with a different body, and writes nothing", async () => {
    const { handler, store } = await createHandlerFixture();
    const send = (body: unknown): Promise<Response> =>
      handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body,
          headers: { "idempotency-key": "key-reused" },
        }),
      );

    expect((await send({ content: "first question" })).status).toBe(201);

    // The failure this exists to stop: a 200 carrying the FIRST turn's ids,
    // with the second message discarded in silence.
    await expectProblem(
      await send({ content: "an entirely different question" }),
      422,
      "idempotency_key_mismatch",
    );
    // Model and provider count as part of the request too.
    await expectProblem(
      await send({ content: "first question", model: "m-other" }),
      422,
      "idempotency_key_mismatch",
    );
    // …and so does metadata.
    await expectProblem(
      await send({ content: "first question", metadata: { source: "cli" } }),
      422,
      "idempotency_key_mismatch",
    );

    // A genuine retry — same body, keys in a different order — still replays.
    const replay = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { metadata: {}, content: "first question" },
        headers: { "idempotency-key": "key-reused" },
      }),
    );
    expect(replay.status).toBe(200);

    // One user message, from the one request that was accepted.
    const stored = await store.conversations.listMessages(TEST_CHAT_ID);
    expect(stored.filter((m) => m.role === "user").length).toBe(1);
  });

  it("accepts a content-parts body, stores it, and projects it back unchanged", async () => {
    const { handler, store } = await createHandlerFixture();
    const content: AiContentPart[] = [
      { type: "text", text: "what is on this board?" },
      { type: "image", source: { kind: "ref", ref: "blob:sha256-abc" } },
      {
        type: "image",
        source: { kind: "data", base64: "aGVsbG8=", mediaType: "image/png" },
        detail: "high",
      },
    ];
    const res = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content },
        headers: { "idempotency-key": "key-parts" },
      }),
    );
    expect(res.status).toBe(201);

    // Stored as parts, not flattened on the way in.
    const stored = await store.conversations.listMessages(TEST_CHAT_ID);
    expect(stored.find((m) => m.role === "user")?.content).toEqual(content);

    // And projected back verbatim — the ref included. A transport that resolved
    // refs here would inline the host's blobs into every page of the chat.
    const page = (await (
      await handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`))
    ).json()) as MessagePageDto;
    expect(page.items.find((m) => m.role === "user")?.content).toEqual(content);
  });

  it("rejects a malformed content-parts body without creating anything", async () => {
    const { handler, store } = await createHandlerFixture();
    const send = (content: unknown): Promise<Response> =>
      handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content },
          headers: { "idempotency-key": `key-${Math.random()}` },
        }),
      );

    // The part union is CLOSED: an unknown type is a 400, not content the
    // server persists and no provider can ever be shown.
    await expectProblem(
      await send([{ type: "audio", url: "https://example.test/a.mp3" }]),
      400,
      "invalid_request",
    );
    await expectProblem(
      await send([{ type: "image", source: { kind: "elsewhere" } }]),
      400,
      "invalid_request",
    );
    // A media type that would break out of the `data:` URL an adapter builds.
    await expectProblem(
      await send([
        {
          type: "image",
          source: {
            kind: "data",
            base64: "aGVsbG8=",
            mediaType: "image/png;x=1,y",
          },
        },
      ]),
      400,
      "invalid_request",
    );
    // A `url` source reaches the provider verbatim, and the provider client
    // runs in the HOST's network position — so the scheme is the contract's
    // (`IMAGE_URL_PATTERN`), enforced here rather than restated.
    for (const url of [
      "file:///etc/passwd",
      "javascript:alert(1)",
      "data:image/png;base64,aGk=",
    ]) {
      await expectProblem(
        await send([{ type: "image", source: { kind: "url", url } }]),
        400,
        "invalid_request",
      );
    }

    // The empty body is the empty STRING; `[]` is a caller bug.
    await expectProblem(await send([]), 400, "invalid_request");

    expect(await store.conversations.listMessages(TEST_CHAT_ID)).toEqual([]);
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

  it("(k2) pages the stream's resume scan at the configured readBatchSize", async () => {
    const { handler, store } = await createHandlerFixture({
      streaming: { readBatchSize: 3, pollIntervalMs: 2 },
    });
    const runId = "task-batch";
    await store.tasks.createTask({
      taskId: runId,
      kind: "chat.turn",
      scopeId: TEST_CHAT_ID,
      payload: { chatId: TEST_CHAT_ID },
    });
    const lease = await store.tasks.acquireLease({
      taskId: runId,
      attemptId: "att-1",
      ownerId: "owner",
      ttlMs: 60_000,
    });
    await store.tasks.appendEvents(
      runId,
      Array.from({ length: 12 }, (_, seq) => ({
        type: seq === 11 ? "run.completed" : "run.message.delta",
        runId,
        seq,
        eventId: `evt-${seq}`,
        timestamp: new Date(seq * 1000).toISOString(),
        contractVersion: CONTRACT_VERSION,
        data: {},
      })) as never,
      { leaseToken: lease.leaseToken },
    );
    await store.tasks.transitionTask(runId, ["queued"], "running");
    await store.tasks.transitionTask(runId, ["running"], "completed");

    const limits: (number | undefined)[] = [];
    const real = store.tasks.listEvents.bind(store.tasks);
    store.tasks.listEvents = async (taskId, opts) => {
      limits.push(opts?.limit);
      return real(taskId, opts);
    };

    const res = await handler(
      request("GET", `/v1/runs/${runId}/stream`, {
        // Unknown id: the scan walks the whole log before giving up, which is
        // the read the batch size is supposed to bound.
        headers: { "last-event-id": "evt-from-another-run" },
      }),
    );
    await res.text();

    // Every read — the resume scan's as well as the pump's — is the size the
    // deployment configured. Before, the scan silently used the default.
    expect(limits.length).toBeGreaterThan(1);
    expect(limits.every((limit) => limit === 3)).toBe(true);
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

    const scopes: (string | undefined)[] = [];
    const wired = await createHandlerFixture({
      toolCatalog: {
        async listTools(scope) {
          scopes.push(scope?.chatId);
          return [
            {
              namespace: "notes",
              definition: {
                name: "notes_read",
                version: "1.0.0",
                effect: "read",
                capability: "notes.read",
                description: "Read a note.",
                inputSchema: { type: "object", properties: {} },
              },
            },
          ];
        },
      },
    });
    const res = await wired.handler(request("GET", "/v1/tools"));
    expect(res.status).toBe(200);
    const body = (await res.json()) as Record<string, unknown>[];
    expect(body[0]?.["name"]).toBe("notes_read");
    // The route names no chat, so it must not invent one — and the entry's
    // host-side `namespace` is attribution, not part of `ToolDefinitionDto`.
    expect(scopes).toEqual([undefined]);
    expect(body[0]).not.toHaveProperty("namespace");
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

describe("submitMessage — provider override", () => {
  it("(r0) threads `providerId` through to the host's submit input", async () => {
    const inputs: SubmitMessageInput[] = [];
    const { handler } = await createHandlerFixture({
      turns: {
        async submitMessage(input) {
          inputs.push(input);
          return {
            chatId: input.chatId,
            runId: "run-1",
            userMessageId: "user-1",
            assistantMessageId: "assistant-1",
          };
        },
        async regenerate() {
          throw new Error("not used here");
        },
      },
    });

    const res = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content: "Hi", providerId: "p-other", model: "m9" },
        headers: { "idempotency-key": "key-provider" },
      }),
    );
    expect(res.status).toBe(201);
    expect(inputs[0]?.providerId).toBe("p-other");
    expect(inputs[0]?.model).toBe("m9");

    // Absent, it is omitted rather than sent as `undefined` — the host reads
    // "no override", not "an override nobody named".
    await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content: "Hi" },
        headers: { "idempotency-key": "key-no-provider" },
      }),
    );
    expect(inputs[1]).not.toHaveProperty("providerId");
  });

  it("(r0b) 400s a `providerId` that is not a string", async () => {
    const { handler } = await createHandlerFixture();
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "Hi", providerId: 7 },
          headers: { "idempotency-key": "key-bad-provider" },
        }),
      ),
      400,
      "invalid_request",
    );
  });
});

describe("maxBodyBytes", () => {
  const big = { content: "x".repeat(500) };

  it("(r) 413s a body over the cap and serves one under it", async () => {
    const { handler } = await createHandlerFixture({ maxBodyBytes: 200 });

    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: big,
          headers: { "idempotency-key": "key-big" },
        }),
      ),
      413,
      "body_too_large",
    );

    const small = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content: "Hi" },
        headers: { "idempotency-key": "key-small" },
      }),
    );
    expect(small.status).toBe(201);
  });

  it("(r2) measures a body that declares no Content-Length, and leaves it readable", async () => {
    const { handler, store } = await createHandlerFixture({
      maxBodyBytes: 200,
    });
    const streamed = (body: unknown): Request =>
      new Request(`http://rest.test/v1/chats/${TEST_CHAT_ID}/messages`, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          "idempotency-key": `key-stream-${JSON.stringify(body).length}`,
        },
        // A ReadableStream body carries no Content-Length, so the cap has to
        // measure it rather than read a header.
        body: new ReadableStream<Uint8Array>({
          start(controller) {
            controller.enqueue(new TextEncoder().encode(JSON.stringify(body)));
            controller.close();
          },
        }),
        // Required by Node/undici for a streaming request body.
        duplex: "half",
      } as RequestInit);

    await expectProblem(await handler(streamed(big)), 413, "body_too_large");

    const ok = await handler(streamed({ content: "streamed hello" }));
    expect(ok.status).toBe(201);
    // Re-wrapping the request did not cost the route its body.
    const stored = await store.conversations.listMessages(TEST_CHAT_ID);
    expect(
      stored.some((m) => JSON.stringify(m.content).includes("streamed hello")),
    ).toBe(true);
  });

  it("(r3) defaults to 1 MiB rather than to no cap at all", async () => {
    const { handler } = await createHandlerFixture();
    // Everything in the contract except an inline image fits well inside it…
    const res = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: big,
        headers: { "idempotency-key": "key-default-small" },
      }),
    );
    expect(res.status).toBe(201);

    // …and a deployment that forgot the option no longer lets an anonymous
    // request make this process buffer a body of any size.
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "x".repeat(2 * 1024 * 1024) },
          headers: { "idempotency-key": "key-default-huge" },
        }),
      ),
      413,
      "body_too_large",
    );

    // A host that accepts inline images raises it, and the raise is honoured.
    const raised = await createHandlerFixture({
      maxBodyBytes: 8 * 1024 * 1024,
    });
    expect(
      (
        await raised.handler(
          request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
            body: { content: "x".repeat(2 * 1024 * 1024) },
            headers: { "idempotency-key": "key-raised" },
          }),
        )
      ).status,
    ).toBe(201);
  });
});

describe("request bounds", () => {
  it("(r4) 400s a body nested past the depth anything downstream can walk", async () => {
    const { handler } = await createHandlerFixture();
    const nest = (depth: number): unknown => {
      let value: unknown = "bottom";
      for (let i = 0; i < depth; i++) value = [value];
      return value;
    };

    // A few hundred bytes of `[` is a stack overflow somewhere in the chain
    // that walks this body — validation, `structuredClone`, the store's own
    // serialization — and which frame it lands in depends on the runtime.
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
          body: { content: "Hi", metadata: { deep: nest(500) } },
          headers: { "idempotency-key": "key-deep" },
        }),
      ),
      400,
      "invalid_body",
    );
    // A metadata bag of ordinary shape is untouched.
    expect(
      (
        await handler(
          request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
            body: { content: "Hi", metadata: { deep: nest(8) } },
            headers: { "idempotency-key": "key-shallow" },
          }),
        )
      ).status,
    ).toBe(201);
  });

  it("(r5) 400s a `limit` above the page ceiling instead of clamping it", async () => {
    const { handler } = await createHandlerFixture();
    for (const path of [
      "/v1/chats?limit=100000",
      `/v1/chats/${TEST_CHAT_ID}/messages?limit=1001`,
      `/v1/chats/${TEST_CHAT_ID}/tool-events?limit=2000`,
    ]) {
      await expectProblem(
        await handler(request("GET", path)),
        400,
        "invalid_request",
      );
    }
    // Refused, not clamped: a client that asked for 100000 rows and silently
    // got 1000 pages forever off a cursor it thinks it has already passed.
    expect((await handler(request("GET", "/v1/chats?limit=1000"))).status).toBe(
      200,
    );
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
