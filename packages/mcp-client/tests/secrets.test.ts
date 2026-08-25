import { describe, expect, it } from "bun:test";
import {
  applySecretsToTransport,
  McpClientManager,
  McpError,
  McpSession,
  resolveMcpSecrets,
  type McpSecretMaterial,
  type McpServerConfig,
} from "../src/index.js";
import {
  buildFakeServer,
  createSecretStore,
  createTestClock,
  InMemoryHarness,
} from "./helpers.js";

const TOKEN = "ghp_supersecrettoken";

/**
 * Every string REACHABLE from a thrown value — message, name, stack, the
 * `details` bag, and the whole `cause` chain.
 *
 * Checking `err.message` alone is not the assertion that matters: hosts log
 * `err.cause`, and a structured logger serializes the object graph wholesale.
 * A secret one hop off the message is a leaked secret.
 */
function stringsReachableFrom(
  value: unknown,
  seen: Set<unknown> = new Set(),
): string[] {
  if (typeof value === "string") return [value];
  if (value === null || typeof value !== "object") return [];
  if (seen.has(value)) return [];
  seen.add(value);
  const found: string[] = [];
  if (value instanceof Error) {
    found.push(value.name, value.message, value.stack ?? "");
    // `cause` is non-enumerable when set through the options bag, so
    // Object.values below would walk straight past it.
    found.push(
      ...stringsReachableFrom((value as { cause?: unknown }).cause, seen),
    );
  }
  for (const nested of Object.values(value as Record<string, unknown>)) {
    found.push(...stringsReachableFrom(nested, seen));
  }
  return found;
}

describe("secret resolution", () => {
  it("substitutes placeholders into http headers", async () => {
    const store = createSecretStore({ "gh.token": TOKEN });
    const material = await resolveMcpSecrets(
      "gh",
      { GH_TOKEN: "gh.token" },
      store,
    );
    const resolved = applySecretsToTransport(
      {
        kind: "http",
        url: "https://example.test/mcp",
        headers: { Authorization: "Bearer ${GH_TOKEN}" },
      },
      material,
    );
    expect(resolved).toEqual({
      kind: "http",
      url: "https://example.test/mcp",
      headers: { Authorization: `Bearer ${TOKEN}` },
    });
  });

  it("substitutes placeholders into stdio env values", async () => {
    const store = createSecretStore({ "gh.token": TOKEN });
    const material = await resolveMcpSecrets(
      "gh",
      { GH_TOKEN: "gh.token" },
      store,
    );
    const resolved = applySecretsToTransport(
      { kind: "stdio", command: "gh-mcp", env: { TOKEN: "${GH_TOKEN}" } },
      material,
    );
    expect(resolved).toEqual({
      kind: "stdio",
      command: "gh-mcp",
      env: { TOKEN },
    });
  });

  it("fails with mcp_secret_missing when the store has no value", async () => {
    const store = createSecretStore();
    const failure = await resolveMcpSecrets(
      "gh",
      { GH_TOKEN: "gh.token" },
      store,
    )
      .then(() => null)
      .catch((err: unknown) => err);
    expect(failure).toBeInstanceOf(McpError);
    expect((failure as McpError).code).toBe("mcp_secret_missing");
    expect((failure as McpError).retryable).toBe(false);
    // The ref and the placeholder are safe to name; they are what an operator fixes.
    expect((failure as McpError).message).toContain("gh.token");
    expect((failure as McpError).message).toContain("GH_TOKEN");
  });

  it("redacts resolved values out of anything derived from the config", async () => {
    const store = createSecretStore({ "gh.token": TOKEN });
    const material = await resolveMcpSecrets(
      "gh",
      { GH_TOKEN: "gh.token" },
      store,
    );
    const echoed = material.substitute("Authorization: Bearer ${GH_TOKEN}");
    expect(material.redact(echoed)).toBe("Authorization: Bearer ***");
    expect(material.redact(echoed)).not.toContain(TOKEN);
  });
});

describe("secrets on the connect path", () => {
  const config = (): McpServerConfig => ({
    alias: "gh",
    transport: {
      kind: "http",
      url: "https://example.test/mcp",
      headers: { Authorization: "Bearer ${GH_TOKEN}" },
    },
    secretRefs: { GH_TOKEN: "gh.token" },
    resilience: { maxConnectAttempts: 1, connectBackoffBaseMs: 1 },
  });

  it("hands the transport factory a config with the secret injected", async () => {
    const seen: Record<string, string>[] = [];
    const harness = new InMemoryHarness({
      gh: () => buildFakeServer("gh", [{ name: "ping" }]),
    });
    const manager = new McpClientManager(
      {
        secrets: createSecretStore({ "gh.token": TOKEN }),
        clock: createTestClock(),
        transportFactory: async (request) => {
          if (request.transport.kind === "http") {
            seen.push(request.transport.headers ?? {});
          }
          return harness.factory(request);
        },
      },
      [config()],
    );
    await manager.connect("gh");
    expect(seen).toEqual([{ Authorization: `Bearer ${TOKEN}` }]);
    await manager.dispose();
    await harness.closeAll();
  });

  it("never leaks the secret value into the connect error", async () => {
    const harness = new InMemoryHarness({
      gh: () => {
        // The failure text deliberately carries the substituted header, the way
        // a real fetch/spawn error carries the URL or the command line.
        throw new Error(`connect refused (Authorization: Bearer ${TOKEN})`);
      },
    });
    const manager = new McpClientManager(
      {
        secrets: createSecretStore({ "gh.token": TOKEN }),
        clock: createTestClock(),
        transportFactory: harness.factory,
      },
      [config()],
    );
    const failure = await manager
      .connect("gh")
      .then(() => null)
      .catch((err: unknown) => err);
    expect(failure).toBeInstanceOf(McpError);
    expect((failure as McpError).code).toBe("mcp_connect_failed");
    expect((failure as McpError).message).not.toContain(TOKEN);
    expect((failure as McpError).message).toContain("***");
    // Not just the message: the raw driver error used to ride along as `cause`,
    // carrying the header text the message had been redacted of.
    const reachable = stringsReachableFrom(failure);
    expect(reachable.filter((text) => text.includes(TOKEN))).toEqual([]);
    expect(reachable.some((text) => text.includes("***"))).toBe(true);
    await manager.dispose();
  });

  it("drops the resolved secret material when the session closes", async () => {
    const harness = new InMemoryHarness({
      gh: () => buildFakeServer("gh", [{ name: "ping" }]),
    });
    const session = new McpSession(config(), {
      secrets: createSecretStore({ "gh.token": TOKEN }),
      clock: createTestClock(),
      transportFactory: harness.factory,
    });
    // White-box on purpose: memory hygiene has no behavioural surface to assert
    // through, and `redact` is the one operation that reveals whether the
    // resolved VALUES are still held.
    const material = (): McpSecretMaterial =>
      (session as unknown as { material: McpSecretMaterial }).material;

    await session.connect();
    expect([...material().placeholders]).toEqual(["GH_TOKEN"]);
    expect(material().redact(`Bearer ${TOKEN}`)).toBe("Bearer ***");

    await session.close();
    // Nothing left to redact WITH: the token is no longer in memory here.
    expect([...material().placeholders]).toEqual([]);
    expect(material().redact(`Bearer ${TOKEN}`)).toBe(`Bearer ${TOKEN}`);
    await harness.closeAll();
  });
});
