import { expect, it } from "bun:test";
import type { AiRunEvent, TaskEventEnvelope } from "@agentkit/contracts";
import type { AiChatRequest } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  AgentSessionService,
  TaskService,
  TurnRunner,
  type ToolSetContributor,
} from "../src/index.js";
import { sessionWithAdmittedContext } from "../src/sessions/admitted-context.js";
import { boundedContextClient } from "../src/context/bounded-context-client.js";
import { FakeAgentInputStore, createHarness } from "./fakes.js";

class Provider extends MockProviderClient {
  calls = 0;
  requests: AiChatRequest[] = [];
  constructor(readonly maxOutputTokens?: number) {
    super();
  }
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.calls += 1;
    this.requests.push(input);
    return super.streamChat(input);
  }
}

it("bounds the output ceiling by the admitted reserve at the provider boundary", async () => {
  const f = await fixture({ windowTokens: 4096, reserveTokens: 1024 });
  const id = await send(f, "first");
  const task = await f.store.tasks.getTask(id);
  if (!task) throw new Error("Missing turn");
  const client = await boundedContextClient(f.mock, {
    ...f,
    task,
    chatId: "s",
    providerId: "p",
    model: "m",
    attemptId: "output-attempt",
  });
  f.mock.setScript([{ steps: [{ kind: "text", content: "answer" }] }]);
  for await (const _event of client.streamChat({
    runId: id,
    model: "m",
    messages: [{ role: "user", content: "x".repeat(8192) }],
    maxOutputTokens: 4096,
  })) {
  }
  expect(f.mock.requests[0]?.maxOutputTokens).toBe(1024);
});

async function fixture(
  context?: { windowTokens: number; reserveTokens?: number },
  contributors: ToolSetContributor[] = [],
  maxOutputTokens?: number,
) {
  const h = createHarness();
  h.store.inputs = new FakeAgentInputStore(h.clock);
  await h.store.providers.upsertProvider({
    id: "p",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1",
    defaultModel: "m",
    enabled: true,
  });
  const mock = new Provider(maxOutputTokens);
  const runner = new TurnRunner({
    ...h,
    providerFactory: () => mock,
    contributors,
  });
  const tasks = new TaskService(h);
  const service = new AgentSessionService({ ...h, turns: runner, tasks });
  await service.createSession({
    sessionId: "s",
    profile: { id: "default" },
    providerId: "p",
    model: "m",
    ...(context ? { context } : {}),
  });
  return { ...h, runner, mock, service };
}

async function outputClient(f: Awaited<ReturnType<typeof fixture>>) {
  const id = await send(f, "first");
  const task = await f.store.tasks.getTask(id);
  if (!task) throw new Error("Missing turn");
  const client = await boundedContextClient(f.mock, {
    ...f,
    task,
    chatId: "s",
    providerId: "p",
    model: "m",
    attemptId: "output-attempt",
  });
  f.mock.setScript([{ steps: [{ kind: "text", content: "answer" }] }]);
  return {
    client,
    request: {
      runId: id,
      model: "m",
      messages: [{ role: "user" as const, content: "short" }],
    },
  };
}

it.each([undefined, 128, 1024, 4096])(
  "forwards explicit output limits and matching diagnostics for requested %s",
  async (requested) => {
    const f = await fixture({ windowTokens: 4096, reserveTokens: 1024 });
    const { client, request } = await outputClient(f);
    const events = [];
    for await (const event of client.streamChat({
      ...request,
      ...(requested === undefined ? {} : { maxOutputTokens: requested }),
    }))
      events.push(event);
    const expected = Math.min(requested ?? 1024, 1024);
    expect(f.mock.requests[0]?.maxOutputTokens).toBe(expected);
    expect(
      events.find((event) => event.type === "run.context.admitted"),
    ).toMatchObject({
      data: { reserveTokens: 1024, maxOutputTokens: expected },
    });
  },
);

it.each([0, -1, NaN, Infinity, 1.5])(
  "refuses invalid explicit output limit %s without dispatch",
  async (maxOutputTokens) => {
    const f = await fixture({ windowTokens: 4096, reserveTokens: 1024 });
    const { client, request } = await outputClient(f);
    const events = [];
    for await (const event of client.streamChat({
      ...request,
      maxOutputTokens,
    }))
      events.push(event);
    expect(f.mock.calls).toBe(0);
    expect(events).toContainEqual(
      expect.objectContaining({
        type: "run.failed",
        data: {
          errorCode: "invalid_max_output_tokens",
          errorMessage: expect.any(String),
        },
      }),
    );
  },
);

