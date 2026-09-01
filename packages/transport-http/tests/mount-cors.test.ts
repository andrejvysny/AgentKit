/**
 * `deps.basePath` and `deps.cors` — where the API is mounted, and who may read
 * it from a browser.
 *
 * Both are opt-in, and both tests assert the "not configured" shape as well as
 * the configured one: a transport whose defaults quietly changed would be a
 * breaking change to every host that never asked for either feature.
 */
import { describe, expect, it } from "bun:test";
import {
  CONTRACT_VERSION,
  type AiRunEvent,
  type TaskEventEnvelope,
} from "@agentkit/contracts";
import type { MemoryAssistantStore } from "@agentkit/reference-adapters";
import { normalizeBasePath, stripBasePath } from "../src/router.js";
import {
  createHandlerFixture,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

const ORIGIN = "https://app.example.com";

function withOrigin(
  method: string,
  path: string,
  origin: string,
  extra: Record<string, string> = {},
): Request {
  return request(method, path, { headers: { origin, ...extra } });
}

describe("basePath — normalization", () => {
  it("accepts the three ways a config file spells `unset`", () => {
    expect(normalizeBasePath(undefined)).toBe("");
    expect(normalizeBasePath("")).toBe("");
    expect(normalizeBasePath("/")).toBe("");
    expect(normalizeBasePath("  ")).toBe("");
  });

  it("adds the leading slash and drops trailing ones", () => {
    expect(normalizeBasePath("api/agentkit")).toBe("/api/agentkit");
    expect(normalizeBasePath("/api/agentkit/")).toBe("/api/agentkit");
    expect(normalizeBasePath("/api/agentkit///")).toBe("/api/agentkit");
  });

  it("strips only inside the mount", () => {
    expect(stripBasePath("/api/v1/version", "/api")).toBe("/v1/version");
    expect(stripBasePath("/api", "/api")).toBe("/");
    expect(stripBasePath("/v1/version", "/api")).toBeNull();
    // A prefix match that is not a segment boundary is not inside the mount.
    expect(stripBasePath("/apiary/v1/version", "/api")).toBeNull();
    expect(stripBasePath("/v1/version", "")).toBe("/v1/version");
  });
});

describe("basePath — routing", () => {
  it("serves the contract under the mount and 404s outside it", async () => {
    const f = await createHandlerFixture({ basePath: "/api/agentkit" });

    const mounted = await f.handler(request("GET", "/api/agentkit/v1/version"));
    expect(mounted.status).toBe(200);
    const version = (await mounted.json()) as Record<string, unknown>;
    expect(version["contractVersion"]).toBe(CONTRACT_VERSION);

    const unmounted = await f.handler(request("GET", "/v1/version"));
    expect(unmounted.status).toBe(404);
    expect(unmounted.headers.get("content-type")).toBe(
      "application/problem+json",
    );
    const body = (await unmounted.json()) as Record<string, unknown>;
    expect(body["code"]).toBe("not_found");
    // The problem reports the path the client actually asked for.
    expect(body["instance"]).toBe("/v1/version");
  });

  it("reports the full path, prefix included, on a problem inside the mount", async () => {
    const f = await createHandlerFixture({ basePath: "/api/agentkit" });
    const res = await f.handler(
      request("GET", "/api/agentkit/v1/runs/missing"),
    );
    expect(res.status).toBe(404);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body["instance"]).toBe("/api/agentkit/v1/runs/missing");
  });

  it("404s the mount root itself", async () => {
    const f = await createHandlerFixture({ basePath: "/api/agentkit" });
    expect((await f.handler(request("GET", "/api/agentkit"))).status).toBe(404);
  });

  it("serves parameterized routes and a 405 under the mount", async () => {
    const f = await createHandlerFixture({ basePath: "api/agentkit/" });

    const chat = await f.handler(
      request("GET", `/api/agentkit/v1/chats/${TEST_CHAT_ID}`),
    );
    expect(chat.status).toBe(200);
    const chatBody = (await chat.json()) as Record<string, unknown>;
    expect(chatBody["id"]).toBe(TEST_CHAT_ID);

    const wrongVerb = await f.handler(
      request("DELETE", "/api/agentkit/v1/chats"),
    );
    expect(wrongVerb.status).toBe(405);
    expect(wrongVerb.headers.get("allow")).toContain("GET");
  });

  it("routes from the root when no basePath is set", async () => {
    const f = await createHandlerFixture();
    expect((await f.handler(request("GET", "/v1/version"))).status).toBe(200);
    expect(
      (await f.handler(request("GET", "/api/agentkit/v1/version"))).status,
    ).toBe(404);
  });
});

