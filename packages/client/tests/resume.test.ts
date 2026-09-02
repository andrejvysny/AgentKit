/**
 * The resume path: a stream that is severed mid-run must come back with
 * `Last-Event-ID` and deliver every event exactly once.
 *
 * HOW THE DISCONNECT IS FORCED. The client takes its `fetch` as an option, so
 * the test supplies one that calls the real `fetch`, then re-wraps the response
 * body in a `ReadableStream` that forwards a fixed number of complete SSE frames
 * and then `controller.error(...)`s. That is what a reset connection looks like
 * to `fetch`: bytes, then a rejected read — not a clean end-of-body, which the
 * client is required to take at face value as the server saying it is done.
 *
 * The error is raised on the pull AFTER the budget is spent rather than in the
 * same one, and that ordering is load-bearing: `controller.error()` discards
 * whatever is still queued, so erroring immediately after an `enqueue` would
 * throw away the very chunk that carried the last event the client saw, and the
 * test would then be asserting about a resume from an id the client never got.
 *
 * WHAT IS ASSERTED is the property a UI depends on and cannot check for itself:
 * the events the caller received are the run's whole log, in `seq` order, with
 * no gap and no duplicate across the seam.
 */
import { afterEach, describe, expect, test } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  createTestEventStamper,
  MockProviderClient,
  nowIso,
} from "@agentkit/testing";
import { createAgentKitClient, type FetchLike } from "../src/index.js";
import {
  chattyProvider,
  startTestServer,
  TEST_CHAT_ID,
  waitFor,
  type TestServer,
} from "./support/server.js";

let server: TestServer | null = null;

afterEach(async () => {
  await server?.stop();
  server = null;
});

interface Severing {
  fetch: FetchLike;
  /** `last-event-id` sent on each stream request; `null` where none was. */
  readonly resumeHeaders: (string | null)[];
  readonly cuts: () => number;
}

/**
 * A `fetch` that severs the first `cutsWanted` stream bodies after exactly
 * `framesPerConnection` SSE frames.
 *
 * Counted in FRAMES, not bytes, and the chunk is split at the boundary rather
 * than forwarded whole. Byte counting was the obvious version and it is not
 * deterministic: HTTP is free to coalesce a whole run's frames into one chunk,
 * and a proxy that can only cut between chunks then delivers the entire log on
 * the first connection and never resumes at all. Splitting at `\n\n` makes each
 * connection carry a known number of frames, so "twelve severed connections" is
 * a fact about the test rather than about the day's chunking.
 */