it("includes the client's immutable output ceiling in admission and rejects an invalid client ceiling", async () => {
  for (const ceiling of [64, 0]) {
    const f = await fixture(
      { windowTokens: 4096, reserveTokens: 1024 },
      [],
      ceiling,
    );
    const { client, request } = await outputClient(f);
    const events = [];
    for await (const event of client.streamChat({
      ...request,
      maxOutputTokens: 4096,
    }))
      events.push(event);
    expect(client.maxOutputTokens).toBe(ceiling);
    expect(f.mock.calls).toBe(ceiling === 0 ? 0 : 1);
    if (ceiling > 0)
      expect(
        events.find((event) => event.type === "run.context.admitted"),
      ).toMatchObject({ data: { maxOutputTokens: ceiling } });
  }
});

it("sets a positive ceiling for a two-token window and refuses a window with no reply budget", async () => {
  for (const windowTokens of [2, 1]) {
    const f = await fixture({ windowTokens, reserveTokens: 1 });
    const { client, request } = await outputClient(f);
    const events = [];
    for await (const event of client.streamChat({ ...request, messages: [] }))
      events.push(event);
    expect(f.mock.calls).toBe(windowTokens === 2 ? 1 : 0);
    if (windowTokens === 2) expect(f.mock.requests[0]?.maxOutputTokens).toBe(1);
    else
      expect(events.find((event) => event.type === "run.failed")).toMatchObject(
        { data: { errorCode: "context_overflow" } },
      );
  }
});

async function send(
  f: Awaited<ReturnType<typeof fixture>>,
  content: string,
  inputId = "first",
) {
  const result = await f.service.send("s", { content, inputId });
  if (result.kind !== "turn") throw new Error("Expected a turn");
  return result.turnId;
}

async function drive(f: Awaited<ReturnType<typeof fixture>>, taskId: string) {
  await f.store.tasks.transitionTask(taskId, ["queued"], "running");
  const attemptId = f.ids.attemptId();
  await f.store.tasks.createAttempt({ attemptId, taskId, ownerId: "worker" });
  const lease = await f.store.tasks.acquireLease({
    taskId,
    attemptId,
    ownerId: "worker",
    ttlMs: 60_000,
  });
  await f.runner.execute({
    taskId,
    attemptId,
    leaseToken: lease.leaseToken,
    signal: new AbortController().signal,
  });
}

it("pins the admitted budget before execution despite future configuration", async () => {
  const f = await fixture({ windowTokens: 4000, reserveTokens: 1000 });
  const id = await send(f, "x".repeat(20_000));
  await f.service.configure("s", { context: { windowTokens: 100_000 } });
  expect((await f.store.tasks.getTask(id))?.payload["sessionContext"]).toEqual({
    windowTokens: 4000,
    reserveTokens: 1000,
  });
  await expect(drive(f, id)).rejects.toMatchObject({
    code: "context_overflow",
  });
  expect(f.mock.calls).toBe(0);
  expect(await f.service.getUsage("s")).toMatchObject({
    contextTurnId: id,
    contextWindowTokens: 4000,
  });
});

it("pins no context override explicitly, so future limits cannot leak backwards", async () => {
  const f = await fixture();
  const id = await send(f, "x".repeat(3000));
  expect(
    (await f.store.tasks.getTask(id))?.payload["sessionContext"],
  ).toBeNull();
  await f.service.configure("s", { context: { windowTokens: 1000 } });
  f.mock.setScript([{ steps: [{ kind: "text", content: "answer" }] }]);
  await drive(f, id);
  expect(f.mock.calls).toBe(1);
  expect((await f.service.getUsage("s")).contextWindowTokens).toBe(32768);
});

it("persists queued input context through settlement and repeated submissions", async () => {
  const f = await fixture({ windowTokens: 4000 });
  const first = await send(f, "first");
  await f.service.configure("s", { context: { windowTokens: 8000 } });
  const request = { inputId: "queued", content: "second" };
  await f.service.followUp("s", request);
  await f.service.configure("s", { context: { windowTokens: 16_000 } });
  await f.service.followUp("s", request);
  expect((await f.store.inputs?.getInput("queued"))?.content).toEqual({
    content: "second",
    sessionContext: { windowTokens: 8000 },
  });
  f.mock.setScript([{ steps: [{ kind: "text", content: "answer" }] }]);
  await drive(f, first);
  expect(
    (await f.store.tasks.getTask("turn:queued"))?.payload["sessionContext"],
  ).toEqual({ windowTokens: 8000 });
});

it("pairs active-turn usage with its admitted window ahead of newer queued turns", async () => {
  const f = await fixture({ windowTokens: 4000 });
  const active = await send(f, "first");
  await f.store.tasks.transitionTask(active, ["queued"], "running");
  await f.store.tasks.createTask({
    taskId: "newer",
    kind: "chat.turn",
    scopeId: "s",
    exclusiveScope: false,
    payload: { sessionContext: { windowTokens: 80_000 } },
  });
  const original = f.store.tasks.listEvents.bind(f.store.tasks);
  f.store.tasks.listEvents = async (id) =>
    id === active
      ? [
          {
            type: "run.usage",
            data: {
              finalForCall: true,
              promptTokens: 300,
              completionTokens: 10,
              totalTokens: 310,
            },
          } as unknown as TaskEventEnvelope,
        ]
      : original(id);
  expect(await f.service.getUsage("s")).toMatchObject({
    contextTurnId: active,
    contextWindowTokens: 4000,
    lastPromptTokens: 300,
    totals: { totalTokens: 310 },
  });
});

