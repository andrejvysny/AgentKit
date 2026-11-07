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
    service.setProvider(() => ({
      content: "the answer",
      finishReason: "stop",
    }));
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
  "sessionProfileId resolves against the service's registry; an undeclared one 400s",
  async () => {
    // The positive half of this contract is `examples/desktop-host`, which
    // declares its own profile through `createService({ profiles })` and
    // creates chats with 201. This is the enforcement half the fix must not
    // have relaxed: a server pointed at a profile THAT service does not know
    // still refuses chat creation, with the profile refusal in the body, and
    // leaves no chat row behind.
    const { store, service } = await withService();
    try {
      const { baseUrl } = await service.startHttpServer({
        bearerToken: TOKEN,
        defaultModel: "test-model",
        sessionProfileId: "undeclared-profile",
      });
      const refused = await call(`${baseUrl}/v1/chats`, {
        method: "POST",
        headers: AUTH,
        body: JSON.stringify({ title: "ghost" }),
      });
      expect(refused.status).toBe(400);
      expect(refused.contentType).toBe("application/problem+json");
      expect(refused.body).toMatchObject({
        status: 400,
        code: "invalid_request",
      });
      expect(String(refused.body.detail)).toContain("unknown profile");
      // Refused before any effect: nothing was half-created.
      const listed = await call(`${baseUrl}/v1/chats`, { headers: AUTH });
      expect(listed.status).toBe(200);
      expect(listed.text).toBe("[]");
    } finally {
      await service.stopHttpServer();
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
      expect(items.map((m) => m.content)).toEqual(["question", "the answer"]);

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
      expect(stream.text).toEndWith(
        "event: agentkit.stream.settled\ndata: {}\n\n",
      );

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
      await expect(
        fetch(`http://127.0.0.1:${port}/v1/chats`),
      ).rejects.toThrow();
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

nativeTest(
  "startHttpServer binds the IPv6 loopback and answers over [::1]:port",
  async () => {
    // (External review, section 3.) `::1` used to die in DNS: the bracketed
    // literal was handed to the resolver as a HOSTNAME. Both spellings of
    // the literal — bare `::1` and URL-copied `[::1]` — must start the
    // server and answer real requests.
    const { store, service } = await withService(false);
    const start = async (host: string) => {
      try {
        return await service.startHttpServer({
          host,
          port: 0,
          bearerToken: TOKEN,
        });
      } catch (error) {
        const cause = JSON.stringify(
          (error as { details?: unknown })?.details ?? "",
        );
        // Skip ONLY for a sandbox with no IPv6 loopback (bind of the literal
        // dies with EADDRNOTAVAIL / unsupported family). A DNS-lookup cause
        // means the bracketed host reached the resolver — the exact bug this
        // test pins — so it must FAIL, never skip.
        if (/address family|cannot assign|eaddrnotavail|ipv6/i.test(cause)) {
          // Skip only for a sandbox without an IPv6 loopback, documenting
          // the OS's own error rather than silently passing.
          console.log(
            `SKIPPING IPv6 listen on ${host}: this sandbox cannot bind the IPv6 loopback (${cause})`,
          );
          return null;
        }
        throw error;
      }
    };
    try {
      for (const host of ["::1", "[::1]"]) {
        const info = await start(host);
        if (info === null) return;
        expect(info.baseUrl).toBe(`http://[::1]:${info.port}`);
        const anonymous = await call(`${info.baseUrl}/v1/chats`);
        expect(anonymous.status).toBe(401);
        expect(anonymous.contentType).toBe("application/problem+json");
        const listed = await call(`${info.baseUrl}/v1/chats`, {
          headers: AUTH,
        });
        expect(listed.status).toBe(200);
        expect(Array.isArray(listed.body)).toBe(true);
        await service.stopHttpServer();
      }
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);

nativeTest(
  "cors option answers the reference transport's exact CORS matrix",
  async () => {
    // (External review, section 5.) The retired TypeScript transport
    // decorated cross-origin responses; the native server must answer a
    // browser identically — or, with `cors` omitted, byte-for-byte as if
    // no browser was ever expected.
    const { store, service } = await withService(false);
    const ALLOWED = "http://localhost:5173";
    const FOREIGN = "http://evil.example";
    try {
      const { baseUrl } = await service.startHttpServer({
        bearerToken: TOKEN,
        cors: {
          origins: [ALLOWED],
          exposeHeaders: ["X-Total-Count"],
          maxAgeSeconds: 600,
        },
      });
      const pre = await fetch(`${baseUrl}/v1/chats`, {
        method: "OPTIONS",
        headers: {
          origin: ALLOWED,
          "access-control-request-method": "POST",
        },
      });
      expect(pre.status).toBe(204);
      expect(pre.headers.get("access-control-allow-origin")).toBe(ALLOWED);
      expect(pre.headers.get("vary")).toBe("Origin");
      const methods = (pre.headers.get("access-control-allow-methods") ?? "")
        .split(", ")
        .filter(Boolean);
      // Truth about THIS path: /v1/chats serves GET+POST — OPTIONS itself
      // must not leak into the list.
      expect(methods).toContain("GET");
      expect(methods).toContain("POST");
      expect(methods).not.toContain("OPTIONS");
      expect(pre.headers.get("access-control-allow-headers")).toBe(
        "Content-Type, Idempotency-Key, Last-Event-ID, Authorization",
      );
      expect(pre.headers.get("access-control-max-age")).toBe("600");

      // Rejected origin: not 403 — the ordinary 405 for the OPTIONS verb,
      // no ACAO, but the shared-cache `Vary: Origin` still rides.
      const rejected = await fetch(`${baseUrl}/v1/chats`, {
        method: "OPTIONS",
        headers: { origin: FOREIGN },
      });
      expect(rejected.status).toBe(405);
      expect(rejected.headers.get("access-control-allow-origin")).toBeNull();
      expect(rejected.headers.get("vary")).toBe("Origin");

      // Preflight to an unrouted path: 404, still CORS-decorated.
      const ghost = await fetch(`${baseUrl}/v1/nope`, {
        method: "OPTIONS",
        headers: { origin: ALLOWED },
      });
      expect(ghost.status).toBe(404);
      expect(ghost.headers.get("access-control-allow-origin")).toBe(ALLOWED);
      expect(ghost.headers.get("access-control-allow-methods")).toBeNull();

      // Matched ordinary response: allow-origin + expose + vary.
      const ok = await fetch(`${baseUrl}/v1/chats`, {
        headers: { ...AUTH, origin: ALLOWED },
      });
      expect(ok.status).toBe(200);
      expect(ok.headers.get("access-control-allow-origin")).toBe(ALLOWED);
      expect(ok.headers.get("access-control-expose-headers")).toBe(
        "X-Total-Count",
      );
      expect(ok.headers.get("vary")).toBe("Origin");

      // A refusal is decorated too — a policy that skips the 401 is a
      // CORS error on the day something actually goes wrong.
      const anon = await fetch(`${baseUrl}/v1/chats`, {
        headers: { origin: ALLOWED },
      });
      expect(anon.status).toBe(401);
      expect(anon.headers.get("access-control-allow-origin")).toBe(ALLOWED);

      // Foreign origin: the SAME 200 body; only the browser blocks.
      const foreign = await fetch(`${baseUrl}/v1/chats`, {
        headers: { ...AUTH, origin: FOREIGN },
      });
      expect(foreign.status).toBe(200);
      expect(foreign.headers.get("access-control-allow-origin")).toBeNull();
      expect(foreign.headers.get("vary")).toBe("Origin");

      await service.stopHttpServer();

      // Unconfigured server: no response is touched, byte for byte, and
      // OPTIONS falls through to the plain 405 like any other verb.
      const plain = await service.startHttpServer({ bearerToken: TOKEN });
      const untouched = await fetch(`${plain.baseUrl}/v1/chats`, {
        headers: { ...AUTH, origin: ALLOWED },
      });
      expect(untouched.status).toBe(200);
      expect(untouched.headers.get("vary")).toBeNull();
      expect(untouched.headers.get("access-control-allow-origin")).toBeNull();
      const plainPre = await fetch(`${plain.baseUrl}/v1/chats`, {
        method: "OPTIONS",
        headers: { origin: ALLOWED },
      });
      expect(plainPre.status).toBe(405);
      expect(plainPre.headers.get("access-control-allow-origin")).toBeNull();
      await service.stopHttpServer();
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);

nativeTest(
  "cors/providerSecrets options are strictly parsed, never ignored",
  async () => {
    const { store, service } = await withService(false);
    try {
      for (const options of [
        { bearerToken: TOKEN, cors: {} },
        { bearerToken: TOKEN, cors: { origins: [] } },
        { bearerToken: TOKEN, cors: { origins: 5 } },
        { bearerToken: TOKEN, cors: { origins: ["ok"], bogus: 1 } },
        { bearerToken: TOKEN, cors: { origins: [" "] } },
        { bearerToken: TOKEN, cors: { origins: ["ok", 7] } },
        { bearerToken: TOKEN, cors: { origins: "*", allowHeaders: [""] } },
        { bearerToken: TOKEN, cors: { origins: "*", exposeHeaders: "x" } },
        { bearerToken: TOKEN, cors: { origins: "*", maxAgeSeconds: -1 } },
        { bearerToken: TOKEN, cors: "on" },
        { bearerToken: TOKEN, providerSecrets: "disk" },
        { bearerToken: TOKEN, providerSecrets: true },
      ] as const) {
        await expect(
          service.startHttpServer(options as never),
        ).rejects.toMatchObject({ code: "invalid_input" });
      }
      // The wildcard spelling and a zero max-age ARE valid shapes.
      const { baseUrl } = await service.startHttpServer({
        bearerToken: TOKEN,
        cors: { origins: "*", maxAgeSeconds: 0 },
      });
      const pre = await fetch(`${baseUrl}/v1/chats`, {
        method: "OPTIONS",
        headers: { origin: "http://anywhere.test" },
      });
      expect(pre.status).toBe(204);
      expect(pre.headers.get("access-control-allow-origin")).toBe("*");
      expect(pre.headers.get("access-control-max-age")).toBe("0");
      await service.stopHttpServer();
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);

nativeTest(
  "provider management routes are real when providerSecrets wires a vault",
  async () => {
    // (External review, section 5.) Without a vault the credential writes
    // answer 501 exactly like the TypeScript transport running with no
    // `SecretStore`; with `"memory"` they really store, list, rename, and
    // retire providers — and the plaintext key never comes back.
    const { store, service } = await withService(false);
    const provider = {
      id: "review-fake",
      label: "Review",
      kind: "openai-compatible",
      baseUrl: "https://api.review.test/v1",
      defaultModel: "rev-1",
    };
    try {
      const bare = await service.startHttpServer({ bearerToken: TOKEN });
      const refused = await call(`${bare.baseUrl}/v1/providers`, {
        method: "POST",
        headers: { ...AUTH, "content-type": "application/json" },
        body: JSON.stringify({ ...provider, apiKey: "sk-bare" }),
      });
      expect(refused.status).toBe(501);
      expect(refused.body.code).toBe("not_implemented");
      await service.stopHttpServer();

      const wired = await service.startHttpServer({
        bearerToken: TOKEN,
        providerSecrets: "memory",
      });
      const created = await call(`${wired.baseUrl}/v1/providers`, {
        method: "POST",
        headers: { ...AUTH, "content-type": "application/json" },
        body: JSON.stringify({ ...provider, apiKey: "sk-do-not-leak" }),
      });
      expect(created.status).toBe(201);
      expect(created.body).toMatchObject({
        id: "review-fake",
        label: "Review",
        kind: "openai-compatible",
        baseUrl: "https://api.review.test/v1",
        defaultModel: "rev-1",
        enabled: true,
        apiKeySecretRef: "provider/review-fake/api-key",
      });
      expect(JSON.stringify(created.body)).not.toContain("sk-do-not-leak");

      const listed = await call(`${wired.baseUrl}/v1/providers`, {
        headers: AUTH,
      });
      expect(listed.status).toBe(200);
      const values = JSON.parse(listed.text) as Array<Record<string, unknown>>;
      expect(values.map((v) => v.id)).toContain("review-fake");

      const renamed = await call(`${wired.baseUrl}/v1/providers/review-fake`, {
        method: "PATCH",
        headers: { ...AUTH, "content-type": "application/json" },
        body: JSON.stringify({ label: "Renamed" }),
      });
      expect(renamed.status).toBe(200);
      expect(renamed.body.label).toBe("Renamed");

      const duplicate = await call(`${wired.baseUrl}/v1/providers`, {
        method: "POST",
        headers: { ...AUTH, "content-type": "application/json" },
        body: JSON.stringify(provider),
      });
      expect(duplicate.status).toBe(409);
      expect(duplicate.body.code).toBe("duplicate_provider");

      // Missing vault for a key update still needs the vault present —
      // it is, so a rotate works and keeps the ref stable.
      const rotated = await call(`${wired.baseUrl}/v1/providers/review-fake`, {
        method: "PATCH",
        headers: { ...AUTH, "content-type": "application/json" },
        body: JSON.stringify({ apiKey: "sk-rotated" }),
      });
      expect(rotated.status).toBe(200);
      expect(rotated.body.apiKeySecretRef).toBe("provider/review-fake/api-key");
      expect(JSON.stringify(rotated.body)).not.toContain("sk-rotated");

      const removed = await call(`${wired.baseUrl}/v1/providers/review-fake`, {
        method: "DELETE",
        headers: AUTH,
      });
      expect(removed.status).toBe(204);
      const after = await call(`${wired.baseUrl}/v1/providers`, {
        headers: AUTH,
      });
      expect((JSON.parse(after.text) as unknown[]).length).toBe(0);
      await service.stopHttpServer();
    } finally {
      await service.close();
      await store.close();
    }
  },
  20_000,
);
