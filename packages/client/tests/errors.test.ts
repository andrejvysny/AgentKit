/**
 * Problem responses as typed exceptions, and the two things a client must never
 * do with a credential.
 */
import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import {
  AgentKitClientError,
  createAgentKitClient,
  isAgentKitClientError,
} from "../src/index.js";
import {
  SEEDED_API_KEY,
  startTestServer,
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

describe("problem+json becomes a typed error", () => {
  test("a 404 carries status, code, detail and the raw problem", async () => {
    const thrown = await client
      .getChat({ chatId: "chat-nope" })
      .then(() => null)
      .catch((err: unknown) => err);

    expect(thrown).toBeInstanceOf(AgentKitClientError);
    expect(isAgentKitClientError(thrown)).toBe(true);
    const err = thrown as AgentKitClientError;
    expect(err.name).toBe("AgentKitClientError");
    expect(err.status).toBe(404);
    expect(err.code).toBe("not_found");
    expect(err.detail).toBe("Chat not found: chat-nope");
    expect(err.problem).toMatchObject({
      type: "https://agentkit.dev/problems/not_found",
      title: "Not Found",
      status: 404,
      code: "not_found",
      instance: "/v1/chats/chat-nope",
    });
    // The message names the call, so a log line is useful without the object.
    expect(err.message).toContain("GET");
    expect(err.message).toContain("/v1/chats/chat-nope");
    expect(err.message).toContain("not_found");
  });

  test("a 409 is distinguished by its code, not its status", async () => {
    // `p1` is seeded; a create that reused the id would silently replace it.
    const thrown = await client
      .createProvider({
        id: "p1",
        label: "Second",
        kind: "openai-compatible",
        baseUrl: "http://localhost:9999",
        defaultModel: "m9",
      })
      .then(() => null)
      .catch((err: unknown) => err as AgentKitClientError);

    expect(thrown).toBeInstanceOf(AgentKitClientError);
    expect(thrown?.status).toBe(409);
    expect(thrown?.code).toBe("duplicate_provider");
  });

  test("a rejected query parameter is a 400 with the transport's code", async () => {
    const thrown = await client
      .listChats({ limit: -3 })
      .then(() => null)
      .catch((err: unknown) => err as AgentKitClientError);

    expect(thrown?.status).toBe(400);
    expect(thrown?.code).toBe("invalid_request");
  });

  test("a body that is not problem+json still becomes the same error", async () => {
    const rude = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: async () =>
        new Response("<html>502 Bad Gateway</html>", {
          status: 502,
          statusText: "Bad Gateway",
          headers: { "content-type": "text/html" },
        }),
    });

    const thrown = await rude
      .getVersion()
      .then(() => null)
      .catch((err: unknown) => err as AgentKitClientError);

    expect(thrown).toBeInstanceOf(AgentKitClientError);
    expect(thrown?.status).toBe(502);
    // Recognisably NOT a contract code, and the body is kept for the log.
    expect(thrown?.code).toBe("http_502");
    expect(thrown?.detail).toBeUndefined();
    expect(thrown?.problem).toBe("<html>502 Bad Gateway</html>");
  });

  test("a 204 resolves to void rather than a parse failure", async () => {
    const created = await client.createProvider({
      label: "Throwaway",
      kind: "openai-compatible",
      baseUrl: "http://localhost:9",
      defaultModel: "m",
    });
    await expect(
      client.deleteProvider({ providerId: created.id }),
    ).resolves.toBeUndefined();
    expect(
      (await client.listProviders()).some((p) => p.id === created.id),
    ).toBe(false);
  });
});

describe("credentials never come back", () => {
  test("createProvider takes an apiKey and no response ever repeats it", async () => {
    const secret = "sk-client-test-0123456789";
    const created = await client.createProvider({
      label: "Keyed",
      kind: "openai-compatible",
      baseUrl: "http://localhost:4321",
      defaultModel: "m1",
      apiKey: secret,
      extraHeaders: { "x-gateway-token": "gw-should-never-be-published" },
    });

    // The response says a credential EXISTS, by ref, and says nothing else.
    expect(created.apiKeySecretRef).toBe(`provider/${created.id}/api-key`);
    expect(JSON.stringify(created)).not.toContain(secret);
    expect(JSON.stringify(created)).not.toContain(
      "gw-should-never-be-published",
    );

    const listed = await client.listProviders();
    const bytes = JSON.stringify(listed);
    expect(bytes).not.toContain(secret);
    expect(bytes).not.toContain("gw-should-never-be-published");
    // The provider seeded with a key on the store side is covered by the same
    // projection: nothing that ever went in comes back out.
    expect(bytes).not.toContain(SEEDED_API_KEY);

    // It went to the SecretStore, which is the one place built to hold it.
    expect(await server.secrets.get(`provider/${created.id}/api-key`)).toBe(
      secret,
    );

    // And the raw wire bytes, not just the parsed object.
    const raw = await fetch(`${server.baseUrl}/v1/providers`);
    expect(await raw.text()).not.toContain(secret);
  });

  test("updateProvider replaces the stored credential without publishing it", async () => {
    const created = await client.createProvider({
      label: "Keyed",
      kind: "openai-compatible",
      baseUrl: "http://localhost:4321",
      defaultModel: "m1",
      apiKey: "sk-first",
    });
    const updated = await client.updateProvider(
      { providerId: created.id },
      { apiKey: "sk-second", label: "Rekeyed" },
    );
    expect(updated.label).toBe("Rekeyed");
    expect(JSON.stringify(updated)).not.toContain("sk-second");
    expect(await server.secrets.get(`provider/${created.id}/api-key`)).toBe(
      "sk-second",
    );
  });
});