it("does not use a previous turn's input count for a new admitted turn", async () => {
  const f = await fixture({ windowTokens: 4000 });
  const id = await send(f, "first");
  f.mock.setScript([{ steps: [{ kind: "text", content: "answer" }] }]);
  await drive(f, id);
  const next = await send(f, "second", "second");
  expect(await f.service.getUsage("s")).toMatchObject({
    contextTurnId: next,
    contextWindowTokens: 4000,
  });
  expect((await f.service.getUsage("s")).lastPromptTokens).toBeUndefined();
});

it("reports a legacy window as unknown and rejects a malformed new snapshot", async () => {
  const f = await fixture({ windowTokens: 4000 });
  await f.store.tasks.createTask({
    taskId: "old",
    kind: "chat.turn",
    scopeId: "s",
    payload: {},
  });
  expect(await f.service.getUsage("s")).toMatchObject({
    contextWindowTokens: null,
    contextWindowSource: "unknown",
  });
  const { descriptor } = await f.service.getState("s");
  expect(() =>
    sessionWithAdmittedContext(descriptor, {
      sessionContext: { windowTokens: -1 },
    }),
  ).toThrow("unreadable");
});

it("reports the actual capability admission after later catalogue and settings changes", async () => {
  const f = await fixture();
  await f.store.providers.saveCapabilities("p", {
    ...(await f.mock.capabilities()),
    maxContextTokens: 6000,
  });
  f.mock.emitUsage = true;
  f.mock.setScript([{ steps: [{ kind: "text", content: "answer" }] }]);
  const id = await send(f, "short");
  await drive(f, id);
  await f.service.configure("s", { context: { windowTokens: 100_000 } });
  await f.store.providers.saveCapabilities("p", {
    ...(await f.mock.capabilities()),
    maxContextTokens: 12_000,
  });
  const usage = await f.service.getUsage("s");
  expect(usage).toMatchObject({
    contextWindowTokens: 6000,
    contextWindowSource: "capabilities",
    contextAdmission: { reserveTokens: 3000 },
  });
  expect(usage.lastPromptTokens).toBeDefined();
  expect(usage.contextAdmission?.estimatedInputTokens).not.toBe(
    usage.lastPromptTokens,
  );
});

it("bounds post-tool requests, preserves request sequencing and accepts a later short task", async () => {
  const contributor: ToolSetContributor = {
    namespace: "test",
    contribute: async () => [
      {
        definition: {
          name: "large_read",
          version: "1",
          description: "Read large text",
          effect: "read",
          capability: "read",
          inputSchema: { type: "object", properties: {} },
        },
        execute: async (ctx) => ({
          ok: true,
          data: { text: "x".repeat(48_000) },
          sources: [],
          warnings: [],
          truncated: false,
          limits: ctx.limits,
        }),
      },
    ],
  };
  const f = await fixture({ windowTokens: 3000 }, [contributor]);
  f.mock.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "large",
          name: "large_read",
          argumentsJson: "{}",
        },
      ],
      finishReason: "tool_calls",
    },
    { steps: [{ kind: "text", content: "must not be requested" }] },
  ]);
  const first = await send(f, "read");
  await drive(f, first);
  expect(
    (await f.store.tasks.listEvents(first)).find(
      (event) => event.type === "run.tool.failed",
    ),
  ).toBeUndefined();
  expect(f.mock.calls).toBe(1);
  expect((await f.store.tasks.getTask(first))?.status).toBe("failed");
  const events = await f.store.tasks.listEvents(first);
  expect(events.find((event) => event.type === "run.failed")).toMatchObject({
    data: { errorCode: "context_overflow" },
  });
  const admission = events.find(
    (event) => event.type === "run.context.admitted",
  );
  expect(admission).toMatchObject({
    attemptId: expect.any(String),
    data: {
      windowTokens: 3000,
      reserveTokens: 1500,
      estimatedInputTokens: expect.any(Number),
    },
  });
  expect(events.map((event) => event.seq)).toEqual(
    events.map((_, index) => index),
  );
  f.mock.setScript([{ steps: [{ kind: "text", content: "short answer" }] }]);
  const next = await send(f, "short follow-up", "next");
  await drive(f, next);
  expect(f.mock.calls).toBe(2);
  expect(
    f.mock.requests.every((request) => request.maxOutputTokens === 1500),
  ).toBe(true);
  expect((await f.store.tasks.getTask(next))?.status).toBe("completed");
  expect(await f.service.getUsage("s")).toMatchObject({
    contextTurnId: next,
    contextAdmission: { windowTokens: 3000 },
  });
});
