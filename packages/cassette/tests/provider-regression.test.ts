import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  AiToolRegistry,
  OpenAiCompatibleClient,
  ResponsesClient,
  resolveToolLimits,
  runAgentLoop,
  type AiProviderClient,
  type AiTool,
  type AiToolResult,
  type TrustedResponsesTransport,
} from "@agentkit/core";
import { createCassette } from "../src/index.js";
import { collect, makeTool, okResult } from "./support/harness.js";

/**
 * B5 provider regression: the enumerated provider/stream edge cases run
 * through the REAL provider clients and the cassette. Recording goes against
 * stubbed transports; replay must (a) reproduce the event stream, (b) never
 * touch a transport. This is the offline CI gate for provider behaviour.
 */

const tempDirs: string[] = [];
async function fixturePath(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "cassette-prov-"));
  tempDirs.push(dir);
  return join(dir, "fixture.json");
}
afterEach(async () => {
  for (const dir of tempDirs.splice(0))
    await rm(dir, { recursive: true, force: true });
});

const limits = resolveToolLimits({ preference: "large" });

/* ----------------------------- openai stubbing ----------------------------- */

function sse(chunks: unknown[], done = true): Response {
  const text = chunks.map((c) => `data: ${JSON.stringify(c)}\n\n`).join("");
  return new Response(`${text}${done ? "data: [DONE]\n\n" : ""}`, {
    headers: { "content-type": "text/event-stream" },
  });
}
function contentChunks(text: string): unknown[] {
  return [
    { choices: [{ delta: { content: text.slice(0, 2) } }] },
    { choices: [{ delta: { content: text.slice(2) } }] },
    { choices: [{ delta: {}, finish_reason: "stop" }] },
  ];
}

class StubFetch {
  hits = 0;
  /** Each entry: a Response to return, or a thrown Error, or "boom". */
  constructor(private readonly script: (() => Promise<Response>)[]) {}
  fetch: typeof fetch = (async (
    _url: string | URL | Request,
    _init?: RequestInit,
  ) => {
    const step = this.script[this.hits];
    this.hits += 1;
    if (!step) throw new Error("stub fetch exhausted");
    return step();
  }) as unknown as typeof fetch;
}
function throwingFetch(): typeof fetch {
  return (async () => {
    throw new Error("network touched during replay");
  }) as unknown as typeof fetch;
}

function oaClient(id: string, fetchImpl: typeof fetch): OpenAiCompatibleClient {
  return new OpenAiCompatibleClient({
    id,
    kind: "openai-compatible",
    baseUrl: "http://localhost:1/v1",
    apiKey: "test-key",
    fetchImpl,
  });
}

const oaRequest = (
  model = "m",
): Parameters<OpenAiCompatibleClient["streamChat"]>[0] =>
  ({
    runId: "run-prov",
    model,
    messages: [{ role: "user", content: "regression prompt" }],
  }) as never;

async function recordThenReplay(
  build: (kind: "record" | "replay") => AiProviderClient,
  invoke: (client: AiProviderClient) => Promise<AiRunEvent[]>,
): Promise<{ recorded: AiRunEvent[]; replayed: AiRunEvent[]; path: string }> {
  const path = await fixturePath();
  const record = createCassette({ mode: "record", fixture: path });
  const recorded = await invoke(record.provider(build("record")));
  await record.close();
  const replay = createCassette({ mode: "replay", fixture: path });
  const replayed = await invoke(replay.provider(build("replay")));
  await replay.close();
  return { recorded, replayed, path };
}

const typesOf = (events: AiRunEvent[]): string[] => events.map((e) => e.type);

/* ----------------------------- responses stubbing -------------------------- */

