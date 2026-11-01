/**
 * The Rust REST v1 + SSE engine, served from the NAPI binding over the
 * service's own host and store. This package contains NO TypeScript HTTP
 * server, so every response here — including the RFC-level `problem+json`
 * error envelopes with `https://agentkit.dev/problems/...` types and the
 * exact `agentkit.stream.settled` SSE terminator — can only be produced by
 * the Rust `agentkit-transport-http` engine.
 */
import { expect, test } from "bun:test";
import { NativeStore } from "../src/index.js";
import type { NativeSessionService } from "../src/index.js";
import { available, missing, required } from "./helpers/binding.js";

if (required && !available)
  throw new Error(`native binding missing: ${missing.join(", ")}`);
const nativeTest = available ? test : test.skip;

/** 32 bytes: the exact floor `DesktopBackend::new` enforces. */
const TOKEN = "12345678901234567890123456789012";
const AUTH = { authorization: `Bearer ${TOKEN}` };

interface Call {
  status: number;
  contentType: string;
  text: string;
  body: Record<string, unknown>;
}

async function call(url: string, init?: RequestInit): Promise<Call> {
  const response = await fetch(url, init);
  const text = await response.text();
  let body: Record<string, unknown> = {};
  try {
    body = JSON.parse(text) as Record<string, unknown>;
  } catch {
    // Non-JSON bodies (SSE text) stay observable via `text`.
  }
  return {
    status: response.status,
    contentType: response.headers.get("content-type") ?? "",
    text,
    body,
  };
}

async function withService(
  provider = true,
): Promise<{ store: NativeStore; service: NativeSessionService }> {
  const store = await NativeStore.inMemory();
  const service = await store.createService();
  if (provider)
    service.setProvider(() => ({ content: "the answer", finishReason: "stop" }));
  return { store, service };
}

/**
 * Wait on the durable event pump: `poll` blocks on committed events, so
 * the awaited signal is the worker's own publication — the deadline is a
 * failure safety net, never the wait mechanism.
 */
async function awaitRunSettled(
  service: NativeSessionService,
  runId: string,
): Promise<void> {
  const subscription = await service.subscribeTurn(runId, 0);
  const started = Date.now();
  let done = false;
  while (!done && Date.now() - started < 15_000)
    done = (await subscription.poll(64, 250)).done;
  subscription.cancel();
  if (!done) throw new Error(`run ${runId} never settled`);
}

