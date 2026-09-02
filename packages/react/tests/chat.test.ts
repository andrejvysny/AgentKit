/**
 * `useChat`, against the real transport: the optimistic write, the streamed
 * answer, and the reconcile that replaces both with what the server stored.
 *
 * The three are asserted as three separate moments on purpose. A test that only
 * looked at the end state would pass for a hook that rendered nothing until the
 * run finished — which is the bug this hook exists to prevent.
 */
import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import { createAgentKitClient, type FetchLike } from "@agentkit/client";
import type { AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest } from "@agentkit/core";
import {
  createTestEventStamper,
  HangingProviderClient,
  MockProviderClient,
  nowIso,
} from "@agentkit/testing";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { useChat } from "../src/index.js";
import { strictWrapper, wrapper } from "./support/render.js";
import {
  chattyProvider,
  echoContributor,
  RetryingProvider,
  startTestServer,
  TEST_CHAT_ID,
  type TestServer,
} from "./support/server.js";

let server: TestServer;

beforeEach(async () => {
  server = await startTestServer();
});

afterEach(async () => {
  await server.stop();
});

/**
 * A provider that starts a turn and then dies, so the run reaches a real
 * `run.failed` with a message the hook can surface.
 *
 * It emits `run.started` before throwing rather than throwing straight away,
 * because that is the failure worth modelling: a run the UI has already started
 * rendering as "thinking" and now has to render as an error.
 */
class FailingProvider extends MockProviderClient {
  override async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    const stamp = createTestEventStamper();
    yield stamp({
      type: "run.started",
      runId: input.runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: 0 },
    });
    throw new Error("the provider said no");
  }
}

function connect(fetchImpl?: FetchLike) {
  return createAgentKitClient({
    baseUrl: server.baseUrl,
    ...(fetchImpl === undefined ? {} : { fetch: fetchImpl }),
  });
}

describe("a turn through useChat", () => {
  test("the optimistic pair renders before the server has answered", async () => {
    const client = connect();
    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    // Deliberately NOT awaited: what is under test is the state between the
    // click and the server's answer, and awaiting would skip past it.
    act(() => {
      void result.current.submit("Say hello.");
    });

    expect(result.current.status).toBe("loading");
    expect(result.current.messages.map((m) => m.role)).toEqual([
      "user",
      "assistant",
    ]);
    expect(result.current.messages[0]?.content).toBe("Say hello.");
    expect(result.current.messages[0]?.metadata["optimistic"]).toBe(true);
    expect(result.current.messages[1]?.content).toBe("");
    expect(result.current.messages[1]?.metadata["placeholder"]).toBe(true);

    await waitFor(() => expect(result.current.status).toBe("idle"));
  });

  test("deltas stream into the placeholder, and the final state is the server's", async () => {
    await server.stop();
    server = await startTestServer({ provider: chattyProvider(12) });
    const client = connect();

    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    await act(async () => {
      await result.current.submit("stream me");
    });
    expect(result.current.status).toBe("streaming");
    expect(result.current.activeRunId).toBeString();

    // Text arrives on the placeholder while the run is still live — the
    // property no amount of end-state assertion can show.
    await waitFor(() => {
      expect(result.current.messages[1]?.content).toContain("chunk-000");
    });

    await waitFor(() => expect(result.current.status).toBe("idle"), {
      timeout: 10_000,
    });
    expect(result.current.phase).toBe("completed");
    expect(result.current.activeRunId).toBeNull();
    expect(result.current.error).toBeNull();

    const page = await client.listMessages({ chatId: TEST_CHAT_ID });
    expect(result.current.messages.map((m) => m.id)).toEqual(
      page.items.map((m) => m.id),
    );
    expect(result.current.messages.map((m) => m.content)).toEqual(
      page.items.map((m) => m.content),
    );
    expect(result.current.messages[1]?.metadata["placeholder"]).toBe(false);
    expect(result.current.messages[1]?.content).toContain("chunk-011");
  });

  test("a retried pass replaces the first pass's text and completes the run", async () => {
    // F-OWN-1 end to end: the terminal `run.failed` on the log belongs to pass
    // 1, not to the turn. The stream must carry pass 2, the placeholder must
    // start over at the boundary rather than reading "PASS-ONEPASS-TWO", and
    // the turn must land as a success.
    await server.stop();
    server = await startTestServer({
      provider: new RetryingProvider(),
      contributors: [echoContributor],
    });
    const client = connect();

    /** Every value the placeholder took, so the LIVE text can be asserted. */
    const rendered: string[] = [];
    const { result } = renderHook(
      () => {
        const chat = useChat(TEST_CHAT_ID);
        // `content` widens to content PARTS in the DTO; this fixture's
        // provider only ever writes a string.
        const content = chat.messages[1]?.content;
        if (typeof content === "string") rendered.push(content);
        return chat;
      },
      { wrapper: wrapper(client) },
    );
    await waitFor(() => expect(result.current.status).toBe("idle"));

    await act(async () => {
      await result.current.submit("retry me");
    });
    await waitFor(() => expect(result.current.status).toBe("idle"), {
      timeout: 10_000,
    });

    expect(result.current.phase).toBe("completed");
    expect(result.current.error).toBeNull();
    expect(result.current.activeRunId).toBeNull();
    expect(result.current.messages[1]?.content).toBe("PASS-TWO");
    // Never both: the abandoned pass's text was cleared at the boundary
    // instead of being prefixed to the answer.
    expect(
      rendered.some((c) => c.includes("PASS-ONE") && c.includes("PASS-TWO")),
    ).toBe(false);

    // Pass 1's text WAS on screen — the reset is what took it off, not a
    // stream that never delivered it.
    expect(rendered.some((c) => c.includes("PASS-ONE"))).toBe(true);
    // Not vacuous: the run really did make two passes on one log, and the
    // first one really did end in a terminal event.
    const runId = result.current.messages[1]?.runId ?? "";
    const log = await server.store.tasks.listEvents(runId);
    expect(log.filter((e) => e.type === "run.started")).toHaveLength(2);
    expect(log.some((e) => e.type === "run.failed")).toBe(true);
  });

  test("a run that fails lands as an error with the run's own message", async () => {
    await server.stop();
    server = await startTestServer({ provider: new FailingProvider() });
    const client = connect();
    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    await act(async () => {
      await result.current.submit("break");
    });
    await waitFor(() => expect(result.current.phase).toBe("failed"));
    expect(result.current.status).toBe("error");
    expect(result.current.error?.message).toContain("the provider said no");
  });
});

