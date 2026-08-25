/**
 * The adapter over a real socket: `Bun.serve` on an ephemeral port, the whole
 * stack behind it, only the model faked.
 *
 * Everything else in this package calls the handler as a function, which is the
 * right way to test routing but proves nothing about the parts a runtime owns —
 * that a `ReadableStream` body actually flushes frame by frame instead of
 * buffering to the end, that `Last-Event-ID` survives the header round-trip,
 * that an SSE response is readable while the run is still writing it.
 */
import { afterAll, beforeAll, describe, expect, it } from "bun:test";
import type {
  MessagePageDto,
  SubmitMessageResponse,
} from "@agentkit/contracts";
import { serveRest } from "../src/index.js";
import {
  createLiveFixture,
  waitFor,
  type LiveFixture,
} from "./support/fixture.js";

let live: LiveFixture;
let server: ReturnType<typeof Bun.serve>;
let origin: string;

beforeAll(async () => {
  live = await createLiveFixture();
  server = Bun.serve({ port: 0, ...serveRest(live.deps) });
  origin = `http://localhost:${server.port}`;
});

afterAll(async () => {
  await live.stop();
  await server.stop(true);
});

interface Frame {
  id: string;
  event: string;
  data: string;
}

/**
 * Read an SSE body until the stream closes, returning its event frames.
 *
 * Comments (`: hb`) and the `retry:` hint are dropped — this helper answers
 * "what events did the client see, in what order", which is the only thing the
 * assertions below care about.
 */
async function readFrames(res: Response): Promise<Frame[]> {
  expect(res.headers.get("content-type")).toBe("text/event-stream");
  expect(res.headers.get("cache-control")).toBe("no-cache, no-transform");
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

describe("REST v1 over Bun.serve", () => {
  it("(a) carries a turn from submit to a streamed answer, then resumes", async () => {
    const created = await fetch(`${origin}/v1/chats`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ title: "e2e" }),
    });
    expect(created.status).toBe(201);
    const chatId = ((await created.json()) as { id: string }).id;

    const submitted = await fetch(`${origin}/v1/chats/${chatId}/messages`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "idempotency-key": "e2e-key-1",
      },
      body: JSON.stringify({ content: "Say hello" }),
    });
    expect(submitted.status).toBe(201);
    const turn = (await submitted.json()) as SubmitMessageResponse;
    expect(turn.chatId).toBe(chatId);

    // Opened while the run is still in flight: the stream replays what exists
    // and then follows the log to the terminal event, which closes it.
    const frames = await readFrames(
      await fetch(`${origin}/v1/runs/${turn.runId}/stream`),
    );
    expect(frames.length).toBeGreaterThan(1);
    expect(frames[0]?.event).toBe("run.started");
    expect(frames[frames.length - 1]?.event).toBe("run.completed");

    // One unbroken sequence, and the SSE id is the event's own id.
    const events = frames.map(
      (frame) => JSON.parse(frame.data) as { seq: number; eventId: string },
    );
    expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => i));
    expect(events.map((e) => e.eventId)).toEqual(frames.map((f) => f.id));

    // The answer landed in the placeholder the submit transaction wrote.
    await waitFor(
      async () =>
        (await live.store.tasks.getTask(turn.runId))?.status === "completed",
      "the run to settle",
    );
    const page = (await (
      await fetch(`${origin}/v1/chats/${chatId}/messages`)
    ).json()) as MessagePageDto;
    const assistant = page.items.find((m) => m.id === turn.assistantMessageId);
    expect(assistant?.content).toBe("Hello from the mock.");
    expect(page.items[0]?.role).toBe("user");

    // Resume from the middle: the tail, and nothing the client already had.
    const midpoint = frames[1];
    expect(midpoint).toBeDefined();
    const resumed = await readFrames(
      await fetch(`${origin}/v1/runs/${turn.runId}/stream`, {
        headers: { "last-event-id": midpoint?.id ?? "" },
      }),
    );
    expect(resumed.map((f) => f.id)).toEqual(frames.slice(2).map((f) => f.id));
  });

  it("(b) 404s a stream for a run that does not exist, as problem+json", async () => {
    const res = await fetch(`${origin}/v1/runs/nope/stream`);
    expect(res.status).toBe(404);
    expect(res.headers.get("content-type")).toBe("application/problem+json");
    expect(((await res.json()) as { code: string }).code).toBe("not_found");
  });
});
