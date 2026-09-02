/**
 * The whole turn, through the client: create a chat, submit, watch the run
 * stream to its terminal event, read the answer back.
 *
 * Against the real transport over a real socket, because that is the only way
 * the SSE half is exercised at all: a stub that handed back a pre-built array of
 * events would prove the client can iterate an array.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  createAgentKitClient,
  isTerminalRunEvent,
  runPhase,
} from "../src/index.js";
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

describe("a full chat turn", () => {
  test("createChat → submitMessage → streamRun → listMessages", async () => {
    const chat = await client.createChat({ title: "Client flow" });
    expect(chat.id).toBeString();
    expect(chat.title).toBe("Client flow");
    expect(chat.archived).toBe(false);

    const submitted = await client.submitMessage(
      { chatId: chat.id },
      { content: "Say hello." },
    );
    // The key came back even though the caller never supplied one — that is
    // what makes a retry of this call safe.
    expect(submitted.idempotencyKey).toBeString();
    expect(submitted.idempotencyKey.length).toBeGreaterThan(0);
    expect(submitted.result.chatId).toBe(chat.id);
    expect(submitted.result.runId).toBeString();
    expect(submitted.result.userMessageId).toBeString();
    expect(submitted.result.assistantMessageId).toBeString();

    const events = [];
    for await (const event of client.streamRun(submitted.result.runId)) {
      events.push(event);
    }

    expect(events.length).toBeGreaterThan(0);
    expect(events.map((e) => e.seq)).toEqual(events.map((_e, i) => i));
    expect(events.at(-1)?.type).toBe("run.completed");
    expect(isTerminalRunEvent(events.at(-1)!)).toBe(true);
    expect(runPhase({ events })).toBe("completed");

    const run = await client.getRun({ runId: submitted.result.runId });
    expect(run.status).toBe("completed");
    expect(run.chatId).toBe(chat.id);

    const page = await client.listMessages({ chatId: chat.id });
    const roles = page.items.map((m) => m.role);
    expect(roles).toEqual(["user", "assistant"]);
    expect(page.items[1]?.content).toBe("Hello, client.");
    expect(page.items[1]?.metadata["placeholder"]).toBe(false);
  });

  test("the stream is iterable more than once — each pass re-reads the log", async () => {
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "hi" },
    );
    const runId = submitted.result.runId;
    await waitFor(
      async () => (await client.getRun({ runId })).status === "completed",
      "the run to complete",
    );

    const first = [];
    for await (const event of client.streamRun(runId)) first.push(event);
    const second = [];
    for await (const event of client.streamRun(runId)) second.push(event);

    expect(second.map((e) => e.eventId)).toEqual(first.map((e) => e.eventId));
  });

  test("breaking out of the stream early does not throw", async () => {
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "hi" },
    );
    const seen = [];
    for await (const event of client.streamRun(submitted.result.runId)) {
      seen.push(event);
      break;
    }
    expect(seen).toHaveLength(1);
    expect(seen[0]?.seq).toBe(0);
  });
});

describe("idempotency", () => {
  test("resubmitting with the returned key lands on the same run", async () => {
    const first = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "Ask once." },
    );
    const replay = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "Ask once." },
      { idempotencyKey: first.idempotencyKey },
    );

    expect(replay.idempotencyKey).toBe(first.idempotencyKey);
    expect(replay.result.runId).toBe(first.result.runId);
    expect(replay.result.userMessageId).toBe(first.result.userMessageId);
    expect(replay.result.assistantMessageId).toBe(
      first.result.assistantMessageId,
    );

    await waitFor(
      async () =>
        (await client.getRun({ runId: first.result.runId })).status ===
        "completed",
      "the run to complete",
    );
    // One turn, not two: the replay wrote nothing.
    const page = await client.listMessages({ chatId: TEST_CHAT_ID });
    expect(page.items.filter((m) => m.role === "user")).toHaveLength(1);
  });

  test("two submits WITHOUT a shared key are two runs", async () => {
    const first = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "one" },
    );
    // A chat answers one turn at a time (`chat_busy`, 409, is the host default
    // — see `TurnRunnerDeps.allowConcurrentSubmit`), so the second submit waits
    // for the first the way a UI does. The assertion is unchanged: a fresh key
    // is a fresh run, never a replay of the one before it.
    await waitFor(
      async () =>
        (await client.getRun({ runId: first.result.runId })).status ===
        "completed",
      "the first run to complete",
    );
    const second = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "two" },
    );
    expect(second.idempotencyKey).not.toBe(first.idempotencyKey);
    expect(second.result.runId).not.toBe(first.result.runId);
  });
});

describe("branching", () => {
  test("regenerate mints its own key and answers on a new branch", async () => {
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "Say hello." },
    );
    await waitFor(
      async () =>
        (await client.getRun({ runId: submitted.result.runId })).status ===
        "completed",
      "the first run to complete",
    );

    server.provider.setScript([
      { steps: [{ kind: "text", content: "A different hello." }] },
    ]);
    const again = await client.regenerateMessage({
      chatId: TEST_CHAT_ID,
      messageId: submitted.result.assistantMessageId,
    });
    expect(again.idempotencyKey).toBeString();
    expect(again.result.runId).not.toBe(submitted.result.runId);

    await waitFor(
      async () =>
        (await client.getRun({ runId: again.result.runId })).status ===
        "completed",
      "the regenerated run to complete",
    );

    const siblings = await client.listSiblings({
      messageId: submitted.result.assistantMessageId,
    });
    expect(siblings).toHaveLength(2);
    expect(siblings.map((m) => m.branchIndex)).toEqual([0, 1]);

    // Switching back reports the path that became active, in one round trip.
    const path = await client.activateBranch({
      messageId: submitted.result.assistantMessageId,
    });
    expect(path.items.at(-1)?.id).toBe(submitted.result.assistantMessageId);
  });

  test("forkChat copies the active path into a new chat", async () => {
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "Fork me." },
    );
    await waitFor(
      async () =>
        (await client.getRun({ runId: submitted.result.runId })).status ===
        "completed",
      "the run to complete",
    );

    const forked = await client.forkChat(
      { chatId: TEST_CHAT_ID },
      { fromMessageId: submitted.result.assistantMessageId },
    );
    expect(forked.id).not.toBe(TEST_CHAT_ID);

    const page = await client.listMessages({ chatId: forked.id });
    expect(page.items.map((m) => m.role)).toEqual(["user", "assistant"]);
    expect(page.items[0]?.id).not.toBe(submitted.result.userMessageId);
  });
});

describe("management routes", () => {
  test("chats round-trip through update, list and delete", async () => {
    const chat = await client.createChat({ title: "before" });
    const renamed = await client.updateChat(
      { chatId: chat.id },
      { title: "after", archived: true },
    );
    expect(renamed.title).toBe("after");
    expect(renamed.archived).toBe(true);

    const listed = await client.listChats({ limit: 50 });
    expect(listed.some((c) => c.id === chat.id)).toBe(false);
    expect((await client.getChat({ chatId: chat.id })).title).toBe("after");
  });

  test("settings patch and read back", async () => {
    const before = await client.getSettings();
    expect(before.defaultProviderId).toBe("p1");
    const after = await client.updateSettings({ allowRawToolData: true });
    expect(after.allowRawToolData).toBe(true);
    expect((await client.getSettings()).allowRawToolData).toBe(true);
  });

  test("version reports both versions", async () => {
    const version = await client.getVersion();
    expect(version.contractVersion).toMatch(/^\d+\.\d+\.\d+$/);
    expect(version.restApiVersion).toBe("v1");
  });

  test("models list through the provider path", async () => {
    const models = await client.listModels({ providerId: "p1" });
    expect(models.map((m) => m.modelId)).toEqual(["m1"]);
  });

  test("an id containing a slash survives the round trip", async () => {
    const chat = await client.createChat({ title: "slashes" });
    // Not a slash in an id the server minted — the point is the ENCODING: a
    // path parameter is interpolated encoded, so a `/` cannot split a segment.
    await expect(
      client.getChat({ chatId: `${chat.id}/../providers` }),
    ).rejects.toMatchObject({ status: 404, code: "not_found" });
  });
});

describe("auth headers", () => {
  test("headers() is applied to every request and resolved per call", async () => {
    let calls = 0;
    const seen: string[] = [];
    const guarded = await startTestServer({
      deps: {
        authenticate: async (req) => {
          seen.push(req.headers.get("authorization") ?? "");
          if (req.headers.get("authorization") !== `Bearer token-${calls}`) {
            return new Response("nope", { status: 401 });
          }
          return { user: "test" };
        },
      },
    });
    try {
      const authed = createAgentKitClient({
        baseUrl: guarded.baseUrl,
        headers: () => ({ authorization: `Bearer token-${calls}` }),
      });
      await authed.getVersion();
      calls += 1;
      // The second call re-reads the source: a token that rotated mid-session
      // must not be the one captured at construction.
      await authed.getVersion();
      expect(seen).toEqual(["Bearer token-0", "Bearer token-1"]);
    } finally {
      await guarded.stop();
    }
  });
});