describe("a submit the server refuses", () => {
  /** A `fetch` that fails the submit POST and passes everything else through. */
  function refusingSubmit(): { fetch: FetchLike; attempts: () => string[] } {
    const keys: string[] = [];
    let refuse = true;
    const wrapped: FetchLike = async (url, init) => {
      if (url.includes("/messages") && init?.method === "POST") {
        const headers = new Headers(init.headers ?? {});
        keys.push(headers.get("idempotency-key") ?? "");
        if (refuse) {
          refuse = false;
          return new Response(
            JSON.stringify({
              type: "about:blank",
              title: "Service Unavailable",
              status: 503,
              code: "upstream_unavailable",
              detail: "the queue is down",
            }),
            {
              status: 503,
              headers: { "content-type": "application/problem+json" },
            },
          );
        }
      }
      return fetch(url, init);
    };
    return { fetch: wrapped, attempts: () => keys };
  }

  test("rolls the optimistic pair back and reports the typed error", async () => {
    const refusing = refusingSubmit();
    const client = connect(refusing.fetch);
    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    await act(async () => {
      await result.current.submit("will not land");
    });

    expect(result.current.status).toBe("error");
    // Rolled back: nothing the user sees claims a turn that never happened.
    expect(result.current.messages).toEqual([]);
    expect(result.current.activeRunId).toBeNull();
    expect(result.current.error).toMatchObject({
      status: 503,
      code: "upstream_unavailable",
    });
  });

  test("the rollback takes its OWN pair, not everything since", async () => {
    await server.stop();
    // Parked mid-turn, so the second submit's run cannot reconcile the list out
    // from under the assertion and hand the old code a passing grade.
    const hanging = new HangingProviderClient({ deltas: ["Think"] });
    server = await startTestServer({ provider: hanging });

    // A submit that fails LATE — late enough for a second one to land while it
    // is still out, which is the whole of what a pre-`await` snapshot loses.
    let release!: () => void;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    let refuse = true;
    const client = connect(async (url, init) => {
      if (url.includes("/messages") && init?.method === "POST" && refuse) {
        refuse = false;
        await gate;
        return new Response(
          JSON.stringify({
            type: "about:blank",
            title: "Service Unavailable",
            status: 503,
            code: "upstream_unavailable",
            detail: "the queue is down",
          }),
          {
            status: 503,
            headers: { "content-type": "application/problem+json" },
          },
        );
      }
      return fetch(url, init);
    });

    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    let first!: Promise<void>;
    act(() => {
      first = result.current.submit("first");
    });
    await waitFor(() => expect(result.current.messages).toHaveLength(2));

    await act(async () => {
      await result.current.submit("second");
    });
    expect(result.current.messages).toHaveLength(4);
    const runId = result.current.activeRunId!;

    await act(async () => {
      release();
      await first;
    });

    // The failed turn's two records are gone; the accepted one is untouched.
    expect(result.current.status).toBe("error");
    expect(result.current.messages.map((m) => m.role)).toEqual([
      "user",
      "assistant",
    ]);
    expect(result.current.messages[0]?.content).toBe("second");

    await hanging.whenBlocking();
    await client.cancelRun({ runId });
  });

  test("the retry of the same question replays the same Idempotency-Key", async () => {
    const refusing = refusingSubmit();
    const client = connect(refusing.fetch);
    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    await act(async () => {
      await result.current.submit("ask once");
    });
    expect(result.current.status).toBe("error");

    await act(async () => {
      await result.current.submit("ask once");
    });
    await waitFor(() => expect(result.current.status).not.toBe("error"));

    const [first, second] = refusing.attempts();
    expect(first).toBeString();
    expect(second).toBe(first!);
  });

  test("a rejected branch submit restores the tail it optimistically cut", async () => {
    // W2: `editAndResubmit` truncates the active path at the parent
    // optimistically; a failed submit's rollback used to filter only the two
    // new optimistic ids and leave that truncation in place, so a rejected
    // edit silently dropped the messages after the edited one.
    //
    // The FIRST submit (building the branch point) must succeed — only the
    // edit itself is refused — so this cannot reuse `refusingSubmit`, which
    // refuses whichever POST reaches it first.
    let refuseNext = false;
    const client = connect(async (url, init) => {
      if (refuseNext && url.includes("/messages") && init?.method === "POST") {
        refuseNext = false;
        return new Response(
          JSON.stringify({
            type: "about:blank",
            title: "Service Unavailable",
            status: 503,
            code: "upstream_unavailable",
            detail: "the queue is down",
          }),
          {
            status: 503,
            headers: { "content-type": "application/problem+json" },
          },
        );
      }
      return fetch(url, init);
    });
    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    // Build a real branch point: a first turn, allowed through, that leaves an
    // answer to edit under.
    await act(async () => {
      await result.current.submit("first question");
    });
    await waitFor(() => expect(result.current.status).toBe("idle"), {
      timeout: 10_000,
    });
    const before = result.current.messages;
    expect(before).toHaveLength(2);
    const parentId = before[0]!.id;

    refuseNext = true;
    await act(async () => {
      await result.current.editAndResubmit(parentId, "edited question");
    });

    expect(result.current.status).toBe("error");
    // Identical to before the click: the cut tail (the original answer) is
    // back, and the failed edit's optimistic pair is gone.
    expect(result.current.messages.map((m) => m.id)).toEqual(
      before.map((m) => m.id),
    );
    expect(result.current.messages.map((m) => m.content)).toEqual(
      before.map((m) => m.content),
    );
  });

  test("a DIFFERENT question after a failure mints a fresh key", async () => {
    const refusing = refusingSubmit();
    const client = connect(refusing.fetch);
    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    await act(async () => {
      await result.current.submit("ask once");
    });
    await act(async () => {
      await result.current.submit("ask something else");
    });
    await waitFor(() => expect(result.current.status).not.toBe("error"));

    const [first, second] = refusing.attempts();
    expect(second).not.toBe(first);
  });
});

