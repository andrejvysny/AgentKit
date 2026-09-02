/**
 * The three bounds that stop a misbehaving server from driving the client:
 * a clamped `retry:` hint, a reconnect budget progress cannot earn back, and
 * de-duplication of a replayed tail.
 *
 * WHY A SCRIPTED `fetch` RATHER THAN THE REAL SERVER, which every other test in
 * this package uses: the failures under test are things a CORRECT server never
 * does. `retry: 0`, a connection that delivers one event and dies forever, a
 * resume answered from the start of the log — `@agentkit/transport-http` does
 * none of them, and a client whose safety depends on the server behaving is not
 * a client with bounds. So the server here is a function that misbehaves on
 * purpose.
 *
 * The `controller.error(...)` is raised on the pull AFTER the one that
 * enqueued, for the reason `resume.test.ts` gives at length: erroring in the
 * same turn discards the queue and takes the frames with it.
 */
import { describe, expect, test } from "bun:test";
import { CONTRACT_VERSION, type AiRunEvent } from "@agentkit/contracts";
import {
  createAgentKitClient,
  MIN_STREAM_RETRY_DELAY_MS,
  type FetchLike,
} from "../src/index.js";

const BASE_URL = "http://stream.test";

function event(type: AiRunEvent["type"], seq: number): AiRunEvent {
  return {
    type,
    runId: "run-1",
    timestamp: new Date(seq * 1000).toISOString(),
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-${seq}`,
    seq,
    data: type === "run.message.delta" ? { delta: `tok-${seq}` } : {},
  } as AiRunEvent;
}

/** The events as SSE frames, `id:` included, exactly as the server writes them. */
function frames(...events: AiRunEvent[]): string {
  return events
    .map((e) => `id: ${e.eventId}\ndata: ${JSON.stringify(e)}\n\n`)
    .join("");
}

/** A response body that delivers `text`, then either closes or breaks. */
function body(
  text: string,
  then: "close" | "error",
): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let sent = false;
  return new ReadableStream<Uint8Array>({
    pull(controller) {
      if (!sent) {
        sent = true;
        if (text.length > 0) controller.enqueue(encoder.encode(text));
        return;
      }
      if (then === "error") {
        controller.error(new Error("connection reset by the test"));
        return;
      }
      controller.close();
    },
  });
}

function sse(text: string, then: "close" | "error"): Response {
  return new Response(body(text, then), { status: 200 });
}

describe("streamRun bounds a server that misbehaves", () => {
  test("a retry hint below the floor is clamped, not obeyed", async () => {
    // `retry: 0` is a reconnect loop with no pause in it.
    const opened: number[] = [];
    const fetchImpl: FetchLike = async () => {
      opened.push(Date.now());
      return opened.length === 1
        ? sse(`retry: 0\n\n${frames(event("run.message.delta", 0))}`, "error")
        : sse(frames(event("run.completed", 1)), "close");
    };
    const client = createAgentKitClient({
      baseUrl: BASE_URL,
      fetch: fetchImpl,
    });

    const seen: AiRunEvent[] = [];
    for await (const e of client.streamRun("run-1")) seen.push(e);

    expect(opened).toHaveLength(2);
    expect(seen.map((e) => e.eventId)).toEqual(["evt-0", "evt-1"]);
    // Slack below the floor for timer resolution; the point is that it is not
    // the ~0 ms the server asked for.
    expect(opened[1]! - opened[0]!).toBeGreaterThanOrEqual(
      MIN_STREAM_RETRY_DELAY_MS - 25,
    );
  });

  test("a server that drops after every event still runs out of budget", async () => {
    // Every connection delivers one event, so the PER-STRETCH budget is earned
    // back every time and only the total cap can end this.
    //
    // A real hostile server would go on forever. The fixture RELENTS after a
    // hundred connections so that a client without a total budget fails this
    // test rather than hanging the suite in an unbounded reconnect loop — the
    // failure it is here to catch.
    let opened = 0;
    const fetchImpl: FetchLike = async () => {
      opened += 1;
      if (opened > 100)
        return sse(frames(event("run.completed", opened)), "close");
      return sse(frames(event("run.message.delta", opened)), "error");
    };
    const client = createAgentKitClient({
      baseUrl: BASE_URL,
      fetch: fetchImpl,
    });

    const iterate = async (): Promise<void> => {
      for await (const _e of client.streamRun("run-1", {
        retryDelayMs: 1,
        maxTotalReconnects: 3,
      })) {
        // no-op
      }
    };
    await expect(iterate()).rejects.toThrow("connection reset by the test");
    // The first attempt plus the three reconnects the budget allowed.
    expect(opened).toBe(4);
  });

  test("a resume answered from the start of the log is not delivered twice", async () => {
    const log = [
      event("run.message.delta", 0),
      event("run.message.delta", 1),
      event("run.message.delta", 2),
      event("run.completed", 3),
    ];
    const resumeHeaders: (string | null)[] = [];
    let opened = 0;
    const fetchImpl: FetchLike = async (_url, init) => {
      opened += 1;
      resumeHeaders.push(new Headers(init?.headers ?? {}).get("last-event-id"));
      if (opened === 1) return sse(frames(log[0]!, log[1]!), "error");
      // A server that did not recognise the `Last-Event-ID` replays the WHOLE
      // log — its documented right, and a UI that appended the replayed tail
      // would show the answer twice.
      return sse(frames(...log), "close");
    };
    const client = createAgentKitClient({
      baseUrl: BASE_URL,
      fetch: fetchImpl,
    });

    const seen: AiRunEvent[] = [];
    for await (const e of client.streamRun("run-1", { retryDelayMs: 1 })) {
      seen.push(e);
    }

    expect(opened).toBe(2);
    expect(resumeHeaders[1]).toBe("evt-1");
    expect(seen.map((e) => e.eventId)).toEqual(log.map((e) => e.eventId));
  });
});