function severingFetch(framesPerConnection: number, cutsWanted = 1): Severing {
  const resumeHeaders: (string | null)[] = [];
  let cuts = 0;

  const wrapped: FetchLike = async (url, init) => {
    const isStream = url.includes("/stream");
    if (isStream) {
      const headers = new Headers(init?.headers ?? {});
      resumeHeaders.push(headers.get("last-event-id"));
    }

    const response = await fetch(url, init);
    if (!isStream || cuts >= cutsWanted || response.body === null) {
      return response;
    }
    cuts += 1;

    const upstream = response.body.getReader();
    let forwarded = 0;
    // Set the moment the budget is spent; the ERROR is raised on the next pull,
    // so whatever was enqueued has already reached the consumer. Erroring in
    // the same turn would discard the queue and take the last event with it.
    let spent = framesPerConnection === 0;
    let carry = new Uint8Array(0);

    const body = new ReadableStream<Uint8Array>({
      async pull(controller) {
        if (spent) {
          await upstream.cancel().catch(() => undefined);
          controller.error(new Error("connection reset by the test proxy"));
          return;
        }
        const chunk = await upstream.read();
        if (chunk.done) {
          if (carry.length > 0) controller.enqueue(carry);
          controller.close();
          return;
        }
        const buffer = concat(carry, chunk.value);
        carry = new Uint8Array(0);

        let cut = -1;
        for (let i = 0; i + 1 < buffer.length; i += 1) {
          if (buffer[i] !== 0x0a || buffer[i + 1] !== 0x0a) continue;
          forwarded += 1;
          if (forwarded >= framesPerConnection) {
            cut = i + 2;
            break;
          }
          i += 1;
        }
        if (cut === -1) {
          controller.enqueue(buffer);
          return;
        }
        spent = true;
        controller.enqueue(buffer.slice(0, cut));
      },
      cancel() {
        void upstream.cancel().catch(() => undefined);
      },
    });
    return new Response(body, {
      status: response.status,
      headers: response.headers,
    });
  };

  return { fetch: wrapped, resumeHeaders, cuts: () => cuts };
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (a.length === 0) return b;
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

/**
 * A provider that streams one delta and then PARKS until the run is cancelled.
 *
 * A `MockProviderClient` subclass rather than `@agentkit/testing`'s
 * `HangingProviderClient` only because the fixture here is typed to the mock —
 * the behaviour is the same, and it is what makes "the run was demonstrably
 * still live" a fact of the test rather than a race against a fast provider.
 */
class ParkingProvider extends MockProviderClient {
  private announce!: () => void;
  /** Resolves the first time the stream parks. */
  readonly parked: Promise<void> = new Promise<void>((resolve) => {
    this.announce = resolve;
  });

  override async *streamChat(
    input: Parameters<MockProviderClient["streamChat"]>[0],
  ): AsyncIterable<AiRunEvent> {
    const stamp = createTestEventStamper();
    yield stamp({
      type: "run.started",
      runId: input.runId,
      timestamp: nowIso(),
      data: { model: input.model, toolCount: 0 },
    });
    yield stamp({
      type: "run.message.delta",
      runId: input.runId,
      timestamp: nowIso(),
      data: { delta: "one" },
    });
    this.announce();
    await new Promise<void>((resolve) => {
      const signal = input.signal;
      if (signal === undefined || signal.aborted) {
        resolve();
        return;
      }
      signal.addEventListener("abort", () => resolve(), { once: true });
    });
    const aborted = new Error("The operation was aborted.");
    aborted.name = "AbortError";
    throw aborted;
  }
}

/** The run's whole durable log, straight out of the store. */
async function logOf(runId: string): Promise<AiRunEvent[]> {
  const events = await server!.store.tasks.listEvents(runId);
  return [...events].sort((a, b) => a.seq - b.seq) as AiRunEvent[];
}

describe("streamRun resumes across a severed connection", () => {
  test("every event arrives exactly once, seq-contiguous", async () => {
    server = await startTestServer({ provider: chattyProvider(60) });
    // The `retry:` hint plus four events, then the connection dies.
    const severing = severingFetch(5);
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: severing.fetch,
    });

    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "stream me" },
    );
    const runId = submitted.result.runId;

    const received: AiRunEvent[] = [];
    for await (const event of client.streamRun(runId, { retryDelayMs: 1 })) {
      received.push(event);
    }

    expect(severing.cuts()).toBe(1);
    // Two connections: the first was severed, the second carried a resume.
    expect(severing.resumeHeaders.length).toBe(2);
    expect(severing.resumeHeaders[0]).toBeNull();
    expect(severing.resumeHeaders[1]).toBeString();

    const log = await logOf(runId);
    expect(received.map((e) => e.eventId)).toEqual(log.map((e) => e.eventId));
    expect(received.map((e) => e.seq)).toEqual(received.map((_e, i) => i));
    expect(new Set(received.map((e) => e.eventId)).size).toBe(received.length);
    expect(received.at(-1)?.type).toBe("run.completed");

    // The resume asked for exactly the id of an event the caller had already
    // been handed — not one past it, not one before.
    const resumedFrom = severing.resumeHeaders[1];
    expect(received.some((e) => e.eventId === resumedFrom)).toBe(true);
  });

  test("survives being severed on every connection", async () => {
    server = await startTestServer({ provider: chattyProvider(20) });
    // Three frames a connection — the `retry:` hint and two events — and every
    // connection cut, so the whole 23-event log arrives across a dozen resumes.
    const severing = severingFetch(3, 99);
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: severing.fetch,
    });

    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "stream me" },
    );
    const runId = submitted.result.runId;

    const received: AiRunEvent[] = [];
    for await (const event of client.streamRun(runId, { retryDelayMs: 1 })) {
      received.push(event);
    }

    const log = await logOf(runId);
    expect(severing.cuts()).toBeGreaterThanOrEqual(10);
    expect(severing.resumeHeaders).toHaveLength(severing.cuts());
    expect(received.map((e) => e.eventId)).toEqual(log.map((e) => e.eventId));
    expect(received.map((e) => e.seq)).toEqual(received.map((_e, i) => i));
    expect(new Set(received.map((e) => e.eventId)).size).toBe(received.length);
  });

  test("gives up once the retry budget is spent, and says why", async () => {
    server = await startTestServer({ provider: chattyProvider(60) });
    // Cut every connection, immediately: nothing is ever delivered, so no
    // attempt earns the budget back.
    const severing = severingFetch(0, 99);
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: severing.fetch,
    });
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "stream me" },
    );

    const iterate = async () => {
      for await (const _event of client.streamRun(submitted.result.runId, {
        maxRetries: 2,
        retryDelayMs: 1,
      })) {
        // no-op
      }
    };
    await expect(iterate()).rejects.toThrow(
      "connection reset by the test proxy",
    );
    // The first attempt plus two retries.
    expect(severing.resumeHeaders).toHaveLength(3);
  });

  test("an explicit lastEventId starts one past that event", async () => {
    server = await startTestServer({ provider: chattyProvider(10) });
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "stream me" },
    );
    const runId = submitted.result.runId;
    await waitFor(
      async () => (await client.getRun({ runId })).status === "completed",
      "the run to complete",
    );

    const log = await logOf(runId);
    const from = log[3]!;
    const tail = [];
    for await (const event of client.streamRun(runId, {
      lastEventId: from.eventId,
    })) {
      tail.push(event);
    }
    expect(tail.map((e) => e.seq)).toEqual(log.slice(4).map((e) => e.seq));
  });

  test("an unknown lastEventId replays the whole log rather than failing", async () => {
    server = await startTestServer({ provider: chattyProvider(5) });
    const client = createAgentKitClient({ baseUrl: server.baseUrl });
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "stream me" },
    );
    const runId = submitted.result.runId;
    await waitFor(
      async () => (await client.getRun({ runId })).status === "completed",
      "the run to complete",
    );

    const all = [];
    for await (const event of client.streamRun(runId, {
      lastEventId: "evt-from-another-run",
    })) {
      all.push(event);
    }
    expect(all.map((e) => e.seq)).toEqual(
      (await logOf(runId)).map((e) => e.seq),
    );
  });

  test("aborting the signal ends the iteration without reconnecting", async () => {
    server = await startTestServer({ provider: chattyProvider(200) });
    const severing = severingFetch(200, 99);
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: severing.fetch,
    });
    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "stream me" },
    );
    const controller = new AbortController();

    const iterate = async () => {
      for await (const _event of client.streamRun(submitted.result.runId, {
        signal: controller.signal,
        retryDelayMs: 5_000,
      })) {
        controller.abort();
      }
    };
    await expect(iterate()).rejects.toThrow();
    // One connection: the abort was not answered with a reconnect.
    expect(severing.resumeHeaders).toHaveLength(1);
  });
});