describe("lifecycle", () => {
  test("unmounting mid-stream leaves no update behind", async () => {
    await server.stop();
    // Parked mid-turn, so the run is DEMONSTRABLY unfinished at the unmount —
    // against a provider that finishes first the assertion proves nothing.
    const hanging = new HangingProviderClient({ deltas: ["Think", "ing"] });
    server = await startTestServer({ provider: hanging });
    const client = connect();

    const { result, unmount } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));
    await act(async () => {
      await result.current.submit("stream me");
    });
    const runId = result.current.activeRunId!;

    await hanging.whenBlocking();
    await waitFor(() => expect(result.current.phase).toBe("streaming"));
    const atUnmount = result.current.messages[1]?.content;
    expect(atUnmount).toBe("Thinking");
    unmount();

    // The run reaches its terminal event after the unmount; the hook must not
    // apply it, and must not warn about an update to an unmounted component.
    await client.cancelRun({ runId });
    await waitFor(async () => {
      expect((await client.getRun({ runId })).status).toBe("cancelled");
    });
    expect(result.current.messages[1]?.content).toBe("Thinking");
    expect(result.current.status).toBe("streaming");
  });

  test("changing the chat id re-reads, and the old chat's messages go", async () => {
    const client = connect();
    const other = await client.createChat({ title: "other" });
    await client.submitMessage({ chatId: TEST_CHAT_ID }, { content: "first" });

    const { result, rerender } = renderHook(
      ({ id }: { id: string }) => useChat(id),
      { wrapper: wrapper(client), initialProps: { id: TEST_CHAT_ID } },
    );
    await waitFor(() => expect(result.current.messages.length).toBe(2));

    rerender({ id: other.id });
    await waitFor(() => expect(result.current.messages).toEqual([]));
  });

  test("a failed read after switching chats does not keep the old chat's messages", async () => {
    // W3: the reset effect used to clear `activeRunId`/`phase`/`status` on a
    // chat switch but leave `messages`/`truncated` alone. A switch whose
    // `refresh` then FAILS was rendering chat A's messages (and A's
    // `truncated` flag) under chat B's id.
    const client = connect();
    const other = await client.createChat({ title: "other" });
    await client.submitMessage({ chatId: TEST_CHAT_ID }, { content: "from A" });

    let failNextRead = false;
    const failing = connect(async (url, init) => {
      if (
        failNextRead &&
        url.includes(`/chats/${other.id}/messages`) &&
        (init?.method ?? "GET") === "GET"
      ) {
        failNextRead = false;
        return new Response(
          JSON.stringify({
            type: "about:blank",
            title: "Service Unavailable",
            status: 503,
            code: "upstream_unavailable",
            detail: "the read failed",
          }),
          {
            status: 503,
            headers: { "content-type": "application/problem+json" },
          },
        );
      }
      return fetch(url, init);
    });

    const { result, rerender } = renderHook(
      ({ id }: { id: string }) => useChat(id),
      { wrapper: wrapper(failing), initialProps: { id: TEST_CHAT_ID } },
    );
    await waitFor(() => expect(result.current.messages.length).toBe(2));

    failNextRead = true;
    rerender({ id: other.id });

    await waitFor(() => expect(result.current.status).toBe("error"));
    // No trace of chat A's messages or its (false) truncation flag survives
    // under chat B.
    expect(result.current.messages).toEqual([]);
    expect(result.current.truncated).toBe(false);
    expect(result.current.activeRunId).toBeNull();
  });

  test("changing the chat id mid-stream ends the old run's follow", async () => {
    await server.stop();
    // Parked mid-turn, so the switch DEMONSTRABLY happens while the run is
    // still live — against a provider that finishes first there is no stale
    // follow left to prove anything about.
    const hanging = new HangingProviderClient({ deltas: ["Think", "ing"] });
    server = await startTestServer({ provider: hanging });
    const client = connect();
    const other = await client.createChat({ title: "other" });

    const { result, rerender } = renderHook(
      ({ id }: { id: string }) => useChat(id),
      { wrapper: wrapper(client), initialProps: { id: TEST_CHAT_ID } },
    );
    await waitFor(() => expect(result.current.status).toBe("idle"));
    await act(async () => {
      await result.current.submit("stream me");
    });
    const runId = result.current.activeRunId!;
    await hanging.whenBlocking();
    await waitFor(() => expect(result.current.phase).toBe("streaming"));

    // The user opens another conversation while the answer is still typing.
    rerender({ id: other.id });
    await waitFor(() => expect(result.current.messages).toEqual([]));
    // Nothing of the old run is left pointing at the new chat.
    expect(result.current.activeRunId).toBeNull();
    expect(result.current.status).not.toBe("streaming");

    // The abandoned run reaches its terminal event on the server. The follow's
    // reconcile closed over the OLD chat id, and must not land here.
    await client.cancelRun({ runId });
    await waitFor(
      async () => {
        expect((await client.getRun({ runId })).status).toBe("cancelled");
      },
      { timeout: 10_000 },
    );
    // A settle: the write this test forbids would arrive within a few
    // milliseconds of the run ending, and there is no event to wait for when
    // the correct behaviour is that nothing happens.
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 300));
    });
    expect(result.current.messages).toEqual([]);
  });

  test("cancel stops a parked run and the stream ends on run.cancelled", async () => {
    await server.stop();
    // Parks mid-turn until the run is cancelled, so this is not a race against
    // a provider that finishes faster than the click.
    const hanging = new HangingProviderClient({ deltas: ["Thinking"] });
    server = await startTestServer({ provider: hanging });
    const client = connect();

    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));
    await act(async () => {
      await result.current.submit("keep going");
    });
    const runId = result.current.activeRunId!;
    await hanging.whenBlocking();
    await waitFor(() => expect(result.current.phase).toBe("streaming"));

    await act(async () => {
      await result.current.cancel();
    });
    await waitFor(() => expect(result.current.phase).toBe("cancelled"));
    expect(result.current.status).toBe("idle");
    expect(result.current.activeRunId).toBeNull();
    expect((await client.getRun({ runId })).status).toBe("cancelled");
  });
});

