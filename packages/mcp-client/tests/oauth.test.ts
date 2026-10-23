import { expect, test } from "bun:test";
import { ManagedOAuthProvider } from "../src/index.js";
import { createSecretStore } from "./helpers.js";

test("SDK OAuth performs PKCE/state, validates callback and coordinates account-scoped refresh", async () => {
  const secrets = createSecretStore();
  let now = 0;
  let authorizationUrl: URL | undefined;
  let tokenRequests = 0;
  const fetcher = async (input: string | URL | Request, init?: RequestInit) => {
    const url = new URL(
      typeof input === "string" || input instanceof URL ? input : input.url,
    );
    if (url.pathname.includes("oauth-protected-resource"))
      return Response.json({
        resource: "https://service.test/mcp",
        authorization_servers: ["https://auth.test"],
      });
    if (url.pathname.includes("well-known"))
      return Response.json({
        issuer: "https://auth.test",
        authorization_endpoint: "https://auth.test/authorize",
        token_endpoint: "https://auth.test/token",
        response_types_supported: ["code"],
        grant_types_supported: ["authorization_code", "refresh_token"],
        code_challenge_methods_supported: ["S256"],
        token_endpoint_auth_methods_supported: ["none"],
      });
    if (url.pathname === "/token") {
      tokenRequests++;
      const body = new URLSearchParams(String(init?.body));
      if (body.get("grant_type") === "authorization_code") {
        expect(body.get("code")).toBe("code");
        expect(body.get("code_verifier")).toBeTruthy();
        expect(
          authorizationUrl?.searchParams.get("code_challenge_method"),
        ).toBe("S256");
      } else {
        expect(body.get("grant_type")).toBe("refresh_token");
        expect(body.get("refresh_token")).toBe("refresh");
      }
      return Response.json({
        access_token: `token-${tokenRequests}`,
        token_type: "Bearer",
        refresh_token: "refresh",
        expires_in: 60,
      });
    }
    return new Response(null, { status: 404 });
  };
  const provider = new ManagedOAuthProvider({
    connectionId: "connection",
    accountId: "account",
    serverUrl: "https://service.test/mcp",
    redirectUrl: "http://127.0.0.1/callback",
    clientMetadata: {
      redirect_uris: ["http://127.0.0.1/callback"],
      token_endpoint_auth_method: "none",
    },
    clientInformation: { client_id: "registered" },
    secrets,
    allowedOrigins: ["https://service.test", "https://auth.test"],
    fetch: fetcher,
    now: () => now,
    redirect: async (url) => {
      authorizationUrl = url;
    },
  });
  await expect(provider.authorize()).rejects.toMatchObject({
    code: "mcp_auth_required",
  });
  expect(authorizationUrl).toBeDefined();
  await expect(
    provider.complete("http://127.0.0.1/callback?code=code&state=wrong"),
  ).rejects.toMatchObject({ code: "mcp_auth_required" });
  const callback = new URL("http://127.0.0.1/callback");
  callback.searchParams.set("code", "code");
  callback.searchParams.set(
    "state",
    authorizationUrl!.searchParams.get("state")!,
  );
  await provider.complete(callback.href);
  expect((await provider.tokens())?.access_token).toBe("token-1");
  await expect(provider.complete(callback.href)).rejects.toMatchObject({
    code: "mcp_auth_required",
  });
  now += 61_000;
  await Promise.all([
    provider.authorize(),
    provider.authorize(),
    provider.authorize(),
  ]);
  expect(tokenRequests).toBe(2);
  const other = new ManagedOAuthProvider({
    connectionId: "connection",
    accountId: "other",
    serverUrl: "https://service.test/mcp",
    redirectUrl: "http://127.0.0.1/callback",
    clientMetadata: { redirect_uris: [] },
    secrets,
    allowedOrigins: ["https://service.test"],
    redirect: async () => {},
  });
  expect(await other.tokens()).toBeUndefined();
  await expect(provider.fetch("https://auth.test/token")).rejects.toMatchObject(
    { code: "mcp_auth_required" },
  );
  await provider.disconnect();
  expect(await provider.tokens()).toBeUndefined();
  await expect(provider.authorize()).rejects.toMatchObject({
    code: "mcp_auth_required",
  });
});
