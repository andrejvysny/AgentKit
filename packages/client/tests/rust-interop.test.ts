import { expect, test } from "bun:test";
import {
  nativeBackendAvailable,
  startCompatibilityServer,
  startNativeServer,
} from "../../../rust/tests/support/http-server.js";
import { runPhase } from "../src/index.js";

for (const backend of ["typescript", "rust"] as const) {
  const skip = backend === "rust" && !nativeBackendAvailable;
  (skip ? test.skip : test)(
    `same client full-turn assertions against ${backend}`,
    async () => {
      const server = await startCompatibilityServer(backend);
      try {
        const client = server.client;
        const chat = await client.createChat({ title: "Client flow" });
        expect(chat.id).toBeString();
        expect(chat.title).toBe("Client flow");
        expect(chat.archived).toBe(false);
        const submitted = await client.submitMessage(
          { chatId: chat.id },
          { content: "Say hello." },
        );
        expect(submitted.idempotencyKey.length).toBeGreaterThan(0);
        expect(submitted.result.chatId).toBe(chat.id);
        const events = [];
        for await (const event of client.streamRun(submitted.result.runId))
          events.push(event);
        expect(events.map((event) => event.seq)).toEqual(
          events.map((_event, index) => index),
        );
        expect(events.at(-1)?.type).toBe("run.completed");
        expect(runPhase({ events })).toBe("settling");
        const run = await client.getRun({ runId: submitted.result.runId });
        expect(run.status).toBe("completed");
        expect(runPhase({ status: run.status, events })).toBe("completed");
        const page = await client.listMessages({ chatId: chat.id });
        expect(page.items.map((message) => message.role)).toEqual([
          "user",
          "assistant",
        ]);
        expect(page.items[1]?.content).toBe("Hello, client.");
        expect(page.items[1]?.metadata["placeholder"]).toBe(false);
        const replay = await client.submitMessage(
          { chatId: chat.id },
          { content: "Say hello." },
          { idempotencyKey: submitted.idempotencyKey },
        );
        expect(replay.result).toEqual(submitted.result);
        const resumed = [];
        for await (const event of client.streamRun(run.runId, {
          lastEventId: events[1]!.eventId,
        }))
          resumed.push(event);
        expect(resumed.map((event) => event.eventId)).toEqual(
          events.slice(2).map((event) => event.eventId),
        );
      } finally {
        await server.stop();
      }
    },
    60_000,
  );
}

test.skipIf(!nativeBackendAvailable)(
  "native SQLite state survives process restart",
  async () => {
    let server = await startNativeServer();
    try {
      const chat = await server.client.createChat({ title: "Persisted" });
      const submitted = await server.client.submitMessage(
        { chatId: chat.id },
        { content: "persist" },
      );
      const ids = [];
      for await (const event of server.client.streamRun(submitted.result.runId))
        ids.push(event.eventId);
      await server.client.updateSettings({ maxToolIterations: 2 });
      server = await server.restart();
      expect((await server.client.getChat({ chatId: chat.id })).title).toBe(
        "Persisted",
      );
      expect(
        (await server.client.listMessages({ chatId: chat.id })).items[1]
          ?.content,
      ).toBe("Hello, client.");
      expect((await server.client.getSettings()).maxToolIterations).toBe(2);
      const replay = [];
      for await (const event of server.client.streamRun(submitted.result.runId))
        replay.push(event.eventId);
      expect(replay).toEqual(ids);
    } finally {
      await server.stop();
    }
  },
  60_000,
);

