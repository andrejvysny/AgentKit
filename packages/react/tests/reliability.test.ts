import "./support/dom.js";
import { act, renderHook, waitFor } from "@testing-library/react";
import { createAgentKitClient, type FetchLike } from "@agentkit/client";
import { HangingProviderClient } from "@agentkit/testing";
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { useChat, useRun } from "../src/index.js";
import { strictWrapper, wrapper } from "./support/render.js";
import {
  scriptedEvent,
  scriptedStreamFetch,
  startTestServer,
  TEST_CHAT_ID,
  type TestServer,
} from "./support/server.js";

let server: TestServer;
let provider: HangingProviderClient;
beforeEach(async () => {
  provider = new HangingProviderClient({ deltas: ["Thinking"] });
  server = await startTestServer({ provider });
});
afterEach(async () => {
  await server.stop();
});

function connect(fetchImpl?: FetchLike) {
  return createAgentKitClient({
    baseUrl: server.baseUrl,
    ...(fetchImpl === undefined ? {} : { fetch: fetchImpl }),
  });
}

describe("headless submission ownership", () => {
  test("provider completion remains settling and busy until host settlement", async () => {
    let hostSettled = false;
    let statusReads = 0;
    const scripted = scriptedStreamFetch({
      events: (runId) => [
        scriptedEvent(runId, 0, "run.completed", {
          iterations: 1,
          finishReason: "stop",
        }),
      ],
    });
    const client = connect(async (url, init) => {
      if (/\/runs\/[^/?]+$/.test(url) && (init?.method ?? "GET") === "GET") {
        statusReads += 1;
        const response = await fetch(url, init);
        const run = (await response.json()) as Record<string, unknown>;
        return Response.json({
          ...run,
          status: hostSettled ? "completed" : "running",
        });
      }
      return scripted.fetch(url, init);
    });
    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));
    await act(async () => {
      await result.current.submit("settle");
    });
    await waitFor(() => expect(statusReads).toBeGreaterThan(0));
    expect(result.current.phase).toBe("settling");
    expect(result.current.status).toBe("streaming");
    const original = result.current.activeRunId!;
    expect(original).toBeString();
    await act(async () => {
      hostSettled = true;
    });
    await waitFor(() => expect(result.current.phase).toBe("completed"));
    expect(result.current.activeRunId).toBeNull();
    await provider.whenBlocking();
    await client.cancelRun({ runId: original });
  });

  test("clean EOF reconciles failure after a completed provider pass", async () => {
    const scripted = scriptedStreamFetch({
      runStatus: "failed",
      events: (runId) => [
        scriptedEvent(runId, 0, "run.completed", {
          iterations: 1,
          finishReason: "stop",
        }),
        scriptedEvent(runId, 1, "run.verification", {
          pass: 1,
          status: "partial",
          deficiencies: ["invalid result"],
        }),
      ],
    });
    const client = connect(scripted.fetch);
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "verify" },
    );
    const { result } = renderHook(() => useRun(submitted.result.runId), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.phase).toBe("failed"));
    expect(result.current.error?.message).toBe(
      "run ended without a terminal event",
    );
    expect(
      result.current.events.filter(
        (event) => event.type === "run.verification",
      ),
    ).toHaveLength(1);
    await provider.whenBlocking();
    await client.cancelRun({ runId: submitted.result.runId });
  });

  test("reconnect after provider completion keeps one tool result and verification", async () => {
    const headers: (string | null)[] = [];
    let streams = 0;
    const client = connect(async (url, init) => {
      const stream = /\/runs\/([^/?]+)\/stream/.exec(url);
      if (stream !== null) {
        streams += 1;
        headers.push(new Headers(init?.headers).get("last-event-id"));
        const runId = stream[1]!;
        const events = [
          scriptedEvent(runId, 0, "run.started", { model: "m1", toolCount: 1 }),
          scriptedEvent(runId, 1, "run.tool.succeeded", {
            toolCallId: "tool-1",
            toolName: "echo",
            resultJson: "{}",
            sources: [],
            truncated: false,
            warnings: [],
          }),
          scriptedEvent(runId, 2, "run.completed", { iterations: 1 }),
          scriptedEvent(runId, 3, "run.verification", {
            pass: 1,
            status: "pass",
            deficiencies: [],
          }),
        ];
        const first = streams === 1;
        let sent = false;
        return new Response(
          new ReadableStream<Uint8Array>({
            pull(controller) {
              if (sent) {
                if (first)
                  controller.error(
                    new Error("pipe lost after provider completion"),
                  );
                else controller.close();
                return;
              }
              sent = true;
              const delivered = first ? events.slice(0, 3) : events.slice(1);
              controller.enqueue(
                new TextEncoder().encode(
                  delivered
                    .map(
                      (event) =>
                        `id: ${event.eventId}\ndata: ${JSON.stringify(event)}\n\n`,
                    )
                    .join(""),
                ),
              );
            },
          }),
        );
      }
      const response = await fetch(url, init);
      if (/\/runs\/[^/?]+$/.test(url) && (init?.method ?? "GET") === "GET") {
        return Response.json({
          ...((await response.json()) as Record<string, unknown>),
          status: "completed",
        });
      }
      return response;
    });
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "reconnect" },
    );
    const { result } = renderHook(() => useRun(submitted.result.runId), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(result.current.phase).toBe("completed"));
    expect(headers[1]).toBe("evt-2");
    expect(
      result.current.events.filter(
        (event) => event.type === "run.tool.succeeded",
      ),
    ).toHaveLength(1);
    expect(
      result.current.events.filter(
        (event) => event.type === "run.verification",
      ),
    ).toHaveLength(1);
    expect(result.current.events.map((event) => event.seq)).toEqual([
      0, 1, 2, 3,
    ]);
    await provider.whenBlocking();
    await client.cancelRun({ runId: submitted.result.runId });
  });

  test("StrictMode double click sends one POST and creates one run", async () => {
    let posts = 0;
    const client = connect(async (url, init) => {
      if (init?.method === "POST" && url.endsWith("/messages")) posts += 1;
      return fetch(url, init);
    });
    const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: strictWrapper(client),
    });
    await waitFor(() => expect(result.current.status).toBe("idle"));
    await act(async () => {
      await Promise.all([
        result.current.submit("once"),
        result.current.submit("once"),
      ]);
    });
    await provider.whenBlocking();
    expect(posts).toBe(1);
    expect(result.current.messages).toHaveLength(2);
    expect(await server.store.tasks.listByScope(TEST_CHAT_ID)).toHaveLength(1);
    const original = result.current.activeRunId;
    await act(async () => {
      await result.current.submit("different while busy");
      await result.current.regenerate(result.current.messages[1]!.id);
    });
    expect(posts).toBe(1);
    expect(result.current.activeRunId).toBe(original);
    expect(result.current.status).toBe("streaming");
    await act(async () => {
      await result.current.cancel();
    });
    await waitFor(() => expect(result.current.phase).toBe("cancelled"));
  });

  test("lost accepted response retries identical payload and key after remount", async () => {
    const keys: string[] = [];
    const bodies: string[] = [];
    let lose = true;
    const client = connect(async (url, init) => {
      if (init?.method === "POST" && url.endsWith("/messages")) {
        keys.push(new Headers(init.headers).get("idempotency-key") ?? "");
        bodies.push(String(init.body));
        const response = await fetch(url, init);
        if (lose) {
          lose = false;
          throw new TypeError("accepted response lost");
        }
        return response;
      }
      return fetch(url, init);
    });
    const first = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(first.result.current.status).toBe("idle"));
    const metadata = { settings: { temperature: 0.3 } };
    await act(async () => {
      await first.result.current.submit("once", { model: "m1", metadata });
    });
    expect(first.result.current.error?.message).toBe("accepted response lost");
    await provider.whenBlocking();
    const original = (await server.store.tasks.listByScope(TEST_CHAT_ID))[0]!
      .taskId;
    first.unmount();
    const second = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: strictWrapper(client),
    });
    await waitFor(() =>
      expect(second.result.current.activeRunId).toBe(original),
    );
    await act(async () => {
      await second.result.current.submit("once", { model: "m1", metadata });
    });
    expect(keys).toHaveLength(2);
    expect(keys[1]).toBe(keys[0]);
    expect(bodies[1]).toBe(bodies[0]);
    expect(await server.store.tasks.listByScope(TEST_CHAT_ID)).toHaveLength(1);
    expect(second.result.current.activeRunId).toBe(original);
    await act(async () => {
      await second.result.current.cancel();
    });
    await waitFor(() => expect(second.result.current.phase).toBe("cancelled"));
  });

  test("remount recovers original busy run and Stop cancels original", async () => {
    const client = connect();
    const first = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: wrapper(client),
    });
    await waitFor(() => expect(first.result.current.status).toBe("idle"));
    await act(async () => {
      await first.result.current.submit("run");
    });
    await provider.whenBlocking();
    const original = first.result.current.activeRunId!;
    first.unmount();
    const second = renderHook(() => useChat(TEST_CHAT_ID), {
      wrapper: strictWrapper(client),
    });
    await waitFor(() =>
      expect(second.result.current.activeRunId).toBe(original),
    );
    await waitFor(() =>
      expect(second.result.current.messages[1]?.content).toBe("Thinking"),
    );
    expect(second.result.current.status).toBe("streaming");
    await act(async () => {
      await second.result.current.cancel();
    });
    await waitFor(() => expect(second.result.current.phase).toBe("cancelled"));
    expect((await client.getRun({ runId: original })).status).toBe("cancelled");
    expect(await server.store.tasks.listByScope(TEST_CHAT_ID)).toHaveLength(1);
  });

  test.each(["model", "metadata"])(
    "changing %s after unknown outcome mints a new key",
    async (field) => {
      const keys: string[] = [];
      const client = connect(async (url, init) => {
        if (init?.method === "POST" && url.endsWith("/messages")) {
          keys.push(new Headers(init.headers).get("idempotency-key") ?? "");
          throw new TypeError("network unavailable");
        }
        return fetch(url, init);
      });
      const { result } = renderHook(() => useChat(TEST_CHAT_ID), {
        wrapper: wrapper(client),
      });
      await waitFor(() => expect(result.current.status).toBe("idle"));
      await act(async () => {
        await result.current.submit("same", {
          model: "m1",
          metadata: { setting: 1 },
        });
      });
      await act(async () => {
        await result.current.submit("same", {
          model: field === "model" ? "m2" : "m1",
          metadata: { setting: field === "metadata" ? 2 : 1 },
        });
      });
      expect(keys).toHaveLength(2);
      expect(keys[1]).not.toBe(keys[0]);
    },
  );
});