nativeTest(
  "bearer refusal and routing errors are Rust problem+json envelopes",
  async () => {
    const { store, service } = await withService(false);
    try {
      const { port, baseUrl } = await service.startHttpServer({
        bearerToken: TOKEN,
      });
      expect(port).toBeGreaterThan(0);
      expect(baseUrl).toBe(`http://127.0.0.1:${port}`);

      const anonymous = await call(`${baseUrl}/v1/chats`);
      expect(anonymous.status).toBe(401);
      expect(anonymous.contentType).toBe("application/problem+json");
      expect(anonymous.body).toMatchObject({
        type: "https://agentkit.dev/problems/unauthorized",
        title: "Unauthorized",
        status: 401,
        code: "unauthorized",
        detail: "A valid desktop bearer token is required.",
        instance: "/v1/chats",
      });

      const wrong = await call(`${baseUrl}/v1/chats`, {
        headers: { authorization: `Bearer ${"x".repeat(32)}` },
      });
      expect(wrong.status).toBe(401);
      expect(wrong.body.code).toBe("unauthorized");

      // Unknown path: routed by the exact frozen route table, answered by
      // the transport's fallback with a method+path-aware detail.
      const unknown = await call(`${baseUrl}/v1/nope`, { headers: AUTH });
      expect(unknown.status).toBe(404);
      expect(unknown.contentType).toBe("application/problem+json");
      expect(unknown.body).toMatchObject({
        type: "https://agentkit.dev/problems/not_found",
        status: 404,
        code: "not_found",
        detail: "No route for GET /v1/nope.",
        instance: "/v1/nope",
      });

      // Wrong method on a routed path: 405 with the transport's allow list.
      const wrongMethod = await call(`${baseUrl}/v1/chats`, {
        method: "PUT",
        headers: AUTH,
      });
      expect(wrongMethod.status).toBe(405);
      expect(wrongMethod.body.code).toBe("method_not_allowed");

      await service.stopHttpServer();
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);

nativeTest(
  "chat round-trip, durable submit + worker execution, task poll, and SSE replay over REST",
  async () => {
    const { store, service } = await withService();
    try {
      const { baseUrl } = await service.startHttpServer({
        bearerToken: TOKEN,
        defaultModel: "test-model",
      });

      const created = await call(`${baseUrl}/v1/chats`, {
        method: "POST",
        headers: AUTH,
        body: JSON.stringify({ title: "Native" }),
      });
      expect(created.status).toBe(201);
      expect(created.contentType).toContain("application/json");
      expect(created.body).toMatchObject({ title: "Native", archived: false });
      const chatId = created.body.id as string;
      expect(chatId.length).toBeGreaterThan(0);

      const listed = await call(`${baseUrl}/v1/chats`, { headers: AUTH });
      expect(listed.status).toBe(200);
      expect((listed.body as unknown as unknown[]).length).toBe(1);
      expect(JSON.stringify(listed.body)).toContain(chatId);

      const fetched = await call(`${baseUrl}/v1/chats/${chatId}`, {
        headers: AUTH,
      });
      expect(fetched.status).toBe(200);
      expect(fetched.body.id).toBe(chatId);
      expect(fetched.body.activeRunId).toBeNull();

      const missingChat = await call(`${baseUrl}/v1/chats/chat_absent`, {
        headers: AUTH,
      });
      expect(missingChat.status).toBe(404);
      expect(missingChat.body.code).toBe("not_found");

      // Submit without the idempotency key is refused by the transport.
      const noKey = await call(`${baseUrl}/v1/chats/${chatId}/messages`, {
        method: "POST",
        headers: AUTH,
        body: JSON.stringify({ content: "question" }),
      });
      expect(noKey.status).toBe(400);
      expect(noKey.body.code).toBe("idempotency_key_required");

      const submitted = await call(`${baseUrl}/v1/chats/${chatId}/messages`, {
        method: "POST",
        headers: { ...AUTH, "idempotency-key": "turn-1" },
        body: JSON.stringify({ content: "question" }),
      });
      expect(submitted.status).toBe(201);
      expect(submitted.body).toMatchObject({ chatId });
      const runId = submitted.body.runId as string;
      expect(runId).toStartWith("task_");

      // The service's own worker (no extra host, no manual run_once) must
      // settle the REST-submitted turn through the JS provider lane.
      await awaitRunSettled(service, runId);
      const run = await call(`${baseUrl}/v1/runs/${runId}`, { headers: AUTH });
      expect(run.status).toBe(200);
      expect(run.body).toMatchObject({ runId, chatId, status: "completed" });

      const messages = await call(`${baseUrl}/v1/chats/${chatId}/messages`, {
        headers: AUTH,
      });
      expect(messages.status).toBe(200);
      const items = messages.body.items as Record<string, unknown>[];
      expect(items.map((m) => m.role)).toEqual(["user", "assistant"]);
      expect(items[0].content).toBe("question");
      expect(items[1].content).toBe("the answer");

      // Durable replay: same key + body replays the original 201 result.
      const replay = await call(`${baseUrl}/v1/chats/${chatId}/messages`, {
        method: "POST",
        headers: { ...AUTH, "idempotency-key": "turn-1" },
        body: JSON.stringify({ content: "question" }),
      });
      expect(replay.status).toBe(200);
      expect(replay.body).toEqual(submitted.body);
      const mismatch = await call(`${baseUrl}/v1/chats/${chatId}/messages`, {
        method: "POST",
        headers: { ...AUTH, "idempotency-key": "turn-1" },
        body: JSON.stringify({ content: "changed" }),
      });
      expect(mismatch.status).toBe(422);
      expect(mismatch.body.code).toBe("idempotency_key_mismatch");

      // SSE event replay for the completed turn, ended by the engine's
      // own settled terminator frame.
      const stream = await call(`${baseUrl}/v1/runs/${runId}/stream`, {
        headers: AUTH,
      });
      expect(stream.status).toBe(200);
      expect(stream.contentType).toStartWith("text/event-stream");
      expect(stream.text).toContain("run.message.delta");
      expect(stream.text).toContain("the answer");
      expect(stream.text).toEndWith("event: agentkit.stream.settled\ndata: {}\n\n");

      // Second start while running is refused; stop then frees the port.
      await expect(
        service.startHttpServer({ bearerToken: TOKEN }),
      ).rejects.toMatchObject({ code: "invalid_input" });
      await service.stopHttpServer();
      await expect(fetch(`${baseUrl}/v1/chats`)).rejects.toThrow();
      // Restart works and re-binds the same (idempotent) desktop actor.
      const again = await service.startHttpServer({ bearerToken: TOKEN });
      expect(again.port).toBeGreaterThan(0);
      await service.stopHttpServer();
      await service.stopHttpServer(); // idempotent
    } finally {
      await service.close();
      await store.close();
    }
  },
  30_000,
);

nativeTest(
  "base path, body-size cap, and stopped-server refusal",
  async () => {
    const { store, service } = await withService(false);
    try {
      const prefixed = await service.startHttpServer({
        bearerToken: TOKEN,
        basePath: "/desktop/",
      });
      expect(prefixed.baseUrl).toBe(
        `http://127.0.0.1:${prefixed.port}/desktop`,
      );
      expect(
        (await call(`${prefixed.baseUrl}/v1/chats`, { headers: AUTH })).status,
      ).toBe(200);
      expect(
        (
          await call(`http://127.0.0.1:${prefixed.port}/v1/chats`, {
            headers: AUTH,
          })
        ).status,
      ).toBe(404);
      await service.stopHttpServer();

      const capped = await service.startHttpServer({
        bearerToken: TOKEN,
        maxBodyBytes: 256,
      });
      const base = `http://127.0.0.1:${capped.port}`;
      const created = await call(`${base}/v1/chats`, {
        method: "POST",
        headers: AUTH,
        body: JSON.stringify({ title: "caps" }),
      });
      expect(created.status).toBe(201);
      const chatId = created.body.id as string;
      const tooLarge = await call(`${base}/v1/chats/${chatId}/messages`, {
        method: "POST",
        headers: { ...AUTH, "idempotency-key": "big" },
        body: JSON.stringify({ content: "z".repeat(4096) }),
      });
      expect(tooLarge.status).toBe(413);
      expect(tooLarge.contentType).toBe("application/problem+json");
      expect(tooLarge.body).toMatchObject({
        type: "https://agentkit.dev/problems/body_too_large",
        title: "Content Too Large",
        status: 413,
        code: "body_too_large",
      });
      await service.stopHttpServer();
      await expect(fetch(`${base}/v1/chats`)).rejects.toThrow();
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);

nativeTest(
  "option validation, closed-service refusal, and close with a live server",
  async () => {
    const { store, service } = await withService(false);
    try {
      await expect(service.stopHttpServer()).resolves.toBeUndefined();
      for (const options of [
        {},
        { bearerToken: null },
        { bearerToken: "short" },
        { bearerToken: 7 },
        { bearerToken: TOKEN, port: 70_000 },
        { bearerToken: TOKEN, port: -1 },
        { bearerToken: TOKEN, host: "" },
        { bearerToken: TOKEN, host: "http://x" },
        { bearerToken: TOKEN, maxBodyBytes: 0 },
        { bearerToken: TOKEN, maxQueryBytes: -8 },
        { bearerToken: TOKEN, actorId: " " },
        { bearerToken: TOKEN, defaultModel: "" },
      ] as const) {
        await expect(
          service.startHttpServer(options as never),
        ).rejects.toMatchObject({ code: "invalid_input" });
      }
      await expect(
        service.startHttpServer("nope" as never),
      ).rejects.toMatchObject({ code: "invalid_input" });

      // close() with a live server must settle (no hang) and refuse later
      // connections: bun's per-test timeout is the hang detector.
      const { port } = await service.startHttpServer({ bearerToken: TOKEN });
      await expect(
        call(`http://127.0.0.1:${port}/v1/chats`, { headers: AUTH }),
      ).resolves.toMatchObject({ status: 200 });
      await service.close();
      await expect(fetch(`http://127.0.0.1:${port}/v1/chats`)).rejects.toThrow();
      await expect(service.stopHttpServer()).resolves.toBeUndefined();
      await expect(
        service.startHttpServer({ bearerToken: TOKEN }),
      ).rejects.toMatchObject({ code: "closed" });
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);
