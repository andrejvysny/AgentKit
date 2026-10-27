/**
 * Session-mode turns (ADR 0016 §2-§5): the TurnRunner wires core's steering and
 * suspend seams to the durable `AgentInputStore`, parks a suspended task, and
 * converts a queued follow-up when a turn completes. A chat without a session
 * descriptor must behave exactly as before.
 */
import { describe, expect, it } from "bun:test";
import { humanRequestId, type AiChatRequest } from "@agentkit/core";
import type { AiRunEvent } from "@agentkit/contracts";
import type { AiTool } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  SESSION_METADATA_KEY,
  TurnRunner,
  createRunProjector,
  type CreateAgentInput,
  type MessageRecord,
  type ToolSetContributor,
} from "../src/index.js";
import { assembleMessages } from "../src/turn/history-assembly.js";
import {
  FakeAgentInputStore,
  createHarness,
  type TestHarness,
} from "./fakes.js";

class SpyProvider extends MockProviderClient {
  readonly requests: AiChatRequest["messages"][] = [];
  failing = false;
  override streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.requests.push(structuredClone(input.messages));
    if (this.failing) throw new Error("provider down");
    return super.streamChat(input);
  }
}

interface Fixture extends TestHarness {
  runner: TurnRunner;
  mock: SpyProvider;
  chatId: string;
  inputs: FakeAgentInputStore;
  executed: string[];
}

async function setup(
  options: { session?: boolean; withInputs?: boolean } = {},
): Promise<Fixture> {
  const { session = true, withInputs = true } = options;
  const harness = createHarness();
  const mock = new SpyProvider();
  const executed: string[] = [];
  const result = (ctx: { limits: never }) => ({
    ok: true as const,
    data: {},
    sources: [],
    warnings: [],
    truncated: false,
    limits: ctx.limits,
  });
  const echo: AiTool<{ text?: string }, Record<string, never>> = {
    definition: {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "Echo the input.",
      inputSchema: { type: "object", properties: { text: { type: "string" } } },
    },
    async execute(ctx, input) {
      executed.push(input.text ?? "");
      return result(ctx as never);
    },
  };
  const ask: AiTool<Record<string, never>, Record<string, never>> = {
    definition: {
      name: "ask",
      version: "1.0.0",
      effect: "read",
      capability: "ask",
      description: "Ask a human.",
      inputSchema: { type: "object", properties: {} },
    },
    async execute(ctx) {
      ctx.suspend?.({ prompt: "Pick one" });
      return result(ctx as never);
    },
  };
  const contributor: ToolSetContributor = {
    namespace: "test",
    contribute: async () => [echo as AiTool, ask as AiTool],
  };
  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const inputs = new FakeAgentInputStore(harness.clock);
  if (withInputs) harness.store.inputs = inputs;
  const chat = await harness.store.conversations.createChat({
    id: "chat-1",
    ...(session
      ? {
          metadata: {
            [SESSION_METADATA_KEY]: {
              version: 1,
              profile: { id: "default" },
              providerId: "p1",
              model: "m-session",
              createdAt: "2026-01-01T00:00:00.000Z",
            },
          },
        }
      : {}),
  });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => mock,
    contributors: [contributor],
    clock: harness.clock,
    ids: harness.ids,
  });
  return { ...harness, runner, mock, chatId: chat.id, inputs, executed };
}

/** Create the attempt, take the lease and execute — the task runner's job. */
async function drive(f: Fixture, taskId: string): Promise<void> {
  const attempt = await f.store.tasks.createAttempt({
    attemptId: f.ids.attemptId(),
    taskId,
    ownerId: "worker-1",
  });
  const lease = await f.store.tasks.acquireLease({
    taskId,
    attemptId: attempt.attemptId,
    ownerId: "worker-1",
    ttlMs: 60_000,
  });
  await f.runner.execute({
    taskId,
    attemptId: attempt.attemptId,
    leaseToken: lease.leaseToken,
    signal: new AbortController().signal,
  });
}

/** The task runner's side of an answered request: back to queued, then claimed. */
async function requeueAndClaim(f: Fixture, taskId: string): Promise<void> {
  await f.store.tasks.transitionTask(taskId, ["waiting_approval"], "queued");
  await f.store.tasks.transitionTask(taskId, ["queued"], "running");
}

async function submit(f: Fixture, content = "go") {
  return f.runner.submitMessage({ chatId: f.chatId, content });
}

