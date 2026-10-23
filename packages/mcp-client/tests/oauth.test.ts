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
    clientInformation: { client_id: "registered", issuer: "https://auth.test" },
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
  callback.searchParams.set("iss", "https://wrong.test");
  await expect(provider.complete(callback.href)).rejects.toThrow();
  expect(tokenRequests).toBe(0);
  await expect(provider.authorize()).rejects.toMatchObject({
    code: "mcp_auth_required",
  });
  callback.searchParams.set(
    "state",
    authorizationUrl!.searchParams.get("state")!,
  );
  callback.searchParams.set("iss", "https://auth.test");
  await provider.complete(callback.href);
  expect((await provider.tokens())?.access_token).toBe("token-1");
  expect((await provider.tokens())?.issuer).toBe("https://auth.test");
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

test("issuer changes never carry the old issuer refresh token into new credentials", async () => {
  const provider = new ManagedOAuthProvider({
    connectionId: "connection",
    accountId: "account",
    serverUrl: "https://service.test/mcp",
    redirectUrl: "http://127.0.0.1/callback",
    clientMetadata: { redirect_uris: [] },
    secrets: createSecretStore(),
    allowedOrigins: ["https://service.test"],
    redirect: async () => {},
  });
  await provider.saveTokens({
    access_token: "old",
    token_type: "Bearer",
    refresh_token: "old-refresh",
    issuer: "https://old.test",
  });
  await provider.saveTokens({
    access_token: "new",
    token_type: "Bearer",
    issuer: "https://new.test",
  });
  expect(await provider.tokens()).toMatchObject({
    access_token: "new",
    issuer: "https://new.test",
  });
  expect((await provider.tokens())?.refresh_token).toBeUndefined();
});

function isolatedOptions(secrets = createSecretStore()) {
  return {
    connectionId: "bound-connection",
    accountId: "bound-account",
    serverUrl: "https://service.test/mcp",
    redirectUrl: "http://127.0.0.1/callback",
    clientMetadata: { redirect_uris: ["http://127.0.0.1/callback"] },
    clientInformation: { client_id: "registered", issuer: "https://auth.test" },
    allowedOrigins: ["https://service.test", "https://auth.test"],
    secrets,
    redirect: async () => {},
  };
}

for (const drift of ["resource", "issuer", "client", "callback"] as const) {
  test(`cached OAuth credentials refuse ${drift} configuration drift before network dispatch`, async () => {
    const original = isolatedOptions();
    const first = new ManagedOAuthProvider(original);
    await first.saveTokens({
      access_token: "private-old-token",
      token_type: "Bearer",
      issuer: "https://auth.test",
      expires_in: 3600,
    });
    let requests = 0;
    const next = new ManagedOAuthProvider({
      ...original,
      ...(drift === "resource"
        ? {
            serverUrl: "https://other-resource.test/mcp",
            allowedOrigins: [
              "https://other-resource.test",
              "https://auth.test",
            ],
          }
        : {}),
      ...(drift === "issuer"
        ? {
            clientInformation: {
              client_id: "registered",
              issuer: "https://new-auth.test",
            },
            allowedOrigins: ["https://service.test", "https://new-auth.test"],
          }
        : {}),
      ...(drift === "client"
        ? {
            clientInformation: {
              client_id: "other-client",
              issuer: "https://auth.test",
            },
          }
        : {}),
      ...(drift === "callback"
        ? { redirectUrl: "http://127.0.0.1/other-callback" }
        : {}),
      fetch: async () => {
        requests++;
        return new Response("ok");
      },
    });
    await expect(next.tokens()).rejects.toMatchObject({
      code: "mcp_auth_required",
    });
    await expect(
      next.fetch(
        drift === "resource"
          ? "https://other-resource.test/mcp"
          : original.serverUrl,
      ),
    ).rejects.toMatchObject({ code: "mcp_auth_required" });
    expect(requests).toBe(0);
  });
}

test("legacy unbound OAuth material fails closed and disconnect clears it without remote revocation", async () => {
  const options = isolatedOptions();
  await options.secrets.set(
    "oauth/bound-connection/bound-account/tokens",
    JSON.stringify({
      tokens: { access_token: "legacy-token", token_type: "Bearer" },
      expiresAt: Date.now() + 3600000,
    }),
  );
  let revocations = 0;
  const provider = new ManagedOAuthProvider({
    ...options,
    revoke: async () => {
      revocations++;
    },
  });
  await expect(provider.authorize()).rejects.toMatchObject({
    code: "mcp_auth_required",
  });
  await provider.disconnect();
  expect(revocations).toBe(0);
  expect(await options.secrets.listRefs()).toHaveLength(0);
});

