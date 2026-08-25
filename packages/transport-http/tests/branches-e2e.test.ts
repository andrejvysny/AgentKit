/**
 * Branching over a real socket, with the real queue and worker behind it.
 *
 * The handler tests next door prove the routes; this one proves the sequence a
 * product actually performs — ask, ask again, rewrite the second question, watch
 * the new turn stream, and fork the surviving prefix into its own chat. It is
 * the only place where a branch submit and a live run meet, which is where a
 * store that switched the path AFTER the worker read history would show up.
 */
import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import type {
  ChatDto,
  MessageDto,
  MessagePageDto,
  SubmitMessageResponse,
} from "@agentkit/contracts";
import { MockProviderClient } from "@agentkit/testing";
import { serveRest } from "../src/index.js";
import {
  createLiveFixture,
  waitFor,
  type LiveFixture,
} from "./support/fixture.js";

let live: LiveFixture;
let server: ReturnType<typeof Bun.serve>;
let origin: string;

beforeAll(async () => {
  const provider = new MockProviderClient();
  provider.setScript([
    { steps: [{ kind: "text", content: "answer one" }] },
    { steps: [{ kind: "text", content: "answer two" }] },
    { steps: [{ kind: "text", content: "answer three" }] },
  ]);
  live = await createLiveFixture(provider);
  server = Bun.serve({ port: 0, ...serveRest(live.deps) });
  origin = `http://localhost:${server.port}`;
});

afterAll(async () => {
  await live.stop();
  await server.stop(true);
});

async function post(
  path: string,
  body?: unknown,
  headers: Record<string, string> = {},
): Promise<Response> {
  return fetch(`${origin}${path}`, {
    method: "POST",
    headers: {
      ...(body === undefined ? {} : { "content-type": "application/json" }),
      ...headers,
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
}

async function json<T>(res: Response): Promise<T> {
  return (await res.json()) as T;
}

/** Submit a turn and wait for the worker to land it. */
async function turn(
  chatId: string,
  key: string,
  body: Record<string, unknown>,
): Promise<SubmitMessageResponse> {
  const res = await post(`/v1/chats/${chatId}/messages`, body, {
    "idempotency-key": key,
  });
  expect(res.status).toBe(201);
  const submitted = await json<SubmitMessageResponse>(res);
  // Streamed to its terminal event, not merely polled: this is the assertion
  // that a branch submit produces a run a client can watch like any other.
  const stream = await fetch(`${origin}/v1/runs/${submitted.runId}/stream`);
  expect(stream.headers.get("content-type")).toBe("text/event-stream");
  await stream.text();
  await waitFor(
    async () =>
      (await live.store.tasks.getTask(submitted.runId))?.status === "completed",
    `run ${submitted.runId} to settle`,
  );
  return submitted;
}

async function path(chatId: string): Promise<MessageDto[]> {
  const res = await fetch(`${origin}/v1/chats/${chatId}/messages`);
  return (await json<MessagePageDto>(res)).items;
}

describe("branching over a real socket", () => {
  it("(a) edits a question, streams the new branch, switches back, then forks", async () => {
    const chatId = (
      await json<ChatDto>(await post("/v1/chats", { title: "Branching e2e" }))
    ).id;

    const first = await turn(chatId, "e2e-branch-1", {
      content: "first question",
    });
    const second = await turn(chatId, "e2e-branch-2", {
      content: "second question",
    });
    expect((await path(chatId)).map((m) => m.content)).toEqual([
      "first question",
      "answer one",
      "second question",
      "answer two",
    ]);

    // Rewrite the second question: it hangs off the answer before it, so it
    // becomes a sibling of the question it replaces.
    const edited = await turn(chatId, "e2e-branch-3", {
      content: "second question, edited",
      parentMessageId: first.assistantMessageId,
    });
    const branched = await path(chatId);
    expect(branched.map((m) => m.content)).toEqual([
      "first question",
      "answer one",
      "second question, edited",
      "answer three",
    ]);
    // The run really did answer the NEW branch, not a rerun of the old one.
    expect(
      branched.find((m) => m.id === edited.assistantMessageId)?.content,
    ).toBe("answer three");
    expect(branched.every((m) => m.active === true)).toBe(true);

    // Both questions are still there, as siblings; the client can see which one
    // it is looking at.
    const siblings = await json<MessageDto[]>(
      await fetch(`${origin}/v1/messages/${second.userMessageId}/siblings`),
    );
    expect(siblings.map((m) => m.content)).toEqual([
      "second question",
      "second question, edited",
    ]);
    expect(siblings.map((m) => m.branchIndex)).toEqual([0, 1]);
    expect(siblings.map((m) => m.active)).toEqual([false, true]);

    // Switch back to the original, and the answer that went with it comes back
    // too — the descent follows the branch, not just the message named.
    const reactivated = await json<MessagePageDto>(
      await post(`/v1/messages/${second.userMessageId}/activate`),
    );
    expect(reactivated.items.map((m) => m.content)).toEqual([
      "first question",
      "answer one",
      "second question",
      "answer two",
    ]);

    // Fork the shared prefix into a chat of its own. It copies the ACTIVE path,
    // which is now the original branch again.
    const forked = await json<ChatDto>(
      await post(`/v1/chats/${chatId}/fork`, {
        fromMessageId: first.assistantMessageId,
      }),
    );
    expect(forked.id).not.toBe(chatId);
    expect(forked.title).toBe("Fork of Branching e2e");
    const forkPath = await path(forked.id);
    expect(forkPath.map((m) => m.content)).toEqual([
      "first question",
      "answer one",
    ]);
    expect(forkPath.map((m) => m.runId)).toEqual([undefined, undefined]);
    expect(forkPath.map((m) => m.parentMessageId)).toEqual([
      undefined,
      forkPath[0]?.id,
    ]);
    // Independent: the source still has its four messages on the path.
    expect((await path(chatId)).length).toBe(4);
  });
});
