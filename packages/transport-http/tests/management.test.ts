/**
 * The management surface: chat lifecycle, search, provider CRUD, settings,
 * write-policy allowances and MCP server configs.
 *
 * Two things here carry more weight than the CRUD around them. The first is
 * that a provider's `apiKey` must reach the `SecretStore` and NOTHING else —
 * not the response, not the stored config, not a later read — which is asserted
 * against the raw response text rather than a parsed field, because a leak is
 * whatever ends up in the bytes. The second is the 501 for every optional
 * dependency: the routes exist in the contract, and a deployment that cannot
 * serve one has to say so rather than 404 and send a client hunting for a typo.
 */
import { describe, expect, it } from "bun:test";
import type {
  ChatDto,
  McpServerDto,
  MessageSearchResponse,
  ProviderDto,
  SettingsDto,
  TestProviderResponse,
  WriteAllowanceDto,
  WriteAllowanceListResponse,
} from "@agentkit/contracts";
import { MemoryMcpServerConfigStore } from "@agentkit/adapters-memory";
import { ConversationService, SessionWritePolicy } from "@agentkit/host";
import {
  createHandlerFixture,
  MemorySecretStore,
  request,
  TEST_CHAT_ID,
} from "./support/fixture.js";

const SECRET = "sk-live-never-publish-me";

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
}

