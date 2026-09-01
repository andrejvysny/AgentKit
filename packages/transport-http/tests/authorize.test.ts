/**
 * `deps.authorize` — the `AuthorizationPort`, enforced per route.
 *
 * The table-driven case at the bottom is the one that matters most: it walks
 * `REST_ROUTES` itself, so a route added to the contract without a resource
 * mapping fails here as well as at compile time.
 */
import { describe, expect, it } from "bun:test";
import { REST_ROUTES, type RestOperation } from "@agentkit/contracts";
import type {
  AuthorizationDecision,
  AuthorizationPort,
  AuthorizationRequest,
} from "@agentkit/host";
import { resourceForOperation } from "../src/authorize.js";
import { matchRoute } from "../src/router.js";
import {
  createHandlerFixture,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

/** An authorizer with a fixed answer that remembers every question. */
class RecordingAuthorizer implements AuthorizationPort {
  readonly calls: AuthorizationRequest[] = [];

  constructor(private readonly answer: AuthorizationDecision) {}

  async authorize(req: AuthorizationRequest): Promise<AuthorizationDecision> {
    this.calls.push(req);
    return this.answer;
  }
}

const DENY = (reason?: string): AuthorizationPort =>
  new RecordingAuthorizer(
    reason === undefined ? { allowed: false } : { allowed: false, reason },
  );

describe("REST authorization — refusal", () => {
  it("answers 403 problem+json with the port's reason", async () => {
    const f = await createHandlerFixture({
      authorize: DENY("Not a member of this workspace."),
    });

    const res = await f.handler(request("GET", `/v1/chats/${TEST_CHAT_ID}`));
    expect(res.status).toBe(403);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
    const body = (await res.json()) as Record<string, unknown>;
    expect(body["code"]).toBe("forbidden");
    expect(body["title"]).toBe("Forbidden");
    expect(body["detail"]).toBe("Not a member of this workspace.");
    expect(body["instance"]).toBe(`/v1/chats/${TEST_CHAT_ID}`);
  });

  it("falls back to a generic detail when the port refuses without a reason", async () => {
    const f = await createHandlerFixture({ authorize: DENY() });
    const res = await f.handler(request("GET", "/v1/chats"));
    const body = (await res.json()) as Record<string, unknown>;
    expect(body["detail"]).toBe("Not allowed to read this chat.");
  });

  it("refuses writes before the host is touched at all", async () => {
    const f = await createHandlerFixture({ authorize: DENY("nope") });
    const res = await f.handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content: "hello" },
        headers: { "idempotency-key": "key-1" },
      }),
    );
    expect(res.status).toBe(403);
    // No turn was submitted: the queue never heard about it.
    expect(f.runner.enqueued).toEqual([]);
  });

  it("refuses a stream before any SSE response exists", async () => {
    const f = await createHandlerFixture({ authorize: DENY("nope") });
    const res = await f.handler(request("GET", "/v1/runs/whatever/stream"));
    expect(res.status).toBe(403);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
    await res.text();
  });
});

describe("REST authorization — what the port is asked", () => {
  it("threads the authenticated principal through as the subject", async () => {
    const authorize = new RecordingAuthorizer({ allowed: true });
    const f = await createHandlerFixture({
      authenticate: async () => ({ userId: "u1", roles: ["admin"] }),
      authorize,
    });

    const res = await f.handler(request("GET", `/v1/chats/${TEST_CHAT_ID}`));
    expect(res.status).toBe(200);
    expect(authorize.calls).toEqual([
      {
        subject: { userId: "u1", roles: ["admin"] },
        action: "read",
        resource: { kind: "chat", id: TEST_CHAT_ID },
      },
    ]);
  });

  it("carries a non-object principal under metadata rather than guessing at it", async () => {
    const authorize = new RecordingAuthorizer({ allowed: true });
    const f = await createHandlerFixture({
      authenticate: async () => "token-abc",
      authorize,
    });
    await f.handler(request("GET", "/v1/providers"));
    expect(authorize.calls[0]?.subject).toEqual({
      metadata: { principal: "token-abc" },
    });
  });

  it("passes an empty subject when no authenticator is wired", async () => {
    const authorize = new RecordingAuthorizer({ allowed: true });
    const f = await createHandlerFixture({ authorize });
    await f.handler(request("GET", "/v1/providers"));
    expect(authorize.calls[0]?.subject).toEqual({});
  });

  it("uses `read` for GET and `write` for everything else", async () => {
    const authorize = new RecordingAuthorizer({ allowed: true });
    const f = await createHandlerFixture({ authorize });

    await f.handler(request("GET", `/v1/chats/${TEST_CHAT_ID}/messages`));
    await f.handler(
      request("POST", "/v1/chats", { body: { title: "New chat" } }),
    );

    expect(authorize.calls.map((call) => call.action)).toEqual([
      "read",
      "write",
    ]);
  });

  it("never consults the port for GET /v1/version", async () => {
    const authorize = new RecordingAuthorizer({ allowed: false });
    const f = await createHandlerFixture({ authorize });

    const res = await f.handler(request("GET", "/v1/version"));
    expect(res.status).toBe(200);
    expect(authorize.calls).toEqual([]);
  });

  it("is not consulted for a 404 or a 405 — routing decides those first", async () => {
    const authorize = new RecordingAuthorizer({ allowed: false });
    const f = await createHandlerFixture({ authorize });

    expect((await f.handler(request("GET", "/v1/nope"))).status).toBe(404);
    expect((await f.handler(request("DELETE", "/v1/chats"))).status).toBe(405);
    expect(authorize.calls).toEqual([]);
  });
});