test.skipIf(!nativeBackendAvailable)(
  "unchanged TypeScript SSE client reconnects to native durable cursor after a cut",
  async () => {
    const { createAgentKitClient } = await import("../src/index.js");
    const server = await startNativeServer();
    const resumes: (string | null)[] = [];
    let cut = false;
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      headers: () => ({ authorization: `Bearer ${server.token}` }),
      fetch: async (url, input) => {
        const response = await fetch(url, input);
        if (!url.includes("/stream")) return response;
        resumes.push(new Headers(input?.headers).get("last-event-id"));
        if (cut || response.body === null) return response;
        cut = true;
        const upstream = response.body.getReader();
        let pending = "";
        let sent = false;
        const decoder = new TextDecoder();
        return new Response(
          new ReadableStream<Uint8Array>({
            async pull(controller) {
              if (sent) {
                await upstream.cancel();
                controller.error(new Error("test connection reset"));
                return;
              }
              for (;;) {
                const chunk = await upstream.read();
                if (chunk.done)
                  throw new Error(
                    "native stream closed before an event arrived",
                  );
                pending += decoder.decode(chunk.value, { stream: true });
                const match =
                  /id: [^\n]+\nevent: [^\n]+\ndata: [^\n]+\n\n/.exec(pending);
                if (match === null) continue;
                controller.enqueue(
                  new TextEncoder().encode(
                    pending.slice(0, match.index + match[0].length),
                  ),
                );
                sent = true;
                return;
              }
            },
            cancel: () => upstream.cancel(),
          }),
          { status: response.status, headers: response.headers },
        );
      },
    });
    try {
      const chat = await client.createChat({});
      const turn = await client.submitMessage(
        { chatId: chat.id },
        { content: "resume" },
      );
      const events = [];
      for await (const event of client.streamRun(turn.result.runId, {
        retryDelayMs: 1,
      }))
        events.push(event);
      expect(resumes.length).toBeGreaterThan(1);
      expect(resumes[1]).toBe(events[0]?.eventId);
      expect(events.map((event) => event.seq)).toEqual(
        events.map((_event, index) => index),
      );
      expect(events.at(-1)?.type).toBe("run.completed");
    } finally {
      await server.stop();
    }
  },
  60_000,
);

for (const backend of ["typescript", "rust"] as const) {
  const skip = backend === "rust" && !nativeBackendAvailable;
  (skip ? test.skip : test)(
    `raw JSON numbers follow JavaScript overflow semantics against ${backend}`,
    async () => {
      const { startTestServer } = await import("./support/server.js");
      const server = await startTestServer({ backend });
      try {
        const response = await fetch(`${server.baseUrl}/v1/chats`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: '{"metadata":{"positive":1e400,"negative":-1e400,"nested":[1e400],"rounded":9007199254740993}}',
        });
        expect(response.status).toBe(201);
        const chat = (await response.json()) as {
          id: string;
          metadata: Record<string, unknown>;
        };
        expect(chat.metadata).toEqual({
          positive: null,
          negative: null,
          nested: [null],
          rounded: 9007199254740992,
        });
        const client = (await import("../src/index.js")).createAgentKitClient({
          baseUrl: server.baseUrl,
        });
        expect((await client.getChat({ chatId: chat.id })).metadata).toEqual(
          chat.metadata,
        );
        const invalid = await fetch(`${server.baseUrl}/v1/settings`, {
          method: "PATCH",
          headers: { "content-type": "application/json" },
          body: '{"maxToolIterations":1e400}',
        });
        expect(invalid.status).toBe(400);
        expect(((await invalid.json()) as { code: string }).code).toBe(
          "invalid_request",
        );
      } finally {
        await server.stop();
      }
    },
    60_000,
  );
}

test.skipIf(!nativeBackendAvailable)(
  "native test control uses a separate listener and credential",
  async () => {
    const server = await startNativeServer();
    try {
      const provider = await server.client.createProvider({
        label: "Control boundary",
        kind: "openai-compatible",
        baseUrl: "https://example.test/v1",
        defaultModel: "m",
        apiKey: "test-only-private-secret",
      });
      const read = (token: string) =>
        fetch(`${server.controlUrl}/secret`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${token}`,
            "content-type": "application/json",
          },
          body: JSON.stringify({ ref: provider.apiKeySecretRef }),
        });
      expect((await read(server.token)).status).toBe(401);
      expect(await (await read(server.controlToken)).json()).toBe(
        "test-only-private-secret",
      );
      expect(
        (
          await fetch(`${server.baseUrl}/v1/providers`, {
            headers: { authorization: `Bearer ${server.controlToken}` },
          })
        ).status,
      ).toBe(401);
      expect(
        (
          await fetch(`${server.baseUrl}/secret`, {
            method: "POST",
            headers: {
              authorization: `Bearer ${server.controlToken}`,
              "content-type": "application/json",
            },
            body: "{}",
          })
        ).status,
      ).toBe(404);
    } finally {
      await server.stop();
    }
    await expect(fetch(server.baseUrl)).rejects.toThrow();
    await expect(fetch(server.controlUrl)).rejects.toThrow();
  },
  60_000,
);
