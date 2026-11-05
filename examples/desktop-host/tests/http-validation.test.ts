/**
 * HTTP validation parity at the FINAL application boundary (external review,
 * section 3): the example's mounted façade must refuse malformed traffic with
 * the same codes the private Rust REST engine refuses it.
 *
 * The façade used to parse `POST /v1/chats` bodies itself with a lenient TS
 * reader (malformed JSON, array bodies, mistyped titles, and oversized bodies
 * all reached `createSession` and returned 201, while the direct native
 * server returned 400/413). It now forwards the raw bytes to the native
 * server, which owns the parse: these tests boot the SAME composition
 * `main.ts` serves (`buildApp` + the façade on `Bun.serve`) and post each
 * malformed input through both doors — asserting the mounted façade and the
 * direct native server answer identically, and that valid traffic is
 * unaffected.
 */
import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { MockProviderClient } from "@agentkit/testing";
import { buildApp, type App } from "../src/wiring.js";

const TOKEN = "12345678901234567890123456789012";
const AUTH = { authorization: `Bearer ${TOKEN}` };

const cleanups: {
  app?: App;
  server?: ReturnType<typeof Bun.serve>;
  dir: string;
}[] = [];

afterEach(async () => {
  let entry: (typeof cleanups)[number] | undefined;
  // biome-ignore lint/suspicious/noAssignInExpressions: tidiest drain of a stack in afterEach.
  while ((entry = cleanups.pop())) {
    await entry.server?.stop(true);
    await entry.app?.stop();
    rmSync(entry.dir, { recursive: true, force: true });
  }
});

/** Boot the example's real wiring — the composition `main.ts` serves. */
async function boot(provider: MockProviderClient) {
  const dir = mkdtempSync(join(tmpdir(), "agentkit-example-http-validation-"));
  const cleanup: (typeof cleanups)[number] = { dir };
  cleanups.push(cleanup);
  const app = await buildApp({
    dbPath: join(dir, "agentkit.sqlite"),
    providerFactory: () => provider,
    env: {},
    httpBearerToken: TOKEN,
  });
  cleanup.app = app;
  const server = Bun.serve({ port: 0, fetch: app.fetch });
  cleanup.server = server;
  return { origin: `http://localhost:${server.port}${app.basePath}`, app };
}

async function postStatus(
  url: string,
  body: string,
  headers: Record<string, string> = {},
) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body,
  });
  return { status: response.status, text: await response.text() };
}

/** Read an SSE body to its close, returning the event names in order. */
async function readEvents(res: Response): Promise<string[]> {
  const body = res.body;
  if (body === null) throw new Error("SSE response had no body.");
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const events: string[] = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let split = buffer.indexOf("\n\n");
    while (split !== -1) {
      for (const line of buffer.slice(0, split).split("\n"))
        if (line.startsWith("event: ")) events.push(line.slice(7));
      buffer = buffer.slice(split + 2);
      split = buffer.indexOf("\n\n");
    }
  }
  return events;
}

const OVERSIZED = JSON.stringify({ title: "x", junk: "a".repeat(1_100_000) });

const refusals = [
  { label: "malformed JSON", body: "{oops", code: 400 },
  { label: "array body", body: "[1,2]", code: 400 },
  { label: "non-string title", body: '{"title":123}', code: 400 },
  { label: "array title", body: '{"title":["x"]}', code: 400 },
  { label: "body above max_body_bytes", body: OVERSIZED, code: 413 },
];