describe("streamRun tells a broken store from a finished run", () => {
  test("a failed log read breaks the body, and the client resumes over it", async () => {
    // A clean end of body is the server saying "the task is terminal", and this
    // client is required to take that at face value. So a `listEvents` that
    // lost a race with `SQLITE_BUSY` must NOT end the body cleanly — it used
    // to, and the iteration returned in the middle of a live pass with no
    // reconnect and nothing to tell the caller anything had gone wrong.
    //
    // The provider parks, so the run is DEMONSTRABLY unfinished when the read
    // fails; a run that could complete on its own would make "the stream ended"
    // ambiguous, which is the whole ambiguity under test.
    server = await startTestServer({ provider: new ParkingProvider() });

    // The SSE stream is the only reader of the event log — the host appends but
    // never lists — so failing one `listEvents` fails exactly one stream read
    // and nothing the turn is doing.
    const tasks = server.store.tasks;
    const realList = tasks.listEvents.bind(tasks);
    let armed = false;
    let thrown = false;
    tasks.listEvents = async (taskId, opts) => {
      if (armed && !thrown) {
        thrown = true;
        throw new Error("SQLITE_BUSY: database is locked");
      }
      return realList(taskId, opts);
    };

    const resumeHeaders: (string | null)[] = [];
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: async (url, init) => {
        if (url.includes("/stream")) {
          resumeHeaders.push(
            new Headers(init?.headers ?? {}).get("last-event-id"),
          );
        }
        return fetch(url, init);
      },
    });

    const submitted = await client.submitMessage(
      { chatId: TEST_CHAT_ID },
      { content: "stream me" },
    );
    const runId = submitted.result.runId;

    const received: AiRunEvent[] = [];
    const iteration = (async () => {
      for await (const event of client.streamRun(runId, { retryDelayMs: 1 })) {
        received.push(event);
        // Armed only once bytes have demonstrably reached this client, so what
        // the failure breaks is the BODY of a 200 rather than the response.
        armed = true;
      }
    })();

    await waitFor(
      async () => thrown && resumeHeaders.length > 1,
      "the store failure to break the stream and the client to resume",
    );
    // The run is parked on the provider; cancelling is what lets it end at all.
    await client.cancelRun({ runId });
    await iteration;

    // Two connections: the store failure was a broken pipe, and the resume
    // carried the id of the last event the caller had actually been handed.
    expect(resumeHeaders).toHaveLength(2);
    expect(resumeHeaders[1]).toBeString();
    const log = await logOf(runId);
    expect(received.map((e) => e.eventId)).toEqual(log.map((e) => e.eventId));
    expect(new Set(received.map((e) => e.eventId)).size).toBe(received.length);
  });
});

describe("streamRun surfaces a problem response instead of retrying it", () => {
  test("a run that does not exist is a typed 404, not a reconnect loop", async () => {
    server = await startTestServer();
    const severing = severingFetch(0, 0);
    const client = createAgentKitClient({
      baseUrl: server.baseUrl,
      fetch: severing.fetch,
    });
    const iterate = async () => {
      for await (const _event of client.streamRun("run-nope")) {
        // no-op
      }
    };
    await expect(iterate()).rejects.toMatchObject({
      status: 404,
      code: "not_found",
    });
    expect(severing.resumeHeaders).toHaveLength(1);
  });
});
