import { expect, test } from "bun:test";
import { MemoryAssistantStore } from "@agentkit/adapters-memory";
import { createAgentKitClient } from "@agentkit/client";
import { CONTRACT_VERSION, type AiRunEvent } from "@agentkit/contracts";
import { DEFAULT_STREAM_OPTIONS } from "../src/deps.js";
import { createRunEventStream, resolveStartSeq } from "../src/sse.js";
import {
  nativeFixtureSkipReason,
  startNativeServer,
} from "../../../rust/tests/support/http-server.js";

// Native-backed variants belong to the Rust workflow; they run here only when
// the environment explicitly requests the native fixture.
const nativeTest = nativeFixtureSkipReason === undefined ? test : test.skip;

function events(sequences: number[], runId = "numeric"): AiRunEvent[] {
  return sequences.map((seq, index) => ({
    type: "run.message.delta",
    seq,
    runId,
    eventId: `numeric-${index}`,
    timestamp: "2026-01-01T00:00:00.000Z",
    contractVersion: CONTRACT_VERSION,
    data: { delta: `${index}` },
  }));
}
async function seed(source: AiRunEvent[]) {
  const store = new MemoryAssistantStore();
  await store.tasks.createTask({
    taskId: "numeric",
    kind: "fixture",
    scopeId: "scope",
    payload: {},
  });
  const lease = await store.tasks.acquireLease({
    taskId: "numeric",
    attemptId: "attempt",
    ownerId: "fixture",
    ttlMs: 60_000,
  });
  await store.tasks.appendEvents("numeric", source, {
    leaseToken: lease.leaseToken,
  });
  await store.tasks.transitionTask("numeric", ["queued"], "running");
  await store.tasks.transitionTask("numeric", ["running"], "completed");
  return store;
}
function baseline(
  store: MemoryAssistantStore,
  startSeq = 0,
  batch = 100,
  signal?: AbortSignal,
) {
  return createRunEventStream({
    tasks: store.tasks,
    taskId: "numeric",
    startSeq,
    options: {
      ...DEFAULT_STREAM_OPTIONS,
      readBatchSize: batch,
      pollIntervalMs: 1,
    },
    ...(signal === undefined ? {} : { signal }),
  });
}
function sequences(wire: string): number[] {
  return wire
    .split("\n")
    .filter((line) => line.startsWith("data: ") && line !== "data: {}")
    .map((line) => (JSON.parse(line.slice(6)) as AiRunEvent).seq);
}

// These expectations freeze a reference defect; native numeric cursors correct it.
test("TypeScript HTTP baseline drops negative and close fractional sequences", async () => {
  const cases: [number[], number[]][] = [
    [
      [0, 2, 9],
      [0, 2, 9],
    ],
    [
      [-0.5, 0.25, 0.5, 1.5],
      [0.25, 1.5],
    ],
  ];
  for (const [input, output] of cases) {
    const store = await seed(events(input));
    expect(sequences(await new Response(baseline(store)).text())).toEqual(
      output,
    );
  }
});

test("TypeScript HTTP wide cursor repeats instead of advancing; reproduction is bounded", async () => {
  const wide = 2 ** 53;
  const store = await seed(events([wide, wide + 2]));
  const cursor = await resolveStartSeq(store.tasks, "numeric", "numeric-0", 1);
  expect(cursor).toBe(wide);
  const abort = new AbortController();
  const timer = setTimeout(() => abort.abort(), 1000);
  const reader = baseline(store, cursor, 1, abort.signal).getReader();
  const decoder = new TextDecoder();
  let wire = "";
  try {
    while (sequences(wire).length < 4) {
      const chunk = await reader.read();
      if (chunk.done) break;
      wire += decoder.decode(chunk.value);
    }
    expect(sequences(wire)).toEqual([wide, wide, wide, wide]);
    expect(wire).not.toContain("agentkit.stream.settled");
  } finally {
    clearTimeout(timer);
    abort.abort();
    await reader.cancel();
  }
});

test("TypeScript client dedupes finite numeric high water without requiring integer steps", async () => {
  const source = events([
    -2.5,
    -1,
    0.25,
    0.5,
    4.75,
    2 ** 53,
    2 ** 53 + 2,
    1e308,
  ]);
  let connections = 0;
  const cursors: (string | null)[] = [];
  const server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    fetch(request) {
      cursors.push(request.headers.get("last-event-id"));
      const first = connections++ === 0;
      const wire = (first ? source.slice(0, 3) : source)
        .map(
          (event) =>
            `id: ${event.eventId}\nevent: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`,
        )
        .join("");
      return new Response(
        wire + (first ? "" : "event: agentkit.stream.settled\ndata: {}\n\n"),
        { headers: { "content-type": "text/event-stream" } },
      );
    },
  });
  try {
    const client = createAgentKitClient({
      baseUrl: `http://127.0.0.1:${server.port}`,
    });
    const received = [];
    for await (const event of client.streamRun("numeric", { retryDelayMs: 1 }))
      received.push(event.seq);
    expect(received).toEqual(source.map((event) => event.seq));
    expect(cursors).toEqual([null, "numeric-2"]);
  } finally {
    await server.stop(true);
  }
});

nativeTest(
  "native SQLite SSE preserves numeric sequences and resumes exclusively at the last event",
  async () => {
    let native = await startNativeServer();
    const source = events([-0.5, 0.25, 0.5, 4.75, 2 ** 53, 2 ** 53 + 2, 1e308]);
    source[0]!.eventId = "";
    try {
      const response = await fetch(`${native.controlUrl}/tasks`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${native.controlToken}`,
          "content-type": "application/json",
        },
        body: JSON.stringify({
          operation: "seedNumericRun",
          taskId: "numeric",
          events: source,
        }),
      });
      expect(response.status).toBe(200);
      for (const [cursor, start] of [
        [undefined, 0],
        ["unknown", 0],
        ["", 0],
        ["numeric-1", 2],
        ["numeric-4", 5],
        ["numeric-6", 7],
      ] as const) {
        const received = [];
        for await (const event of native.client.streamRun("numeric", {
          ...(cursor === undefined ? {} : { lastEventId: cursor }),
          maxRetries: 0,
        }))
          received.push(event.seq);
        expect(received).toEqual(source.slice(start).map((event) => event.seq));
      }
      native = await native.restart();
      const replay = [];
      for await (const event of native.client.streamRun("numeric", {
        maxRetries: 0,
      }))
        replay.push(event.seq);
      expect(replay).toEqual(source.map((event) => event.seq));
    } finally {
      await native.stop();
    }
  },
  60_000,
);
