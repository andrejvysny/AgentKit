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

function event(
  type: AiRunEvent["type"],
  seq: number,
  data?: Record<string, unknown>,
): AiRunEvent {
  return {
    type,
    runId: "run-1",
    timestamp: new Date(seq * 1000).toISOString(),
    contractVersion: CONTRACT_VERSION,
    eventId: `evt-${seq}`,
    seq,
    data: data ?? (type === "run.message.delta" ? { delta: `tok-${seq}` } : {}),
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

  test("a long replay past the old id-window still dedupes cleanly", async () => {
    // W1: a 6000-event log breaks at 5000, and the server (not recognising the
    // `Last-Event-ID` — its documented right) replays from the start. An
    // `eventId`-window dedupe of a few thousand ids would have evicted event 0
    // long before event 5000 reconnects, so the replayed head (0..4999) would
    // land as 5000 duplicates. Dedupe by `seq` has no window to outrun.
    const total = 6000;
    const breakAt = 5000;
    const log = Array.from({ length: total }, (_unused, index) =>
      event("run.message.delta", index),
    );
    log.push(event("run.completed", total));
    let opened = 0;
    const fetchImpl: FetchLike = async () => {
      opened += 1;
      if (opened === 1) {
        return sse(frames(...log.slice(0, breakAt)), "error");
      }
      // A resume the server answered from the start of the log, not from
      // `Last-Event-ID` — the replay this dedupe exists to catch.
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
    expect(seen).toHaveLength(total + 1);
    expect(seen.map((e) => e.seq)).toEqual(log.map((e) => e.seq));
  });
});

/**
 * A scripted server here for the opposite reason to the ones above: this is
 * what a CORRECT server does, and the shape is hard to hit reliably against the
 * real one. The host runs a recovery pass after a failed pass, so `run.failed`
 * can be followed by a `retry_pass` warning and a whole second pass on the same
 * log — and the connection can break in between, as connections do.
 */
describe("streamRun keeps going past a terminal event", () => {
  test("a break after run.failed reconnects and delivers the retry pass", async () => {
    const passOne = [
      event("run.started", 0, { model: "m", toolCount: 1 }),
      event("run.message.delta", 1),
      event("run.failed", 2, { errorMessage: "the provider said no" }),
    ];
    const passTwo = [
      event("run.warning", 3, {
        code: "retry_pass",
        message: "Retrying without tools.",
        pass: 2,
        reason: "chat_only",
      }),
      event("run.started", 4, { model: "m", toolCount: 0 }),
      event("run.message.delta", 5),
      event("run.completed", 6, { iterations: 1 }),
    ];
    const resumeHeaders: (string | null)[] = [];
    let opened = 0;
    const fetchImpl: FetchLike = async (_url, init) => {
      opened += 1;
      resumeHeaders.push(new Headers(init?.headers ?? {}).get("last-event-id"));
      // The task is still `running` when the pipe breaks — the server has not
      // closed anything, the connection died.
      if (opened === 1) return sse(frames(...passOne), "error");
      return sse(frames(...passTwo), "close");
    };
    const client = createAgentKitClient({
      baseUrl: BASE_URL,
      fetch: fetchImpl,
    });

    const seen: AiRunEvent[] = [];
    for await (const e of client.streamRun("run-1", { retryDelayMs: 1 })) {
      seen.push(e);
    }

    // Stopping at `run.failed` reported pass 1's failure as the run's answer
    // while pass 2 was writing the real one.
    expect(opened).toBe(2);
    expect(resumeHeaders[1]).toBe("evt-2");
    expect(seen.map((e) => e.eventId)).toEqual([
      ...passOne.map((e) => e.eventId),
      ...passTwo.map((e) => e.eventId),
    ]);
    expect(seen.at(-1)?.type).toBe("run.completed");
  });
});