describe("CORS — preflight", () => {
  it("answers 204 with the methods the route table declares for that path", async () => {
    const f = await createHandlerFixture({
      cors: { origins: [ORIGIN], maxAgeSeconds: 600 },
    });

    const res = await f.handler(
      withOrigin("OPTIONS", "/v1/chats", ORIGIN, {
        "access-control-request-method": "POST",
      }),
    );
    expect(res.status).toBe(204);
    expect(res.headers.get("access-control-allow-origin")).toBe(ORIGIN);
    expect(res.headers.get("vary")).toBe("Origin");
    const methods = (res.headers.get("access-control-allow-methods") ?? "")
      .split(", ")
      .sort();
    expect(methods).toEqual(["GET", "POST"]);
    expect(res.headers.get("access-control-allow-headers")).toBe(
      "Content-Type, Idempotency-Key, Last-Event-ID, Authorization",
    );
    expect(res.headers.get("access-control-max-age")).toBe("600");
    expect(await res.text()).toBe("");
  });

  it("honours a custom allowHeaders list and omits max-age when unset", async () => {
    const f = await createHandlerFixture({
      cors: { origins: "*", allowHeaders: ["Content-Type", "X-Tenant"] },
    });
    const res = await f.handler(withOrigin("OPTIONS", "/v1/version", ORIGIN));
    expect(res.status).toBe(204);
    expect(res.headers.get("access-control-allow-origin")).toBe("*");
    expect(res.headers.get("access-control-allow-headers")).toBe(
      "Content-Type, X-Tenant",
    );
    expect(res.headers.get("access-control-max-age")).toBeNull();
    expect(res.headers.get("access-control-allow-methods")).toBe("GET");
  });

  it("404s a preflight for a path that does not exist", async () => {
    const f = await createHandlerFixture({ cors: { origins: [ORIGIN] } });
    const res = await f.handler(withOrigin("OPTIONS", "/v1/nope", ORIGIN));
    expect(res.status).toBe(404);
    expect(res.headers.get("access-control-allow-origin")).toBe(ORIGIN);
  });

  it("falls through to the ordinary 405 for an origin that is not allowed", async () => {
    const f = await createHandlerFixture({ cors: { origins: [ORIGIN] } });
    const res = await f.handler(
      withOrigin("OPTIONS", "/v1/chats", "https://evil.example"),
    );
    expect(res.status).toBe(405);
    expect(res.headers.get("access-control-allow-origin")).toBeNull();
  });

  it("405s OPTIONS when CORS is not configured at all", async () => {
    const f = await createHandlerFixture();
    const res = await f.handler(withOrigin("OPTIONS", "/v1/chats", ORIGIN));
    expect(res.status).toBe(405);
    expect(res.headers.get("access-control-allow-origin")).toBeNull();
  });
});