describe("REST authorization — port absent", () => {
  it("checks nothing: an unwired port is no enforcement", async () => {
    const f = await createHandlerFixture();
    const res = await f.handler(request("GET", `/v1/chats/${TEST_CHAT_ID}`));
    expect(res.status).toBe(200);
  });
});

/**
 * Every contract route resolves to a resource — walked from `REST_ROUTES`, so a
 * route added there without a mapping fails this test even if someone widened
 * the type that is supposed to catch it at compile time.
 */
describe("resourceForOperation — over the whole route table", () => {
  /** `/v1/chats/:chatId` → `/v1/chats/id-chatId`, so the id is traceable. */
  function concretePath(path: string): string {
    return path
      .split("/")
      .map((segment) =>
        segment.startsWith(":") ? `id-${segment.slice(1)}` : segment,
      )
      .join("/");
  }

  const operations = Object.keys(REST_ROUTES) as RestOperation[];

  it("covers every operation in the contract (sanity: the table is not empty)", () => {
    expect(operations.length).toBeGreaterThan(15);
  });

  for (const operation of operations) {
    const def = REST_ROUTES[operation];

    it(`${operation} (${def.method} ${def.path}) resolves`, () => {
      const match = matchRoute(def.method, concretePath(def.path));
      if (match.kind !== "matched") {
        throw new Error(`${operation} did not route to itself`);
      }
      expect(match.operation).toBe(operation);

      const resource = resourceForOperation(operation, match.params);

      // `getVersion` is the one deliberate exemption; everything else must name
      // a resource kind, and must carry the path's id when the path has one.
      if (operation === "getVersion") {
        expect(resource).toBeNull();
        return;
      }
      expect(resource).not.toBeNull();
      expect(resource?.kind.length).toBeGreaterThan(0);

      const paramNames = Object.keys(match.params);
      if (paramNames.length === 0) {
        expect(resource?.id).toBeUndefined();
      } else {
        expect(paramNames.length).toBe(1);
        expect(resource?.id).toBe(`id-${paramNames[0]}`);
      }
    });
  }

  it("maps each route to the kind the path actually names", () => {
    const kinds = Object.fromEntries(
      operations.map((operation) => {
        const def = REST_ROUTES[operation];
        const match = matchRoute(def.method, concretePath(def.path));
        if (match.kind !== "matched") throw new Error(operation);
        return [
          operation,
          resourceForOperation(operation, match.params)?.kind ?? null,
        ];
      }),
    );

    expect(kinds).toEqual({
      createChat: "chat",
      listChats: "chat",
      getChat: "chat",
      listMessages: "chat",
      submitMessage: "chat",
      forkChat: "chat",
      activateBranch: "message",
      listSiblings: "message",
      getRun: "run",
      streamRun: "run",
      cancelRun: "run",
      listToolEvents: "chat",
      listProposals: "chat",
      approveProposal: "proposal",
      rejectProposal: "proposal",
      applyProposal: "proposal",
      listProviders: "provider",
      listModels: "provider",
      listTools: "tools",
      getVersion: null,
    });
  });
});