async function eventsOf(f: Fixture, runId: string): Promise<AiRunEvent[]> {
  return (await f.store.tasks.listEvents(runId)) as AiRunEvent[];
}

function expectContiguous(events: AiRunEvent[]): void {
  expect(events.map((e) => e.seq)).toEqual(events.map((_, index) => index));
}

function steerInput(
  f: Fixture,
  inputId: string,
  overrides: Partial<CreateAgentInput> = {},
): Promise<unknown> {
  return f.inputs.createInput({
    inputId,
    sessionId: f.chatId,
    kind: "steer",
    content: { content: `text of ${inputId}` },
    contentHash: `hash-${inputId}`,
    ...overrides,
  });
}

function oneToolThenAnswer(f: Fixture): void {
  f.mock.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-1",
          name: "echo",
          argumentsJson: '{"text":"hi"}',
        },
      ],
    },
    { steps: [{ kind: "text", content: "understood" }] },
  ]);
}

function suspendScript(f: Fixture): void {
  f.mock.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-1",
          name: "echo",
          argumentsJson: '{"text":"hi"}',
        },
        {
          kind: "tool_call",
          toolCallId: "call-2",
          name: "ask",
          argumentsJson: "{}",
        },
      ],
    },
    { steps: [{ kind: "text", content: "done" }] },
  ]);
}

function placeholderOf(f: Fixture, id: string): MessageRecord {
  const record = f.store.conversations.messages.find((m) => m.id === id);
  if (!record) throw new Error(`no message ${id}`);
  return record;
}

describe("session steering", () => {
  it("places targeted and untargeted steers after the tool batch, exactly once", async () => {
    const f = await setup();
    oneToolThenAnswer(f);
    const submitted = await submit(f);
    const { runId } = submitted;
    await steerInput(f, "s-open");
    await steerInput(f, "s-target", { targetTaskId: runId });
    await steerInput(f, "s-other", { targetTaskId: "some-other-task" });
    await drive(f, runId);

    const events = await eventsOf(f, runId);
    const types = events.map((e) => e.type);
    const toolDone = types.indexOf("run.tool.succeeded");
    const applied = events.filter((e) => e.type === "run.input.applied");
    expect(
      applied.map((e) => e.type === "run.input.applied" && e.data.inputId),
    ).toEqual(["s-open", "s-target"]);
    expect(types.indexOf("run.input.applied")).toBeGreaterThan(toolDone);
    expectContiguous(events);

    const records = f.store.conversations.messages.filter(
      (m) => m.metadata["steering"] === true,
    );
    expect(records.map((m) => m.metadata["inputId"])).toEqual([
      "s-open",
      "s-target",
    ]);
    expect(records.map((m) => m.content)).toEqual([
      "text of s-open",
      "text of s-target",
    ]);
    expect(records[0]?.metadata["internal"]).toBe(true);
    expect(records[0]?.runId).toBe(runId);
    // Chained: the first steer descends from the tool result, the second from
    // the first.
    const tool = f.store.conversations.messages.find((m) => m.role === "tool");
    expect(records[0]?.parentMessageId).toBe(tool?.id);
    expect(records[1]?.parentMessageId).toBe(records[0]?.id);

    for (const [inputId, record] of [
      ["s-open", records[0]],
      ["s-target", records[1]],
    ] as const) {
      const input = await f.inputs.getInput(inputId);
      expect(input?.status).toBe("applied");
      expect(input?.appliedTaskId).toBe(runId);
      expect(input?.appliedMessageId).toBe(record?.id);
    }
    expect((await f.inputs.getInput("s-other"))?.status).toBe("pending");

    // The model saw the steers: the second provider request ends with them.
    const second = f.mock.requests[1] ?? [];
    expect(second.slice(-2).map((m) => m.content)).toEqual([
      "text of s-open",
      "text of s-target",
    ]);
  });

  it("replaying the log creates no duplicate record and keeps provider order", async () => {
    const f = await setup();
    f.mock.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "call-1",
            name: "echo",
            argumentsJson: '{"text":"a"}',
          },
        ],
      },
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "call-2",
            name: "echo",
            argumentsJson: '{"text":"b"}',
          },
        ],
      },
      { steps: [{ kind: "text", content: "understood" }] },
    ]);
    const { runId, assistantMessageId } = await submit(f);
    await steerInput(f, "s-open");
    await drive(f, runId);
    const before = f.store.conversations.messages.length;

    const task = (await f.store.tasks.getTask(runId))!;
    const lease = f.store.tasks.leases.get(runId)!;
    const projector = createRunProjector({ store: f.store, clock: f.clock });
    const state = projector.createState({
      chatId: f.chatId,
      assistantMessageId,
    });
    await projector.replay(
      { task, attemptId: lease.attemptId, leaseToken: lease.leaseToken },
      state,
    );
    expect(f.store.conversations.messages.length).toBe(before);
    expect((await f.inputs.getInput("s-open"))?.status).toBe("applied");

    // What a second attempt would assemble: calls, results, then the steer.
    const assembled = await assembleMessages(
      { store: f.store } as never,
      f.chatId,
      assistantMessageId,
      null,
    );
    expect(assembled.map((m) => m.role)).toEqual([
      "user",
      "assistant",
      "tool",
      "user",
      "assistant",
      "tool",
    ]);
    expect(assembled[3]?.content).toBe("text of s-open");
  });
});