describe("CORS — actual responses", () => {
  it("decorates a matched origin and leaves an unmatched one untouched", async () => {
    const f = await createHandlerFixture({ cors: { origins: [ORIGIN] } });

    const allowed = await f.handler(withOrigin("GET", "/v1/version", ORIGIN));
    expect(allowed.status).toBe(200);
    expect(allowed.headers.get("access-control-allow-origin")).toBe(ORIGIN);
    expect(allowed.headers.get("vary")).toBe("Origin");
    expect(allowed.headers.get("content-type")).toBe("application/json");

    const other = await f.handler(
      withOrigin("GET", "/v1/version", "https://evil.example"),
    );
    // Not an error — just no allow-origin header, which is what stops the
    // browser. `Vary: Origin` is still present: the response differs by
    // origin, and a cache must not replay this headerless answer to an
    // allowed one.
    expect(other.status).toBe(200);
    expect(other.headers.get("access-control-allow-origin")).toBeNull();
    expect(other.headers.get("vary")).toBe("Origin");

    const noOrigin = await f.handler(request("GET", "/v1/version"));
    expect(noOrigin.headers.get("access-control-allow-origin")).toBeNull();
  });

  it("echoes `*` only when `*` is configured", async () => {
    const wildcard = await createHandlerFixture({ cors: { origins: "*" } });
    const res = await wildcard.handler(
      withOrigin("GET", "/v1/version", "https://anything.example"),
    );
    expect(res.headers.get("access-control-allow-origin")).toBe("*");

    const listed = await createHandlerFixture({ cors: { origins: [ORIGIN] } });
    const echoed = await listed.handler(
      withOrigin("GET", "/v1/version", ORIGIN),
    );
    expect(echoed.headers.get("access-control-allow-origin")).toBe(ORIGIN);
  });

  it("exposes the headers a host asks for", async () => {
    const f = await createHandlerFixture({
      cors: { origins: [ORIGIN], exposeHeaders: ["X-Request-Id"] },
    });
    const res = await f.handler(withOrigin("GET", "/v1/version", ORIGIN));
    expect(res.headers.get("access-control-expose-headers")).toBe(
      "X-Request-Id",
    );
  });

  it("decorates a 404 problem response", async () => {
    const f = await createHandlerFixture({ cors: { origins: [ORIGIN] } });
    const res = await f.handler(withOrigin("GET", "/v1/nope", ORIGIN));
    expect(res.status).toBe(404);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
    expect(res.headers.get("access-control-allow-origin")).toBe(ORIGIN);
    expect(res.headers.get("vary")).toBe("Origin");
    expect(((await res.json()) as Record<string, unknown>)["code"]).toBe(
      "not_found",
    );
  });

  it("decorates the host's own `authenticate` response", async () => {
    const f = await createHandlerFixture({
      cors: { origins: [ORIGIN] },
      authenticate: async () => new Response("no", { status: 401 }),
    });
    const res = await f.handler(withOrigin("GET", "/v1/chats", ORIGIN));
    expect(res.status).toBe(401);
    expect(await res.text()).toBe("no");
    expect(res.headers.get("access-control-allow-origin")).toBe(ORIGIN);
  });

  it("decorates a 403 from the authorization port", async () => {
    const f = await createHandlerFixture({
      cors: { origins: [ORIGIN] },
      authorize: { authorize: async () => ({ allowed: false }) },
    });
    const res = await f.handler(withOrigin("GET", "/v1/chats", ORIGIN));
    expect(res.status).toBe(403);
    expect(res.headers.get("access-control-allow-origin")).toBe(ORIGIN);
    await res.text();
  });

  it("decorates the SSE stream without breaking it", async () => {
    const f = await createHandlerFixture({
      cors: { origins: [ORIGIN] },
      basePath: "/api",
      streaming: { pollIntervalMs: 2 },
    });
    const runId = await seedCompletedRun(f.store);

    const res = await f.handler(
      withOrigin("GET", `/api/v1/runs/${runId}/stream`, ORIGIN),
    );
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("text/event-stream");
    expect(res.headers.get("cache-control")).toBe("no-cache, no-transform");
    expect(res.headers.get("access-control-allow-origin")).toBe(ORIGIN);
    expect(res.headers.get("vary")).toBe("Origin");

    // The body is still a live stream, not a buffered copy: it carries the
    // frames and closes on the terminal event.
    const text = await res.text();
    expect(text).toContain("event: run.started");
    expect(text).toContain("event: run.completed");
  });
});

/** A finished run whose log holds a terminal event, so its stream self-closes. */
async function seedCompletedRun(store: MemoryAssistantStore): Promise<string> {
  const runId = "task-cors-stream";
  await store.tasks.createTask({
    taskId: runId,
    kind: "chat.turn",
    scopeId: TEST_CHAT_ID,
    payload: { chatId: TEST_CHAT_ID },
  });
  const lease = await store.tasks.acquireLease({
    taskId: runId,
    attemptId: "att-cors",
    ownerId: "owner",
    ttlMs: 60_000,
  });
  const events: AiRunEvent[] = [
    {
      type: "run.started",
      runId,
      timestamp: new Date(0).toISOString(),
      contractVersion: CONTRACT_VERSION,
      eventId: "evt-0",
      seq: 0,
      data: { model: "m1", toolCount: 0 },
    } as AiRunEvent,
    {
      type: "run.completed",
      runId,
      timestamp: new Date(1000).toISOString(),
      contractVersion: CONTRACT_VERSION,
      eventId: "evt-1",
      seq: 1,
      data: { iterations: 1 },
    } as AiRunEvent,
  ];
  await store.tasks.appendEvents(runId, events as TaskEventEnvelope[], {
    leaseToken: lease.leaseToken,
  });
  await store.tasks.transitionTask(runId, ["queued"], "running");
  await store.tasks.transitionTask(runId, ["running"], "completed");
  return runId;
}