describe("chat lifecycle", () => {
  it("(a) patches title, metadata and archived, and reports archived on every read", async () => {
    const { handler } = await createHandlerFixture();

    const before = (await (
      await handler(request("GET", `/v1/chats/${TEST_CHAT_ID}`))
    ).json()) as ChatDto;
    // Required, not optional: a client rendering a checkbox needs a boolean,
    // not an absence it has to guess the meaning of.
    expect(before.archived).toBe(false);

    const res = await handler(
      request("PATCH", `/v1/chats/${TEST_CHAT_ID}`, {
        body: { title: "Renamed", metadata: { pinned: true }, archived: true },
      }),
    );
    expect(res.status).toBe(200);
    const patched = (await res.json()) as ChatDto;
    expect(patched.title).toBe("Renamed");
    expect(patched.metadata).toEqual({ pinned: true });
    expect(patched.archived).toBe(true);

    // Archiving hides it from the default listing — that is what archiving IS.
    const listed = (await (
      await handler(request("GET", "/v1/chats"))
    ).json()) as ChatDto[];
    expect(listed.some((chat) => chat.id === TEST_CHAT_ID)).toBe(false);
  });

  it("(b) 404s a patch of an unknown chat and 400s a mistyped field", async () => {
    const { handler } = await createHandlerFixture();
    await expectProblem(
      await handler(request("PATCH", "/v1/chats/missing", { body: {} })),
      404,
      "not_found",
    );
    await expectProblem(
      await handler(
        request("PATCH", `/v1/chats/${TEST_CHAT_ID}`, {
          body: { archived: "yes" },
        }),
      ),
      400,
      "invalid_request",
    );
  });

  it("(c) 501s deleteChat with no ConversationService, 204s with one", async () => {
    const bare = await createHandlerFixture();
    await expectProblem(
      await bare.handler(request("DELETE", `/v1/chats/${TEST_CHAT_ID}`)),
      501,
      "not_implemented",
    );

    const f = await createHandlerFixture();
    const wired = await createHandlerFixture({
      store: f.store,
      conversations: new ConversationService({ store: f.store }),
    });
    const res = await wired.handler(
      request("DELETE", `/v1/chats/${TEST_CHAT_ID}`),
    );
    expect(res.status).toBe(204);
    expect(await res.text()).toBe("");
    expect(await f.store.conversations.getChat(TEST_CHAT_ID)).toBeNull();
  });

  it("(d) 409s a delete while a run in the chat is still live", async () => {
    const f = await createHandlerFixture();
    const wired = await createHandlerFixture({
      store: f.store,
      conversations: new ConversationService({ store: f.store }),
    });
    const submitted = await f.handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/messages`, {
        body: { content: "hello" },
        headers: { "idempotency-key": "busy-1" },
      }),
    );
    expect(submitted.status).toBe(201);
    // The fixture's queue runs nothing, so drive the task into `running` the
    // way a worker would — the state `chat_busy` is actually about.
    const runId = ((await submitted.json()) as { runId: string }).runId;
    await f.store.tasks.transitionTask(runId, ["queued"], "running");

    await expectProblem(
      await wired.handler(request("DELETE", `/v1/chats/${TEST_CHAT_ID}`)),
      409,
      "chat_busy",
    );
    expect(await f.store.conversations.getChat(TEST_CHAT_ID)).not.toBeNull();
  });
});

describe("searchMessages", () => {
  it("(e) finds a message, scopes on chatId, and requires q", async () => {
    const f = await createHandlerFixture();
    await f.store.conversations.appendMessage({
      chatId: TEST_CHAT_ID,
      role: "user",
      content: "the zebrafish footprint is wrong",
    });
    const other = await f.store.conversations.createChat({ id: "chat-other" });
    await f.store.conversations.appendMessage({
      chatId: other.id,
      role: "user",
      content: "another zebrafish sighting",
    });

    const all = (await (
      await f.handler(request("GET", "/v1/search?q=zebrafish"))
    ).json()) as MessageSearchResponse;
    expect(all.hits.length).toBe(2);
    expect(all.hits[0]?.snippet).toContain("zebrafish");

    const scoped = (await (
      await f.handler(
        request("GET", `/v1/search?q=zebrafish&chatId=${TEST_CHAT_ID}`),
      )
    ).json()) as MessageSearchResponse;
    expect(scoped.hits.map((hit) => hit.chatId)).toEqual([TEST_CHAT_ID]);

    await expectProblem(
      await f.handler(request("GET", "/v1/search")),
      400,
      "invalid_request",
    );
  });

  it("(f) 501s when the store cannot search", async () => {
    const f = await createHandlerFixture();
    // A store that never implemented the OPTIONAL port method. 501 rather than
    // an empty result: "nothing matched" and "I cannot search" are different
    // answers, and a client can only act on one of them.
    const blind = {
      ...f.store,
      conversations: new Proxy(f.store.conversations, {
        get: (target, prop, receiver) =>
          prop === "searchMessages"
            ? undefined
            : Reflect.get(target, prop, receiver),
      }),
    };
    const handler = (
      await createHandlerFixture({
        store: blind as unknown as typeof f.store,
      })
    ).handler;
    await expectProblem(
      await handler(request("GET", "/v1/search?q=anything")),
      501,
      "not_implemented",
    );
  });
});

describe("provider CRUD", () => {
  it("(g) creates with a write-only apiKey: the store gets the KEY, the wire gets the REF", async () => {
    const secrets = new MemorySecretStore();
    const f = await createHandlerFixture({ secrets });

    const res = await f.handler(
      request("POST", "/v1/providers", {
        body: {
          id: "p-new",
          label: "New",
          kind: "openai-compatible",
          baseUrl: "http://localhost:9",
          defaultModel: "m1",
          apiKey: SECRET,
        },
      }),
    );
    expect(res.status).toBe(201);
    const text = await res.text();
    expect(text).not.toContain(SECRET);
    const created = JSON.parse(text) as ProviderDto;
    expect(created.apiKeySecretRef).toBe("provider/p-new/api-key");

    // The key is in the secret store and nowhere else: the stored config
    // carries the ref, and its own `apiKey` field was never written.
    expect(secrets.values.get("provider/p-new/api-key")).toBe(SECRET);
    const stored = await f.store.providers.getProvider("p-new");
    expect(stored?.apiKey).toBeUndefined();
    expect(stored?.metadata?.["apiKeySecretRef"]).toBe(
      "provider/p-new/api-key",
    );

    // And the echo through the listing carries the ref, never the key.
    const listing = await f.handler(request("GET", "/v1/providers"));
    const listingText = await listing.text();
    expect(listingText).not.toContain(SECRET);
    const providers = JSON.parse(listingText) as ProviderDto[];
    expect(providers.find((p) => p.id === "p-new")?.apiKeySecretRef).toBe(
      "provider/p-new/api-key",
    );
  });

  it("(h) 501s an apiKey with no SecretStore, rather than storing it in the config", async () => {
    const f = await createHandlerFixture();
    await expectProblem(
      await f.handler(
        request("POST", "/v1/providers", {
          body: {
            label: "New",
            kind: "openai-compatible",
            baseUrl: "http://localhost:9",
            defaultModel: "m1",
            apiKey: SECRET,
          },
        }),
      ),
      501,
      "not_implemented",
    );
    // Refused, not half-applied.
    expect((await f.store.providers.listProviders()).length).toBe(1);

    // The same request WITHOUT a key needs no secret store at all.
    const ok = await f.handler(
      request("POST", "/v1/providers", {
        body: {
          label: "Keyless",
          kind: "ollama",
          baseUrl: "http://localhost:11434",
          defaultModel: "llama",
        },
      }),
    );
    expect(ok.status).toBe(201);
    expect((await ok.json()) as ProviderDto).not.toHaveProperty(
      "apiKeySecretRef",
    );
  });

  it("(h2) persists the CONFIG before the secret, and 500s when the secret write fails", async () => {
    // A store that cannot hold a key. The two failures are not symmetric: a
    // config naming a ref the store lacks is fixed by re-sending the `apiKey`
    // (the ref is derived from the provider id, so the retry overwrites), while
    // a secret written for a config that never landed is a live credential
    // under a ref nothing names.
    const secrets = {
      async get() {
        return null;
      },
      async set() {
        throw new Error("secret store is down");
      },
      async delete() {},
      async listRefs() {
        return [];
      },
    };

    const created = await createHandlerFixture({ secrets });
    const res = await created.handler(
      request("POST", "/v1/providers", {
        body: {
          id: "p-half",
          label: "Half",
          kind: "openai-compatible",
          baseUrl: "http://localhost:9",
          defaultModel: "m1",
          apiKey: SECRET,
        },
      }),
    );
    expect(res.status).toBe(500);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
    expect((await res.json()) as Record<string, unknown>).toMatchObject({
      code: "secret_write_failed",
    });
    // The recoverable half landed: the provider is there, pointing at the ref a
    // retry will fill.
    const stored = await created.store.providers.getProvider("p-half");
    expect(stored?.label).toBe("Half");
    expect(stored?.metadata?.["apiKeySecretRef"]).toBe(
      "provider/p-half/api-key",
    );
    expect(stored?.apiKey).toBeUndefined();

    // Same order on the update path.
    const patched = await createHandlerFixture({ secrets });
    const patch = await patched.handler(
      request("PATCH", "/v1/providers/p1", {
        body: { label: "Renamed", apiKey: SECRET },
      }),
    );
    expect(patch.status).toBe(500);
    const after = await patched.store.providers.getProvider("p1");
    expect(after?.label).toBe("Renamed");
    expect(after?.metadata?.["apiKeySecretRef"]).toBe("provider/p1/api-key");
  });

  it("(i) 409s a create over an existing id instead of overwriting it", async () => {
    const f = await createHandlerFixture();
    await expectProblem(
      await f.handler(
        request("POST", "/v1/providers", {
          body: {
            id: "p1",
            label: "Impostor",
            kind: "openai",
            baseUrl: "http://elsewhere",
            defaultModel: "m9",
          },
        }),
      ),
      409,
      "duplicate_provider",
    );
    expect((await f.store.providers.getProvider("p1"))?.label).toBe("Mock");
  });

  it("(j) patches only what it names, and keeps the secret ref across a metadata replace", async () => {
    const secrets = new MemorySecretStore();
    const f = await createHandlerFixture({ secrets });
    await f.handler(
      request("PATCH", "/v1/providers/p1", { body: { apiKey: SECRET } }),
    );

    const res = await f.handler(
      request("PATCH", "/v1/providers/p1", {
        body: { label: "Renamed", enabled: false, metadata: {} },
      }),
    );
    expect(res.status).toBe(200);
    const patched = (await res.json()) as ProviderDto;
    expect(patched.label).toBe("Renamed");
    expect(patched.enabled).toBe(false);
    // Untouched fields survive.
    expect(patched.baseUrl).toBe("http://localhost:1234");
    // A client clearing its own metadata tags must not unlink the credential —
    // the next turn would call the vendor unauthenticated.
    expect(patched.apiKeySecretRef).toBe("provider/p1/api-key");
    expect(secrets.values.get("provider/p1/api-key")).toBe(SECRET);
  });

  it("(k) deletes the provider and its stored credential", async () => {
    const secrets = new MemorySecretStore();
    const f = await createHandlerFixture({ secrets });
    await f.handler(
      request("PATCH", "/v1/providers/p1", { body: { apiKey: SECRET } }),
    );
    expect(secrets.values.size).toBe(1);

    const res = await f.handler(request("DELETE", "/v1/providers/p1"));
    expect(res.status).toBe(204);
    expect(await f.store.providers.getProvider("p1")).toBeNull();
    // A live key under a ref nothing points at any more is the worst kind of
    // leak: unreachable through the API and impossible to notice.
    expect(secrets.values.size).toBe(0);

    await expectProblem(
      await f.handler(request("DELETE", "/v1/providers/p1")),
      404,
      "not_found",
    );
  });

  it("(l) 501s refresh and test without providerOps, serves them with it", async () => {
    const bare = await createHandlerFixture();
    await expectProblem(
      await bare.handler(request("POST", "/v1/providers/p1/models/refresh")),
      501,
      "not_implemented",
    );
    await expectProblem(
      await bare.handler(request("POST", "/v1/providers/p1/test")),
      501,
      "not_implemented",
    );

    const asked: string[] = [];
    const f = await createHandlerFixture({
      providerOps: {
        async refreshModels(providerId) {
          asked.push(`refresh:${providerId}`);
          return [
            {
              providerId,
              modelId: "m2",
              displayName: "Fresh",
              fetchedAt: new Date(0).toISOString(),
            },
          ];
        },
        async testConnection(providerId) {
          asked.push(`test:${providerId}`);
          return { ok: false, error: "connection refused" };
        },
      },
    });

    const refreshed = await f.handler(
      request("POST", "/v1/providers/p1/models/refresh"),
    );
    expect(refreshed.status).toBe(200);
    expect(
      ((await refreshed.json()) as { modelId: string }[]).map((m) => m.modelId),
    ).toEqual(["m2"]);

    // A failed probe is a 200: the request succeeded, and the result is the
    // answer. A 4xx would make "the endpoint is down" look like a bad request.
    const tested = await f.handler(request("POST", "/v1/providers/p1/test"));
    expect(tested.status).toBe(200);
    expect((await tested.json()) as TestProviderResponse).toEqual({
      ok: false,
      error: "connection refused",
    });
    expect(asked).toEqual(["refresh:p1", "test:p1"]);

    // An unknown provider is a 404 before the host is asked anything.
    await expectProblem(
      await f.handler(request("POST", "/v1/providers/nope/test")),
      404,
      "not_found",
    );
    expect(asked).toEqual(["refresh:p1", "test:p1"]);
  });
});

describe("settings", () => {
  it("(m) reads the row and applies a partial patch, toolCalling included", async () => {
    const { handler } = await createHandlerFixture();
    const initial = (await (
      await handler(request("GET", "/v1/settings"))
    ).json()) as SettingsDto;
    expect(initial.defaultProviderId).toBe("p1");
    expect(initial.writePolicyMode).toBe("auto_readonly_confirm_writes");
    expect(initial.toolCalling).toBe("auto");

    const res = await handler(
      request("PATCH", "/v1/settings", {
        body: { toolCalling: "off", allowRawToolData: true },
      }),
    );
    expect(res.status).toBe(200);
    const patched = (await res.json()) as SettingsDto;
    expect(patched.toolCalling).toBe("off");
    expect(patched.allowRawToolData).toBe(true);
    // Untouched fields are untouched.
    expect(patched.defaultProviderId).toBe("p1");
  });

  it("(n) 400s a value outside a closed union", async () => {
    const { handler } = await createHandlerFixture();
    await expectProblem(
      await handler(
        request("PATCH", "/v1/settings", { body: { toolCalling: "maybe" } }),
      ),
      400,
      "invalid_request",
    );
    await expectProblem(
      await handler(
        request("PATCH", "/v1/settings", {
          // A hyphen instead of an underscore: stored, it would read back as a
          // mode nothing matches and silently confirm every write forever.
          body: { writePolicyMode: "auto-all" },
        }),
      ),
      400,
      "invalid_request",
    );
  });
});

describe("write-policy allowances", () => {
  it("(o) 501s all three without a policy", async () => {
    const { handler } = await createHandlerFixture();
    await expectProblem(
      await handler(request("GET", "/v1/chats/c1/write-policy/allowances")),
      501,
      "not_implemented",
    );
    await expectProblem(
      await handler(
        request("POST", "/v1/chats/c1/write-policy/allowances", { body: {} }),
      ),
      501,
      "not_implemented",
    );
    await expectProblem(
      await handler(
        request("DELETE", "/v1/chats/c1/write-policy/allowances/k"),
      ),
      501,
      "not_implemented",
    );
  });

  it("(p) grants, lists and revokes — all scoped to one chat", async () => {
    const writePolicy = new SessionWritePolicy();
    const { handler } = await createHandlerFixture({ writePolicy });

    const granted = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/write-policy/allowances`, {
        body: {
          toolName: "notes_append",
          proposalKind: "notes.append",
          maxRisk: "medium",
        },
      }),
    );
    expect(granted.status).toBe(201);
    const allowance = (await granted.json()) as WriteAllowanceDto;
    expect(allowance.maxRisk).toBe("medium");
    expect(allowance.chatId).toBe(TEST_CHAT_ID);

    // The grant is real: the policy now auto-applies at or below the ceiling,
    // and refuses above it.
    expect(
      writePolicy.isAutoApplyAllowed({
        chatId: TEST_CHAT_ID,
        toolName: "notes_append",
        proposalKind: "notes.append",
        risk: "low",
      }),
    ).toBe(true);
    expect(
      writePolicy.isAutoApplyAllowed({
        chatId: TEST_CHAT_ID,
        toolName: "notes_append",
        proposalKind: "notes.append",
        risk: "high",
      }),
    ).toBe(false);

    const listed = (await (
      await handler(
        request("GET", `/v1/chats/${TEST_CHAT_ID}/write-policy/allowances`),
      )
    ).json()) as WriteAllowanceListResponse;
    expect(listed.allowances.map((a) => a.key)).toEqual([allowance.key]);

    // Another chat sees none of it — consent does not travel.
    const otherChat = (await (
      await handler(request("GET", "/v1/chats/other/write-policy/allowances"))
    ).json()) as WriteAllowanceListResponse;
    expect(otherChat.allowances).toEqual([]);

    // And cannot revoke it either, even holding the key.
    const wrongChat = await handler(
      request(
        "DELETE",
        `/v1/chats/other/write-policy/allowances/${encodeURIComponent(allowance.key)}`,
      ),
    );
    expect(wrongChat.status).toBe(204);
    expect(writePolicy.list(TEST_CHAT_ID).length).toBe(1);

    const revoked = await handler(
      request(
        "DELETE",
        `/v1/chats/${TEST_CHAT_ID}/write-policy/allowances/${encodeURIComponent(allowance.key)}`,
      ),
    );
    expect(revoked.status).toBe(204);
    expect(writePolicy.list(TEST_CHAT_ID)).toEqual([]);
  });

  it("(q) 400s a grant missing a field, and 404s the chatless paths the routes no longer have", async () => {
    const { handler } = await createHandlerFixture({
      writePolicy: new SessionWritePolicy(),
    });
    await expectProblem(
      await handler(
        request("POST", `/v1/chats/${TEST_CHAT_ID}/write-policy/allowances`, {
          body: { toolName: "t", proposalKind: "k" },
        }),
      ),
      400,
      "invalid_request",
    );
    // The chat is a PATH parameter now, so the old policy-rooted URLs are not
    // routes at all — there is no shape of this request whose chat the
    // authorizer cannot see.
    expect(
      (await handler(request("GET", "/v1/write-policy/allowances"))).status,
    ).toBe(404);
    expect(
      (await handler(request("DELETE", "/v1/write-policy/allowances/k")))
        .status,
    ).toBe(404);
  });

  it("(q2) authorizes every allowance route as the CHAT's policy", async () => {
    const asked: { action: string; resource: unknown }[] = [];
    const { handler } = await createHandlerFixture({
      writePolicy: new SessionWritePolicy(),
      authorize: {
        async authorize({ action, resource }) {
          asked.push({ action, resource });
          return { allowed: resource.id === TEST_CHAT_ID };
        },
      },
    });

    const listed = await handler(
      request("GET", `/v1/chats/${TEST_CHAT_ID}/write-policy/allowances`),
    );
    expect(listed.status).toBe(200);

    // The grant's chat is the path's, and it is the id the port was handed —
    // which is the whole reason it moved out of the body.
    const granted = await handler(
      request("POST", `/v1/chats/${TEST_CHAT_ID}/write-policy/allowances`, {
        body: { toolName: "t", proposalKind: "k", maxRisk: "low" },
      }),
    );
    expect(granted.status).toBe(201);

    const revoked = await handler(
      request(
        "DELETE",
        `/v1/chats/${TEST_CHAT_ID}/write-policy/allowances/whatever`,
      ),
    );
    expect(revoked.status).toBe(204);

    expect(asked).toEqual([
      { action: "read", resource: { kind: "policy", id: TEST_CHAT_ID } },
      { action: "write", resource: { kind: "policy", id: TEST_CHAT_ID } },
      { action: "write", resource: { kind: "policy", id: TEST_CHAT_ID } },
    ]);

    // Another chat's grant is refused on the id the path carried, not waved
    // through because the decision had nothing to read.
    const forbidden = await handler(
      request("POST", "/v1/chats/other/write-policy/allowances", {
        body: { toolName: "t", proposalKind: "k", maxRisk: "low" },
      }),
    );
    expect(forbidden.status).toBe(403);
  });
});