function responsesHarness(events: unknown[], id = "rsp") {
  let hits = 0;
  const transport: TrustedResponsesTransport = {
    identity: {
      providerId: id,
      protocol: "responses",
      connectionId: "connection-1",
      generation: 1,
    },
    authKind: "api-key",
    async request() {
      hits += 1;
      const text = events.map((e) => `data: ${JSON.stringify(e)}\n\n`).join("");
      return new Response(text, {
        headers: { "content-type": "text/event-stream" },
      });
    },
  };
  return {
    client: new ResponsesClient({ id, transport }),
    get hits() {
      return hits;
    },
  };
}
function offlineResponses(id = "rsp"): ResponsesClient {
  const transport: TrustedResponsesTransport = {
    identity: {
      providerId: id,
      protocol: "responses",
      connectionId: "connection-1",
      generation: 1,
    },
    authKind: "api-key",
    async request() {
      throw new Error("transport touched during replay");
    },
  };
  return new ResponsesClient({ id, transport });
}
const rspCompleted = (text: string): unknown => ({
  type: "response.completed",
  response: {
    id: "resp-1",
    status: "completed",
    output: [
      {
        type: "message",
        id: "msg-1",
        role: "assistant",
        status: "completed",
        content: [{ type: "output_text", text }],
      },
    ],
    usage: { input_tokens: 7, output_tokens: 3, total_tokens: 10 },
  },
});

/* --------------------------------- tests ---------------------------------- */