describe("session suspension", () => {
  it("parks the task on a durable human request", async () => {
    const f = await setup();
    suspendScript(f);
    const { runId, assistantMessageId } = await submit(f);
    await drive(f, runId);

    const task = await f.store.tasks.getTask(runId);
    expect(task?.status).toBe("waiting_approval");
    expect(task?.finishedAt).toBeUndefined();
    expect([...f.store.tasks.attempts.values()][0]?.status).toBe("completed");

    const events = await eventsOf(f, runId);
    expect(events.at(-1)?.type).toBe("run.waiting");
    expect(events.map((e) => e.type)).not.toContain("run.completed");
    expectContiguous(events);

    const requests = await f.inputs.listInputs({
      sessionId: f.chatId,
      kinds: ["human_request"],
    });
    expect(requests).toHaveLength(1);
    expect(requests[0]).toMatchObject({
      inputId: humanRequestId(runId, "call-2"),
      status: "pending",
      targetTaskId: runId,
      toolCallId: "call-2",
      content: { prompt: "Pick one", toolName: "ask" },
    });
    expect(placeholderOf(f, assistantMessageId).metadata["placeholder"]).toBe(
      true,
    );
    // Only the call that finished has a tool record.
    expect(
      f.store.conversations.messages
        .filter((m) => m.role === "tool")
        .map((m) => m.toolCallId),
    ).toEqual(["call-1"]);
  });

  it("re-parks without a provider call while the request is unanswered", async () => {
    const f = await setup();
    suspendScript(f);
    const { runId } = await submit(f);
    await drive(f, runId);
    const calls = f.mock.callCount;
    const eventCount = (await eventsOf(f, runId)).length;

    await requeueAndClaim(f, runId);
    await drive(f, runId);

    expect(f.mock.callCount).toBe(calls);
    expect((await f.store.tasks.getTask(runId))?.status).toBe(
      "waiting_approval",
    );
    expect(await eventsOf(f, runId)).toHaveLength(eventCount);
  });

  it("continues once the request is answered", async () => {
    const f = await setup();
    suspendScript(f);
    const { runId, assistantMessageId } = await submit(f);
    await drive(f, runId);

    const requestId = humanRequestId(runId, "call-2");
    const last = await f.store.conversations.lastMessageOfRun(f.chatId, runId);
    await f.store.conversations.appendMessage({
      chatId: f.chatId,
      runId,
      role: "tool",
      content: '{"answer":"b"}',
      toolCallId: "call-2",
      parentMessageId: last!.id,
      activate: false,
      metadata: { internal: true },
    });
    await f.inputs.transitionInput(requestId, ["pending"], "answered", {
      answerId: "ans-1",
    });
    await requeueAndClaim(f, runId);
    await drive(f, runId);

    const resumed = f.mock.requests.at(-1) ?? [];
    expect(resumed.map((m) => m.role)).toEqual([
      "user",
      "assistant",
      "tool",
      "tool",
    ]);
    expect(resumed[3]?.content).toBe('{"answer":"b"}');
    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
    expect(placeholderOf(f, assistantMessageId).content).toBe("done");
    expect(placeholderOf(f, assistantMessageId).metadata["placeholder"]).toBe(
      false,
    );
    const events = await eventsOf(f, runId);
    expectContiguous(events);
  });
});