test("equivalent canonical resource URLs share bound credentials; resource queries remain exact", async () => {
  const options = isolatedOptions();
  const first = new ManagedOAuthProvider({
    ...options,
    serverUrl: "https://SERVICE.test:443/a/../mcp",
  });
  await first.saveTokens({
    access_token: "token",
    token_type: "Bearer",
    issuer: "https://auth.test",
    expires_in: 3600,
  });
  const equivalent = new ManagedOAuthProvider(options);
  expect((await equivalent.tokens())?.access_token).toBe("token");
  await expect(
    equivalent.validateResourceURL(
      options.serverUrl,
      `${options.serverUrl}?tenant=other`,
    ),
  ).rejects.toMatchObject({ code: "mcp_auth_required" });
  await expect(
    equivalent.fetch(`${options.serverUrl}?tenant=other`),
  ).rejects.toMatchObject({ code: "mcp_auth_required" });
});

test("provider snapshots configuration and valid revocation failure still clears credentials", async () => {
  const options = isolatedOptions();
  let requests = 0;
  let revocations = 0;
  const provider = new ManagedOAuthProvider({
    ...options,
    fetch: async () => {
      requests++;
      return new Response("ok");
    },
    revoke: async (tokens) => {
      expect(tokens.access_token).toBe("token");
      revocations++;
      throw new Error("remote unavailable");
    },
  });
  await provider.saveTokens({
    access_token: "token",
    token_type: "Bearer",
    issuer: "https://auth.test",
    expires_in: 3600,
  });
  options.clientInformation.client_id = "mutated";
  options.allowedOrigins.splice(0);
  const exposed = (await provider.clientInformation())!;
  exposed.client_id = "also-mutated";
  expect(Object.isFrozen(provider.clientMetadata.redirect_uris)).toBe(true);
  await provider.fetch("https://service.test/mcp");
  expect(requests).toBe(1);
  await expect(provider.disconnect()).rejects.toThrow("remote unavailable");
  expect(revocations).toBe(1);
  expect(await options.secrets.listRefs()).toHaveLength(0);
});

test("changed resource disconnect discards bound tokens without invoking the new revocation callback", async () => {
  const options = isolatedOptions();
  const first = new ManagedOAuthProvider(options);
  await first.saveTokens({
    access_token: "old-token",
    token_type: "Bearer",
    issuer: "https://auth.test",
  });
  let revocations = 0;
  const changed = new ManagedOAuthProvider({
    ...options,
    serverUrl: "https://other.test/mcp",
    allowedOrigins: ["https://other.test"],
    revoke: async () => {
      revocations++;
    },
  });
  await changed.disconnect();
  expect(revocations).toBe(0);
  expect(await options.secrets.listRefs()).toHaveLength(0);
});

test("explicit credential invalidation permits fresh authorization after binding drift", async () => {
  const options = isolatedOptions();
  const first = new ManagedOAuthProvider(options);
  await first.saveTokens({
    access_token: "old-token",
    token_type: "Bearer",
    issuer: "https://auth.test",
  });
  const changed = new ManagedOAuthProvider({
    ...options,
    clientInformation: { client_id: "new-client", issuer: "https://auth.test" },
  });
  await expect(changed.tokens()).rejects.toMatchObject({
    code: "mcp_auth_required",
  });
  await changed.invalidateCredentials("all");
  await changed.saveTokens({
    access_token: "new-token",
    token_type: "Bearer",
    issuer: "https://auth.test",
  });
  expect((await changed.tokens())?.access_token).toBe("new-token");
  await expect(first.tokens()).rejects.toMatchObject({
    code: "mcp_auth_required",
  });
});

test("registered issuer mismatch cannot enter the cached-token path", async () => {
  const provider = new ManagedOAuthProvider(isolatedOptions());
  await expect(
    provider.saveTokens({
      access_token: "wrong-token",
      token_type: "Bearer",
      issuer: "https://other.test",
    }),
  ).rejects.toMatchObject({ code: "mcp_auth_required" });
  expect(await provider.tokens()).toBeUndefined();
});