describe("MCP server configs", () => {
  const STDIO = {
    kind: "stdio" as const,
    command: "notes-server",
    args: ["--stdio"],
    env: { NOTES_TOKEN: "${token}" },
  };

  it("(r) 501s all four without a store", async () => {
    const { handler } = await createHandlerFixture();
    for (const req of [
      request("GET", "/v1/mcp/servers"),
      request("POST", "/v1/mcp/servers", { body: {} }),
      request("PATCH", "/v1/mcp/servers/x", { body: {} }),
      request("DELETE", "/v1/mcp/servers/x"),
    ]) {
      await expectProblem(await handler(req), 501, "not_implemented");
    }
  });

  it("(s) round-trips a config, refs and all, and never a secret VALUE", async () => {
    const mcpConfigs = new MemoryMcpServerConfigStore();
    const { handler } = await createHandlerFixture({ mcpConfigs });

    const res = await handler(
      request("POST", "/v1/mcp/servers", {
        body: {
          alias: "notes",
          transport: STDIO,
          secretRefs: { token: "secret/notes-token" },
          toolAliases: { list_notes: "list" },
        },
      }),
    );
    expect(res.status).toBe(201);
    const created = (await res.json()) as McpServerDto;
    expect(created.alias).toBe("notes");
    // Keys travel, values do not: `env` and `headers` are where a client is
    // free to put a literal token, so the projection redacts every value.
    expect(created.transport).toEqual({
      ...STDIO,
      env: { NOTES_TOKEN: "***" },
    });
    // Refs travel; the values behind them are resolved at connect time and are
    // not in the record at all, which is why the map can be published whole.
    expect(created.secretRefs).toEqual({ token: "secret/notes-token" });
    expect(created.id.startsWith("mcp_")).toBe(true);
    expect(created.createdAt).toBe(created.updatedAt);

    const listed = (await (
      await handler(request("GET", "/v1/mcp/servers"))
    ).json()) as McpServerDto[];
    expect(listed.map((row) => row.id)).toEqual([created.id]);

    const patched = (await (
      await handler(
        request("PATCH", `/v1/mcp/servers/${created.id}`, {
          body: { enabled: false, secretRefs: {} },
        }),
      )
    ).json()) as McpServerDto;
    expect(patched.enabled).toBe(false);
    // FIELD-LEVEL REPLACE: the stored refs are gone, not merged with.
    expect(patched.secretRefs).toEqual({});
    expect(patched.alias).toBe("notes");

    const deleted = await handler(
      request("DELETE", `/v1/mcp/servers/${created.id}`),
    );
    expect(deleted.status).toBe(204);
    expect(await mcpConfigs.list()).toEqual([]);
  });

  it("(s2) redacts a LITERAL env value and a header, and keeps the stored record whole", async () => {
    const mcpConfigs = new MemoryMcpServerConfigStore();
    const { handler } = await createHandlerFixture({ mcpConfigs });

    const created = (await (
      await handler(
        request("POST", "/v1/mcp/servers", {
          body: {
            alias: "gh",
            transport: {
              kind: "http",
              url: "https://mcp.example/gh",
              headers: {
                authorization: "Bearer ghp-live-never-publish-me",
                "x-trace": "on",
              },
            },
          },
        }),
      )
    ).json()) as McpServerDto;

    const asRead = await handler(request("GET", "/v1/mcp/servers"));
    const text = await asRead.text();
    expect(text).not.toContain("ghp-live-never-publish-me");
    const listed = JSON.parse(text) as McpServerDto[];
    expect(listed[0]?.transport).toEqual({
      kind: "http",
      url: "https://mcp.example/gh",
      // Every value, not just the one that looks like a credential — a header
      // this projection had to judge is a header it would eventually misjudge.
      headers: { authorization: "***", "x-trace": "***" },
    });

    // The STORE still holds the real values; only the wire is redacted, which
    // is what keeps the server able to connect.
    const stored = await mcpConfigs.get(created.id);
    expect(stored?.transport).toEqual({
      kind: "http",
      url: "https://mcp.example/gh",
      headers: {
        authorization: "Bearer ghp-live-never-publish-me",
        "x-trace": "on",
      },
    });

    // PATCH IS FIELD-LEVEL: an absent `transport` keeps the stored one, values
    // included, so a client that only wants to disable a server need not
    // resend anything it cannot read back.
    await handler(
      request("PATCH", `/v1/mcp/servers/${created.id}`, {
        body: { enabled: false },
      }),
    );
    expect((await mcpConfigs.get(created.id))?.transport).toMatchObject({
      headers: { authorization: "Bearer ghp-live-never-publish-me" },
    });

    // A PRESENT `transport` replaces wholesale — so a client that resends what
    // it read stores the redaction verbatim. That is the cost, and it is why
    // full values must be resupplied on a transport patch.
    await handler(
      request("PATCH", `/v1/mcp/servers/${created.id}`, {
        body: { transport: listed[0]?.transport },
      }),
    );
    expect((await mcpConfigs.get(created.id))?.transport).toMatchObject({
      headers: { authorization: "***" },
    });
  });

  it("(s3) stamps both timestamps from the INJECTED clock", async () => {
    const fixed = "2031-04-05T06:07:08.000Z";
    const mcpConfigs = new MemoryMcpServerConfigStore();
    const { handler } = await createHandlerFixture({
      mcpConfigs,
      clock: { now: () => new Date(fixed), nowIso: () => fixed },
    });

    const created = (await (
      await handler(
        request("POST", "/v1/mcp/servers", {
          body: { alias: "notes", transport: STDIO },
        }),
      )
    ).json()) as McpServerDto;
    expect(created.createdAt).toBe(fixed);
    expect(created.updatedAt).toBe(fixed);
  });

  it("(t) 409s a duplicate alias on create and on rename", async () => {
    const mcpConfigs = new MemoryMcpServerConfigStore();
    const { handler } = await createHandlerFixture({ mcpConfigs });
    const first = (await (
      await handler(
        request("POST", "/v1/mcp/servers", {
          body: { alias: "notes", transport: STDIO },
        }),
      )
    ).json()) as McpServerDto;
    const second = (await (
      await handler(
        request("POST", "/v1/mcp/servers", {
          body: { alias: "memos", transport: STDIO },
        }),
      )
    ).json()) as McpServerDto;

    await expectProblem(
      await handler(
        request("POST", "/v1/mcp/servers", {
          body: { alias: "notes", transport: STDIO },
        }),
      ),
      409,
      "duplicate_alias",
    );
    await expectProblem(
      await handler(
        request("PATCH", `/v1/mcp/servers/${second.id}`, {
          body: { alias: "notes" },
        }),
      ),
      409,
      "duplicate_alias",
    );
    // Re-stating a record's own alias is not a collision — an edit form resends
    // every field it read.
    const same = await handler(
      request("PATCH", `/v1/mcp/servers/${first.id}`, {
        body: { alias: "notes", enabled: false },
      }),
    );
    expect(same.status).toBe(200);
    expect((await same.json()) as McpServerDto).toMatchObject({
      alias: "notes",
      enabled: false,
    });
  });

  it("(u) rejects a transport outside the closed union, and 404s an unknown id", async () => {
    const { handler } = await createHandlerFixture({
      mcpConfigs: new MemoryMcpServerConfigStore(),
    });
    await expectProblem(
      await handler(
        request("POST", "/v1/mcp/servers", {
          body: { alias: "weird", transport: { kind: "carrier-pigeon" } },
        }),
      ),
      400,
      "invalid_request",
    );
    await expectProblem(
      await handler(
        request("POST", "/v1/mcp/servers", {
          body: { alias: "weird", transport: { kind: "http" } },
        }),
      ),
      400,
      "invalid_request",
    );
    await expectProblem(
      await handler(
        request("PATCH", "/v1/mcp/servers/nope", { body: { enabled: false } }),
      ),
      404,
      "not_found",
    );
    await expectProblem(
      await handler(request("DELETE", "/v1/mcp/servers/nope")),
      404,
      "not_found",
    );
  });
});
