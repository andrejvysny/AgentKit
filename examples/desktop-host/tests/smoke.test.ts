/**
 * End-to-end smoke test for `examples/desktop-host`, over REAL HTTP.
 *
 * This does not call `main.ts` (which binds a real port from env and installs
 * signal handlers) — it calls the same `buildApp` composition main.ts calls,
 * with a scripted provider and a temp-dir sqlite file standing in for the
 * network and the real database, then serves the result on an ephemeral port
 * exactly the way main.ts serves it on a real one. That is what proves the
 * example wiring — not a mock of it — actually answers HTTP requests.
 *
 * Two scenarios: a plain text turn end to end, and one round trip through the
 * `example_echo` tool this example ships (see `../src/tools.ts`), scripted the
 * way `packages/runner-local/tests/e2e-vertical-slice.test.ts` scripts a tool
 * call for `MockProviderClient`.
 */
import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type {
  ChatDto,
  MessagePageDto,
  SubmitMessageResponse,
  ToolEventDto,
} from "@agentkit/contracts";
import { MockProviderClient } from "@agentkit/testing";
import { serveRest } from "@agentkit/transport-http";
import { buildApp, type App } from "../src/wiring.js";

interface Booted {
  origin: string;
  app: App;
}

interface Cleanup {
  app: App;
  server: ReturnType<typeof Bun.serve>;
  dir: string;
}

const cleanups: Cleanup[] = [];

afterEach(async () => {
  let entry: Cleanup | undefined;
  // biome-ignore lint/suspicious/noAssignInExpressions: tidiest drain of a stack in afterEach.
  while ((entry = cleanups.pop())) {
    await entry.server.stop(true);
    await entry.app.stop();
    rmSync(entry.dir, { recursive: true, force: true });
  }
});

/** Boot the example's real wiring, scripted provider in, real socket out. */
async function boot(provider: MockProviderClient): Promise<Booted> {
  const dir = mkdtempSync(join(tmpdir(), "agentkit-example-smoke-"));
  const app = await buildApp({
    dbPath: join(dir, "agentkit.sqlite"),
    providerFactory: () => provider,
    env: {},
  });
  const server = Bun.serve({ port: 0, ...serveRest(app.deps) });
  cleanups.push({ app, server, dir });
  return {
    origin: `http://localhost:${server.port}${app.deps.basePath ?? ""}`,
    app,
  };
}

interface Frame {
  id: string;
  event: string;
  data: string;
}

/** Read an SSE body to its close, returning its event frames (see transport-http's e2e.test.ts). */
async function readFrames(res: Response): Promise<Frame[]> {
  expect(res.headers.get("content-type")).toBe("text/event-stream");
  const body = res.body;
  if (body === null) throw new Error("SSE response had no body.");
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const frames: Frame[] = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    let split = buffer.indexOf("\n\n");
    while (split !== -1) {
      const block = buffer.slice(0, split);
      buffer = buffer.slice(split + 2);
      const frame = parseFrame(block);
      if (frame !== null) frames.push(frame);
      split = buffer.indexOf("\n\n");
    }
  }
  return frames;
}

function parseFrame(block: string): Frame | null {
  let id: string | undefined;
  let event: string | undefined;
  let data: string | undefined;
  for (const line of block.split("\n")) {
    if (line.startsWith("id: ")) id = line.slice(4);
    else if (line.startsWith("event: ")) event = line.slice(7);
    else if (line.startsWith("data: ")) data = line.slice(6);
  }
  if (id === undefined || event === undefined || data === undefined)
    return null;
  return { id, event, data };
}

async function postJson(
  url: string,
  body: unknown,
  headers: Record<string, string> = {},
) {
  return fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
}

describe("examples/desktop-host — HTTP smoke", () => {
  it("carries a plain turn from submit to a streamed, persisted answer", async () => {
    const provider = new MockProviderClient();
    provider.setScript([
      { steps: [{ kind: "text", content: "Hello from the desktop example." }] },
    ]);
    const { origin } = await boot(provider);

    // GET /v1/version needs no provider, no chat, nothing seeded — the
    // manual-boot check in the task's VERIFY step relies on the same route.
    const version = await fetch(`${origin}/v1/version`);
    expect(version.status).toBe(200);

    const created = await postJson(`${origin}/v1/chats`, {});
    expect(created.status).toBe(201);
    const chat = (await created.json()) as ChatDto;

    const submitted = await postJson(
      `${origin}/v1/chats/${chat.id}/messages`,
      { content: "Say hello" },
      { "idempotency-key": "smoke-text-1" },
    );
    expect(submitted.status).toBe(201);
    const turn = (await submitted.json()) as SubmitMessageResponse;
    expect(turn.chatId).toBe(chat.id);

    // Replaying the SAME idempotency key must return the identical turn, 200
    // this time — the one write in the API that creates three records at once.
    const replay = await postJson(
      `${origin}/v1/chats/${chat.id}/messages`,
      { content: "Say hello" },
      { "idempotency-key": "smoke-text-1" },
    );
    expect(replay.status).toBe(200);
    expect(await replay.json()).toEqual(turn);

    const frames = await readFrames(
      await fetch(`${origin}/v1/runs/${turn.runId}/stream`),
    );
    expect(frames[0]?.event).toBe("run.started");
    expect(frames[frames.length - 1]?.event).toBe("run.completed");

    const page = (await (
      await fetch(`${origin}/v1/chats/${chat.id}/messages`)
    ).json()) as MessagePageDto;
    const assistant = page.items.find((m) => m.id === turn.assistantMessageId);
    expect(assistant?.content).toBe("Hello from the desktop example.");
    expect(page.items[0]?.role).toBe("user");
  });

  it("round-trips the example_echo tool this example contributes", async () => {
    const provider = new MockProviderClient();
    provider.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "call-echo-1",
            name: "example_echo",
            argumentsJson: JSON.stringify({ text: "ping" }),
          },
        ],
      },
      { steps: [{ kind: "text", content: "The tool echoed: ping" }] },
    ]);
    const { origin } = await boot(provider);

    const created = await postJson(`${origin}/v1/chats`, {});
    const chat = (await created.json()) as ChatDto;

    const submitted = await postJson(
      `${origin}/v1/chats/${chat.id}/messages`,
      { content: "call example_echo with ping" },
      { "idempotency-key": "smoke-tool-1" },
    );
    expect(submitted.status).toBe(201);
    const turn = (await submitted.json()) as SubmitMessageResponse;

    const frames = await readFrames(
      await fetch(`${origin}/v1/runs/${turn.runId}/stream`),
    );
    expect(frames[frames.length - 1]?.event).toBe("run.completed");

    const toolEvents = (await (
      await fetch(`${origin}/v1/chats/${chat.id}/tool-events`)
    ).json()) as ToolEventDto[];
    const succeeded = toolEvents.find(
      (e) => e.toolName === "example_echo" && e.status === "succeeded",
    );
    expect(succeeded).toBeDefined();
    expect(succeeded?.resultJson).toContain("ping");

    const page = (await (
      await fetch(`${origin}/v1/chats/${chat.id}/messages`)
    ).json()) as MessagePageDto;
    const assistant = page.items.find((m) => m.id === turn.assistantMessageId);
    expect(assistant?.content).toBe("The tool echoed: ping");
  });
});