describe("provider regression: openai-compatible streams (B5)", () => {
  test("streamed text round-trips record -> replay", async () => {
    const stub = new StubFetch([async () => sse(contentChunks("hello there"))]);
    const { recorded, replayed } = await recordThenReplay(
      (kind) =>
        oaClient("oa", kind === "record" ? stub.fetch : throwingFetch()),
      (c) => collect(c.streamChat(oaRequest() as never)),
    );
    expect(typesOf(replayed)).toEqual(typesOf(recorded));
    expect(stub.hits).toBe(1);
    expect(recorded.some((e) => e.type === "run.message.completed")).toBe(true);
  });

  test("fragmented tool-call arguments assemble into one call and replay", async () => {
    const chunks = [
      {
        choices: [
          {
            delta: {
              tool_calls: [
                {
                  index: 0,
                  id: "call-1",
                  function: { name: "grep", arguments: '{"pat' },
                },
              ],
            },
          },
        ],
      },
      {
        choices: [
          {
            delta: {
              tool_calls: [{ index: 0, function: { arguments: 'tern":"x"}' } }],
            },
          },
        ],
      },
      { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
    ];
    const stub = new StubFetch([async () => sse(chunks)]);
    const { recorded, replayed } = await recordThenReplay(
      (kind) =>
        oaClient("oa", kind === "record" ? stub.fetch : throwingFetch()),
      (c) => collect(c.streamChat(oaRequest() as never)),
    );
    const requested = recorded.filter((e) => e.type === "run.tool.requested");
    expect(requested.length).toBe(1);
    const firstCall = requested[0] as
      | { data: { argumentsJson: string } }
      | undefined;
    expect(firstCall?.data.argumentsJson).toBe('{"pattern":"x"}');
    expect(typesOf(replayed)).toEqual(typesOf(recorded));
  });

  test("missing terminal marker fails closed and replays identically", async () => {
    const stub = new StubFetch([
      async () =>
        sse([{ choices: [{ delta: { content: "partial" } }] }], false),
    ]);
    const { recorded, replayed } = await recordThenReplay(
      (kind) =>
        oaClient("oa", kind === "record" ? stub.fetch : throwingFetch()),
      (c) => collect(c.streamChat(oaRequest() as never)),
    );
    expect(recorded.at(-1)).toMatchObject({
      type: "run.failed",
      data: { errorCode: "incomplete_stream" },
    });
    expect(typesOf(replayed)).toEqual(typesOf(recorded));
  });

  test("empty response body normalizes and replays", async () => {
    const stub = new StubFetch([
      async () =>
        new Response("", { headers: { "content-type": "text/event-stream" } }),
    ]);
    const { recorded, replayed } = await recordThenReplay(
      (kind) =>
        oaClient("oa", kind === "record" ? stub.fetch : throwingFetch()),
      (c) => collect(c.streamChat(oaRequest() as never)),
    );
    expect(recorded.at(-1)?.type).toBe("run.failed");
    expect(typesOf(replayed)).toEqual(typesOf(recorded));
  });

  test("usage fragment in the final chunk is recorded and replays", async () => {
    const chunks = [
      ...contentChunks("done now"),
      { usage: { prompt_tokens: 11, completion_tokens: 4, total_tokens: 15 } },
    ];
    const stub = new StubFetch([async () => sse(chunks)]);
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const recorded = await collect(
      record
        .provider(oaClient("oa", stub.fetch))
        .streamChat(oaRequest() as never),
    );
    await record.close();
    const usage = recorded.find((e) => e.type === "run.usage");
    expect(usage?.data).toMatchObject({
      promptTokens: 11,
      completionTokens: 4,
    });
    const replay = createCassette({ mode: "replay", fixture: path });
    const replayed = await collect(
      replay
        .provider(oaClient("oa", throwingFetch()))
        .streamChat(oaRequest() as never),
    );
    await replay.close();
    expect(replayed.find((e) => e.type === "run.usage")?.data).toMatchObject({
      promptTokens: 11,
      completionTokens: 4,
    });
    expect(typesOf(replayed)).toEqual(typesOf(recorded));
  });

  test("provider retry through the loop replays with its retry warning", async () => {
    const stub = new StubFetch([
      async () => new Response("busy", { status: 503 }),
      async () => sse(contentChunks("recovered answer")),
    ]);
    const path = await fixturePath();
    const registry = new AiToolRegistry();
    const loop = (client: unknown, providerRetry: unknown) =>
      drain(
        runAgentLoop({
          client,
          registry,
          model: "m",
          messages: [{ role: "user", content: "go" }],
          limits,
          runId: "run-retry",
          providerRetry,
        } as never),
      );
    const retry = {
      maxAttempts: 2,
      baseDelayMs: 1,
      maxDelayMs: 2,
      random: () => 0.5,
      sleep: async () => {},
    };
    const record = createCassette({ mode: "record", fixture: path });
    const recorded = await loop(
      record.provider(oaClient("oa", stub.fetch)),
      retry,
    );
    await record.close();
    expect(
      recorded.events.some(
        (e) =>
          e.type === "run.warning" &&
          (e.data as { code: string }).code === "provider_retry",
      ),
    ).toBe(true);
    const replay = createCassette({ mode: "replay", fixture: path });
    const replayed = await loop(
      replay.provider(oaClient("oa", throwingFetch())),
      retry,
    );
    await replay.close();
    expect(typesOf(replayed.events)).toEqual(typesOf(recorded.events));
    expect(replayed.terminal).toBe("completed");
    expect(stub.hits).toBe(2);
  });

  test("cancellation during retry backoff replays as a cancellation", async () => {
    const stub = new StubFetch([
      async () => new Response("busy", { status: 503 }),
    ]);
    const path = await fixturePath();
    const registry = new AiToolRegistry();
    const loop = (client: unknown) => {
      const controller = new AbortController();
      const retry = {
        maxAttempts: 2,
        baseDelayMs: 1,
        maxDelayMs: 2,
        random: () => 0.5,
        sleep: async () => {
          controller.abort();
        },
      };
      return drain(
        runAgentLoop({
          client,
          registry,
          model: "m",
          messages: [{ role: "user", content: "go" }],
          limits,
          runId: "run-abort",
          providerRetry: retry,
          signal: controller.signal,
        } as never),
      );
    };
    const record = createCassette({ mode: "record", fixture: path });
    const recorded = await loop(record.provider(oaClient("oa", stub.fetch)));
    await record.close();
    expect(recorded.terminal).toBe("cancelled");
    const replay = createCassette({ mode: "replay", fixture: path });
    const replayed = await loop(
      replay.provider(oaClient("oa", throwingFetch())),
    );
    await replay.close();
    expect(replayed.terminal).toBe("cancelled");
    expect(typesOf(replayed.events)).toEqual(typesOf(recorded.events));
  });

  test("capability mismatch does not change the recorded replay", async () => {
    // The provider advertises no tool calling yet streams a tool call: the
    // loop-side contract stays what it was, and the cassette replays it.
    const chunks = [
      {
        choices: [
          {
            delta: {
              tool_calls: [
                {
                  index: 0,
                  id: "c1",
                  function: { name: "grep", arguments: "{}" },
                },
              ],
            },
          },
        ],
      },
      { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
    ];
    const stub = new StubFetch([async () => sse(chunks)]);
    const { recorded, replayed } = await recordThenReplay(
      (kind) =>
        oaClient("oa", kind === "record" ? stub.fetch : throwingFetch()),
      (c) => collect(c.streamChat(oaRequest() as never)),
    );
    expect(recorded.some((e) => e.type === "run.tool.requested")).toBe(true);
    expect(typesOf(replayed)).toEqual(typesOf(recorded));
  });
});

describe("provider regression: responses streams (B5)", () => {
  test("streamed message round-trips record -> replay", async () => {
    const { recorded, replayed } = await recordThenReplay(
      (kind) =>
        kind === "record"
          ? responsesHarness([rspCompleted("all good")]).client
          : offlineResponses(),
      (c) => collect(c.streamChat(oaRequest() as never)),
    );
    expect(typesOf(replayed)).toEqual(typesOf(recorded));
    expect(recorded.some((e) => e.type === "run.message.completed")).toBe(true);
  });

  test("reasoning summary + effort replay; effort is in the request view", async () => {
    const events = [
      {
        type: "response.output_item.added",
        output_index: 0,
        item: { type: "reasoning", id: "r1" },
      },
      {
        type: "response.reasoning_summary_text.delta",
        item_id: "r1",
        output_index: 0,
        summary_index: 0,
        delta: "thinking hard",
      },
      {
        type: "response.output_item.done",
        output_index: 0,
        item: { type: "reasoning", id: "r1", summary: [] },
      },
      {
        type: "response.output_item.added",
        output_index: 1,
        item: {
          type: "message",
          id: "msg-1",
          role: "assistant",
          status: "in_progress",
          content: [],
        },
      },
      {
        type: "response.output_text.delta",
        item_id: "msg-1",
        output_index: 1,
        content_index: 0,
        delta: "concluded",
      },
      {
        type: "response.output_item.done",
        output_index: 1,
        item: {
          type: "message",
          id: "msg-1",
          role: "assistant",
          status: "completed",
          content: [{ type: "output_text", text: "concluded" }],
        },
      },
      {
        type: "response.completed",
        response: {
          id: "resp-1",
          status: "completed",
          output: [
            { type: "reasoning", id: "r1", summary: [] },
            {
              type: "message",
              id: "msg-1",
              role: "assistant",
              status: "completed",
              content: [{ type: "output_text", text: "concluded" }],
            },
          ],
          usage: { input_tokens: 7, output_tokens: 3, total_tokens: 10 },
        },
      },
    ];
    const path = await fixturePath();
    const record = createCassette({ mode: "record", fixture: path });
    const recorded = await collect(
      record.provider(responsesHarness(events).client).streamChat({
        ...oaRequest(),
        reasoningEffort: "high",
      } as never),
    );
    await record.close();
    expect(recorded.some((e) => e.type === "run.message.reasoning-delta")).toBe(
      true,
    );
    const replay = createCassette({ mode: "replay", fixture: path });
    const replayed = await collect(
      replay.provider(offlineResponses()).streamChat({
        ...oaRequest(),
        reasoningEffort: "high",
      } as never),
    );
    await replay.close();
    // metadata capture never records hidden reasoning text (security design),
    // so the replayed stream legitimately drops the reasoning deltas
    expect(typesOf(replayed)).toEqual(
      typesOf(recorded).filter((t) => t !== "run.message.reasoning-delta"),
    );
    // reasoning text itself is never persisted under metadata capture
    const fixtureText = await Bun.file(path).text();
    expect(fixtureText.includes("thinking hard")).toBe(false);
  });

  test("continuation from a foreign connection fails closed and replays", async () => {
    const foreign = {
      protocol: "responses",
      providerId: "rsp",
      connectionId: "someone-elses-connection",
      generation: 9,
      opaque: "state",
    } as never;
    const { recorded, replayed } = await recordThenReplay(
      (kind) =>
        kind === "record"
          ? responsesHarness([rspCompleted("never")]).client
          : offlineResponses(),
      async (c) =>
        collect(
          c.streamChat({ ...oaRequest(), continuation: foreign } as never),
        ),
    );
    expect(recorded.at(-1)?.type).toBe("run.failed");
    expect(typesOf(replayed)).toEqual(typesOf(recorded));
  });
});

describe("provider regression: routed model switch (B5)", () => {
  test("a provider switch inside one run replays with tools executed once", async () => {
    const grep: AiTool = makeTool(
      "grep",
      async () => okResult("match") as AiToolResult,
    );
    const oaToolTurn = [
      {
        choices: [
          {
            delta: {
              tool_calls: [
                {
                  index: 0,
                  id: "c1",
                  function: { name: "grep", arguments: "{}" },
                },
              ],
            },
          },
        ],
      },
      { choices: [{ delta: {}, finish_reason: "tool_calls" }] },
    ];
    const stubA = new StubFetch([async () => sse(oaToolTurn)]);
    const stubB = new StubFetch([
      async () => sse(contentChunks("final words")),
    ]);
    const path = await fixturePath();
    let toolRuns = 0;
    const countedTool: AiTool = {
      ...grep,
      execute: async (ctx, input) => {
        toolRuns += 1;
        return grep.execute(ctx, input);
      },
    };
    const registryCounted = new AiToolRegistry();
    registryCounted.register(countedTool);

    const loop = (
      client: unknown,
      resolveClient: unknown,
      registry: AiToolRegistry,
    ) =>
      drain(
        runAgentLoop({
          client,
          registry,
          model: "m1",
          messages: [{ role: "user", content: "go" }],
          limits,
          runId: "run-switch",
          modelRouter: {
            selectModel: (ctx: { step: number }) =>
              ctx.step === 1
                ? { providerId: "oaA", modelId: "m1" }
                : { providerId: "oaB", modelId: "m2" },
          },
          resolveClient,
        } as never),
      );

    const record = createCassette({ mode: "record", fixture: path });
    const recorded = await loop(
      record.provider(oaClient("oaA", stubA.fetch)),
      async (providerId: string) =>
        record.provider(oaClient(providerId, stubB.fetch)),
      record.instrumentRegistry(registryCounted),
    );
    await record.close();
    expect(recorded.terminal).toBe("completed");
    expect(toolRuns).toBe(1);
    expect(stubA.hits + stubB.hits).toBe(2);

    const replay = createCassette({ mode: "replay", fixture: path });
    const explodingRegistry = new AiToolRegistry();
    explodingRegistry.register(
      makeTool("grep", async () => {
        throw new Error("tool body executed during replay");
      }),
    );
    const replayed = await loop(
      replay.provider(oaClient("oaA", throwingFetch())),
      async () => replay.provider(oaClient("oaB", throwingFetch())),
      replay.instrumentRegistry(explodingRegistry),
    );
    await replay.close();
    expect(typesOf(replayed.events)).toEqual(typesOf(recorded.events));
    expect(replayed.terminal).toBe("completed");
    // the tool replayed from the fixture: the counted body ran a second time
    // only if execution leaked — replay must NOT run tool bodies.
    expect(toolRuns).toBe(1);
    expect(stubA.hits + stubB.hits).toBe(2);
  });
});

async function drain(
  it: AsyncGenerator<AiRunEvent, { terminal: string }, unknown>,
): Promise<{ events: AiRunEvent[]; terminal: string }> {
  const events: AiRunEvent[] = [];
  let step = await it.next();
  while (!step.done) {
    events.push(step.value);
    step = await it.next();
  }
  return { events, terminal: step.value.terminal };
}
