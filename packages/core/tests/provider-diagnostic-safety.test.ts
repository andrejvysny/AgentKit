import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import { providerDiagnostic } from "../src/providers/diagnostics.js";
import { OpenAiCompatibleClient } from "../src/providers/openai-compatible.js";

function fetchFor(handler: () => Promise<Response>): typeof fetch {
  return Object.assign(handler, { preconnect() {} });
}

const apiKey = "sk-configured-secret";
const leaked = `${apiKey} Authorization: Bearer echoed-token {"api_key":"json-secret","password":"private-password"}`;

async function diagnostics(
  fetchImpl: typeof fetch,
  signal?: AbortSignal,
): Promise<AiRunEvent[]> {
  const client = new OpenAiCompatibleClient({
    id: "test",
    kind: "openai-compatible",
    baseUrl: "https://test.invalid",
    apiKey,
    fetchImpl,
  });
  return Array.fromAsync(
    client.streamChat({ runId: "run", model: "model", messages: [], signal }),
  );
}

function assertSafe(event: AiRunEvent | undefined): void {
  const serialized = JSON.stringify(event);
  for (const secret of [
    apiKey,
    "echoed-token",
    "json-secret",
    "private-password",
  ])
    expect(serialized).not.toContain(secret);
  expect(serialized).toContain("[REDACTED]");
}

describe("provider public diagnostics", () => {
  it("redacts echoed credentials in HTTP errors without changing status codes", async () => {
    const events = await diagnostics(
      (async () =>
        new Response(leaked, { status: 401 })) as unknown as typeof fetch,
    );
    const failed = events.find((event) => event.type === "run.failed");
    assertSafe(failed);
    expect(failed?.type === "run.failed" && failed.data.errorCode).toBe("401");
  });

  it("redacts thrown network errors without changing their codes", async () => {
    const events = await diagnostics(
      fetchFor(async () => {
        throw new Error(leaked);
      }),
    );
    const failed = events.find((event) => event.type === "run.failed");
    assertSafe(failed);
    expect(failed?.type === "run.failed" && failed.data.errorCode).toBe(
      "network_error",
    );
  });

  it("redacts cancellation reasons", async () => {
    const controller = new AbortController();
    const events = await diagnostics(
      fetchFor(async () => {
        controller.abort();
        throw new Error(leaked);
      }),
      controller.signal,
    );
    assertSafe(events.find((event) => event.type === "run.cancelled"));
  });

  it("caps UTF-8 diagnostics after redaction", async () => {
    const events = await diagnostics(
      fetchFor(
        async () =>
          new Response(`${leaked} ${"🙂".repeat(3000)}`, {
            status: 500,
          }),
      ),
    );
    const failed = events.find((event) => event.type === "run.failed");
    assertSafe(failed);
    if (failed?.type !== "run.failed")
      throw new Error("Expected provider failure");
    expect(
      new TextEncoder().encode(failed.data.errorMessage).byteLength,
    ).toBeLessThanOrEqual(4096);
    expect(failed.data.errorMessage).not.toContain("�");
  });
});

describe("diagnostic credential formats", () => {
  it("redacts escaped JSON credentials and exact configured header values", () => {
    const secret = 'key"with\\escape';
    const message = providerDiagnostic(
      `${JSON.stringify({ apiKey: secret })} header-secret`,
      [secret, "header-secret"],
    );
    expect(message).not.toContain("header-secret");
    expect(message).not.toContain("with");
    expect(message).toContain("[REDACTED]");
  });
});