describe("session settlement", () => {
  async function followUp(f: Fixture, inputId: string, text: string) {
    await f.inputs.createInput({
      inputId,
      sessionId: f.chatId,
      kind: "follow_up",
      content: { content: text },
      contentHash: `hash-${inputId}`,
    });
  }

  it("converts the oldest queued follow-up into the next turn", async () => {
    const f = await setup();
    f.mock.setScript([{ steps: [{ kind: "text", content: "first" }] }]);
    const { runId, assistantMessageId } = await submit(f);
    await followUp(f, "fu-1", "second question");
    await followUp(f, "fu-2", "third question");
    await drive(f, runId);

    expect((await f.store.tasks.getTask(runId))?.status).toBe("completed");
    const turns = [...f.store.tasks.tasks.values()].filter(
      (t) => t.taskId !== runId,
    );
    expect(turns).toHaveLength(1);
    const next = turns[0]!;
    expect(next).toMatchObject({
      kind: "chat.turn",
      status: "queued",
      scopeId: f.chatId,
    });
    expect(f.taskRunner.enqueued.some((e) => e.taskId === next.taskId)).toBe(
      true,
    );

    const payload = next.payload as {
      userMessageId: string;
      assistantMessageId: string;
      model?: string;
      providerId?: string;
    };
    expect(payload.model).toBe("m-session");
    expect(payload.providerId).toBe("p1");
    const user = placeholderOf(f, payload.userMessageId);
    expect(user.content).toBe("second question");
    expect(user.parentMessageId).toBe(assistantMessageId);
    expect(placeholderOf(f, payload.assistantMessageId).parentMessageId).toBe(
      user.id,
    );

    expect(await f.inputs.getInput("fu-1")).toMatchObject({
      status: "converted",
      appliedTaskId: next.taskId,
    });
    expect((await f.inputs.getInput("fu-2"))?.status).toBe("pending");
  });

  it("cancels cancel-policy leftover steers and queues the rest", async () => {
    const f = await setup();
    f.mock.setScript([{ steps: [{ kind: "text", content: "first" }] }]);
    const { runId } = await submit(f);
    await steerInput(f, "s-cancel", { onSettled: "cancel" });
    await steerInput(f, "s-keep", { onSettled: "follow_up" });
    await drive(f, runId);

    expect((await f.inputs.getInput("s-cancel"))?.status).toBe("cancelled");
    expect(await f.inputs.getInput("s-keep")).toMatchObject({
      status: "converted",
    });
  });

  it("converts nothing when the turn fails, but still cancels cancel-policy steers", async () => {
    const f = await setup();
    f.mock.failing = true;
    const { runId } = await submit(f);
    await followUp(f, "fu-1", "second question");
    await steerInput(f, "s-cancel", { onSettled: "cancel" });
    await steerInput(f, "s-keep");
    await drive(f, runId).catch(() => undefined);

    expect((await f.store.tasks.getTask(runId))?.status).toBe("failed");
    expect([...f.store.tasks.tasks.keys()]).toEqual([runId]);
    expect((await f.inputs.getInput("fu-1"))?.status).toBe("pending");
    expect((await f.inputs.getInput("s-keep"))?.status).toBe("pending");
    expect((await f.inputs.getInput("s-cancel"))?.status).toBe("cancelled");
  });
});

describe("non-session chats", () => {
  it("never polls inputs and logs the same events as a store without inputs", async () => {
    const withPort = await setup({ session: false });
    oneToolThenAnswer(withPort);
    const a = await submit(withPort);
    await withPort.inputs.createInput({
      inputId: "fu",
      sessionId: withPort.chatId,
      kind: "follow_up",
      content: { content: "ignored" },
      contentHash: "h",
    });
    await steerInput(withPort, "s");
    await drive(withPort, a.runId);
    expect(withPort.inputs.listCalls).toBe(0);
    expect((await withPort.inputs.getInput("s"))?.status).toBe("pending");
    expect(withPort.store.tasks.tasks.size).toBe(1);

    const without = await setup({ session: false, withInputs: false });
    oneToolThenAnswer(without);
    const b = await submit(without);
    await drive(without, b.runId);
    // Event ids are random and timestamps are wall-clock; the rest must match.
    const shape = (events: AiRunEvent[]) =>
      events.map(({ eventId: _id, timestamp: _at, ...rest }) => rest);
    expect(shape(await eventsOf(withPort, a.runId))).toEqual(
      shape(await eventsOf(without, b.runId)),
    );
  });
});