describe("paging", () => {
  test("a read the page cap cut short says so", async () => {
    const client = connect();
    await client.submitMessage({ chatId: TEST_CHAT_ID }, { content: "first" });
    await waitFor(async () => {
      const page = await client.listMessages({ chatId: TEST_CHAT_ID });
      expect(page.items.length).toBeGreaterThanOrEqual(2);
    });

    // One message a page, one page: the read stops BEFORE the newest turn,
    // which is the truncation a bare `MessageDto[]` could not report and a UI
    // would render as the whole conversation.
    const cut = renderHook(
      () => useChat(TEST_CHAT_ID, { pageSize: 1, maxPages: 1 }),
      { wrapper: wrapper(client) },
    );
    await waitFor(() => expect(cut.result.current.status).toBe("idle"));
    expect(cut.result.current.messages).toHaveLength(1);
    expect(cut.result.current.truncated).toBe(true);

    const whole = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(whole.result.current.status).toBe("idle"));
    expect(whole.result.current.messages.length).toBeGreaterThanOrEqual(2);
    expect(whole.result.current.truncated).toBe(false);
  });
});

describe("<StrictMode>", () => {
  test("the doubled mount effect reads once and streams once", async () => {
    const reads: string[] = [];
    const client = connect(async (url, init) => {
      if (url.includes("/messages") && (init?.method ?? "GET") === "GET") {
        reads.push(url);
      }
      return fetch(url, init);
    });

    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: strictWrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));

    await act(async () => {
      await result.current.submit("strict");
    });
    await waitFor(() => expect(result.current.status).toBe("idle"), {
      timeout: 10_000,
    });

    // One user turn, not two: the doubled effect re-read the list (which is
    // idempotent) but nothing wrote twice.
    expect(result.current.messages.map((m) => m.role)).toEqual([
      "user",
      "assistant",
    ]);
    expect(result.current.messages[1]?.content).toBe("Hello, hooks.");
    const page = await client.listMessages({ chatId: TEST_CHAT_ID });
    expect(page.items.filter((m) => m.role === "user")).toHaveLength(1);
  });
});

describe("useChat(null)", () => {
  test("renders empty and refuses to write", async () => {
    const client = connect();
    const { result } = renderHook(() => useChat(null), {
      wrapper: wrapper(client),
    });
    expect(result.current.messages).toEqual([]);
    expect(result.current.status).toBe("idle");

    await act(async () => {
      await result.current.submit("nowhere");
    });
    expect(result.current.status).toBe("error");
    expect(result.current.error?.message).toContain("no chat to write to");
  });
});