describe("examples/desktop-host — façade validation parity with the native server", () => {
  it("refuses every malformed POST /v1/chats body with the direct native server's code", async () => {
    const { origin, app } = await boot(new MockProviderClient());
    for (const refusal of refusals) {
      // The mounted façade and the private Rust server it fronts must not
      // disagree on what is a valid request.
      const mounted = await postStatus(`${origin}/v1/chats`, refusal.body);
      const direct = await postStatus(
        `${app.http.baseUrl}/v1/chats`,
        refusal.body,
        AUTH,
      );
      expect(direct.status, `direct native: ${refusal.label}`).toBe(
        refusal.code,
      );
      expect(mounted.status, `façade: ${refusal.label}`).toBe(refusal.code);
      // The refusal envelope is the Rust engine's problem+json — the façade
      // mirrors it verbatim rather than minting its own answer.
      expect(mounted.text, `façade envelope for ${refusal.label}`).toContain(
        "agentkit.dev/problems",
      );
    }
  });

  it("creates a session through one native request without temporary chat writes", async () => {
    const { app } = await boot(new MockProviderClient());
    const original = globalThis.fetch;
    const mutations: string[] = [];
    globalThis.fetch = Object.assign(
      (...args: Parameters<typeof fetch>): ReturnType<typeof fetch> => {
        const method = args[1]?.method ?? "GET";
        if (method === "POST" || method === "DELETE") mutations.push(method);
        return original(...args);
      },
      original,
    );
    try {
      const response = await app.fetch(
        new Request(`http://localhost${app.basePath}/v1/chats`, {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ title: "single transaction" }),
        }),
      );
      expect(response.status).toBe(201);
      expect(mutations).toEqual(["POST"]);
      const chat = (await response.json()) as {
        metadata: Record<string, unknown>;
      };
      expect(chat.metadata["agentkit.session"]).toBeDefined();
    } finally {
      globalThis.fetch = original;
    }
  });

  it("creates valid sessions through the façade without extra rows", async () => {
    const { origin, app } = await boot(new MockProviderClient());
    const valid = await postStatus(
      `${origin}/v1/chats`,
      JSON.stringify({ title: "keep me" }),
    );
    expect(valid.status).toBe(201);
    const chat = JSON.parse(valid.text) as { id: string; title?: string };
    expect(chat.title).toBe("keep me");
    // The direct native server agrees the shape is valid — parity covers
    // accepted shapes, not just refusals.
    const direct = await postStatus(
      `${app.http.baseUrl}/v1/chats`,
      '{"title":"ok"}',
      AUTH,
    );
    expect(direct.status).toBe(201);
    const empty = await postStatus(`${origin}/v1/chats`, '{"title":""}');
    expect(empty.status).toBe(201);
    // Rejected requests never create rows; each valid request creates one
    // session, including requests through the private native listener.
    for (const refusal of refusals)
      await postStatus(`${origin}/v1/chats`, refusal.body);
    const list = await fetch(`${origin}/v1/chats`);
    expect(list.status).toBe(200);
    const chats = (await list.json()) as { id: string }[];
    expect(chats.map((entry) => entry.id).sort()).toEqual(
      [
        chat.id,
        (JSON.parse(empty.text) as { id: string }).id,
        (JSON.parse(direct.text) as { id: string }).id,
      ].sort(),
    );
  });

  it("valid traffic still runs end to end through the native session façade", async () => {
    const provider = new MockProviderClient();
    provider.setScript([
      { steps: [{ kind: "text", content: "adjudicated and running" }] },
    ]);
    const { origin } = await boot(provider);
    const created = await postStatus(
      `${origin}/v1/chats`,
      JSON.stringify({ title: "run me" }),
    );
    expect(created.status).toBe(201);
    const { id } = JSON.parse(created.text) as { id: string };
    const submitted = await fetch(`${origin}/v1/chats/${id}/messages`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "idempotency-key": "facade-run-1",
      },
      body: JSON.stringify({ content: "hello", model: "mock" }),
    });
    expect(submitted.status).toBe(201);
    const turn = (await submitted.json()) as {
      runId: string;
      assistantMessageId: string;
    };
    const events = await readEvents(
      await fetch(`${origin}/v1/runs/${turn.runId}/stream`),
    );
    expect(events).toContain("run.completed");
    expect(events[events.length - 1]).toBe("agentkit.stream.settled");
    const page = (await (
      await fetch(`${origin}/v1/chats/${id}/messages`)
    ).json()) as { items: { id: string; content: string }[] };
    expect(
      page.items.find((entry) => entry.id === turn.assistantMessageId)?.content,
    ).toBe("adjudicated and running");
  });
});
