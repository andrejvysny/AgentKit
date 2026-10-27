/**
 * AgentSessionService (ADR 0016 §6) over the fake store: create/read, send and
 * its busy modes, follow-up conversion, flush, the human-input round trip with
 * its deny rules, cancel, fork and setModel.
 */
import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import { humanRequestId, type AiChatRequest } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  AgentSessionService,
  SESSION_METADATA_KEY,
  TaskService,
  TurnRunner,
  createUserInputContributor,
} from "../src/index.js";
import {
  FakeAgentInputStore,
  FakeTaskRunner,
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

/** Mirrors SingleProcessTaskRunner: a parked task is cancelled in the store. */
class ParkAwareRunner extends FakeTaskRunner {
  readonly calls: string[] = [];
  store: TestHarness["store"] | undefined;
  override async enqueue(
    input: Parameters<FakeTaskRunner["enqueue"]>[0],
  ): Promise<void> {
    this.calls.push(input.taskId);
    await super.enqueue(input);
  }
  override async requestCancel(taskId: string): Promise<void> {
    await super.requestCancel(taskId);
    await this.store?.tasks.transitionTask(
      taskId,
      ["waiting_approval", "queued"],
      "cancelled",
      { finishedAt: "2026-01-01T00:00:00.000Z" },
    );
  }
}

interface Fixture extends TestHarness {
  runner: TurnRunner;
  mock: SpyProvider;
  inputs: FakeAgentInputStore;
  service: AgentSessionService;
  parkRunner: ParkAwareRunner;
}

async function setup(): Promise<Fixture> {
  const harness = createHarness();
  const parkRunner = new ParkAwareRunner();
  parkRunner.store = harness.store;
  const mock = new SpyProvider();
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
  harness.store.inputs = inputs;
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: parkRunner,
    providerFactory: () => mock,
    contributors: [createUserInputContributor({ clock: harness.clock })],
    clock: harness.clock,
    ids: harness.ids,
  });
  const tasks = new TaskService({
    store: harness.store,
    taskRunner: parkRunner,
    ids: harness.ids,
    clock: harness.clock,
  });
  const service = new AgentSessionService({
    store: harness.store,
    turns: runner,
    tasks,
    taskRunner: parkRunner,
    clock: harness.clock,
    ids: harness.ids,
  });
  return { ...harness, runner, mock, inputs, service, parkRunner };
}

async function newSession(f: Fixture, sessionId = "s1") {
  return f.service.createSession({
    sessionId,
    profile: { id: "default" },
    providerId: "p1",
    model: "m-x",
  });
}

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

/** The task runner's claim: queued to running, then execute. */
async function claimAndDrive(f: Fixture, taskId: string): Promise<void> {
  await f.store.tasks.transitionTask(taskId, ["queued"], "running");
  await drive(f, taskId);
}

async function codeOf(promise: Promise<unknown>): Promise<string | undefined> {
  try {
    await promise;
  } catch (error) {
    return (error as { code?: string }).code;
  }
  return undefined;
}

async function turnOf(
  f: Fixture,
  sessionId: string,
  content: string,
  inputId?: string,
) {
  const result = await f.service.send(sessionId, {
    content,
    ...(inputId === undefined ? {} : { inputId }),
  });
  if (result.kind !== "turn") throw new Error("expected a turn");
  return result;
}

function askScript(f: Fixture, args = '{"prompt":"Pick one"}'): void {
  f.mock.setScript([
    {
      steps: [
        {
          kind: "tool_call",
          toolCallId: "call-1",
          name: "request_user_input",
          argumentsJson: args,
        },
      ],
    },
    { steps: [{ kind: "text", content: "done" }] },
  ]);
}

function message(f: Fixture, id: string) {
  const record = f.store.conversations.messages.find((m) => m.id === id);
  if (!record) throw new Error(`no message ${id}`);
  return record;
}

describe("createSession and getState", () => {
  it("writes a validated descriptor and reports empty state", async () => {
    const f = await setup();
    const state = await newSession(f);
    expect(state.sessionId).toBe("s1");
    expect(state.descriptor).toMatchObject({
      version: 1,
      profile: { id: "default" },
      providerId: "p1",
      model: "m-x",
    });
    expect(state.pending).toEqual([]);
    expect(state.activeTurn).toBeUndefined();
    expect(state.waiting).toBeUndefined();
  });

  it("refuses reserved metadata, a blank profile and non-sessions", async () => {
    const f = await setup();
    expect(
      await codeOf(
        f.service.createSession({
          profile: { id: "x" },
          metadata: { [SESSION_METADATA_KEY]: {} },
        }),
      ),
    ).toBe("invalid_input");
    expect(await codeOf(f.service.createSession({ profile: { id: "" } }))).toBe(
      "invalid_input",
    );
    await f.store.conversations.createChat({ id: "plain" });
    expect(await codeOf(f.service.getState("plain"))).toBe("session_not_found");
    expect(await codeOf(f.service.getState("nope"))).toBe("session_not_found");
  });

  it("refuses a store without inputs", async () => {
    const f = await setup();
    delete f.store.inputs;
    let code: string | undefined;
    try {
      new AgentSessionService({
        store: f.store,
        turns: f.runner,
        tasks: new TaskService({
          store: f.store,
          taskRunner: f.parkRunner,
          ids: f.ids,
          clock: f.clock,
        }),
        taskRunner: f.parkRunner,
        clock: f.clock,
        ids: f.ids,
      });
    } catch (error) {
      code = (error as { code?: string }).code;
    }
    expect(code).toBe("sessions_unavailable");
  });
});

describe("send", () => {
  it("starts a turn with the descriptor model, idempotent on inputId", async () => {
    const f = await setup();
    await newSession(f);
    const first = await turnOf(f, "s1", "hello", "in-1");
    expect(first.turnId).toBe("turn:in-1");
    const task = await f.store.tasks.getTask(first.turnId);
    expect(task?.payload).toMatchObject({ model: "m-x", providerId: "p1" });
    const again = await turnOf(f, "s1", "hello", "in-1");
    expect(again.turnId).toBe(first.turnId);
    expect((await f.service.getState("s1")).activeTurn).toEqual({
      turnId: first.turnId,
      status: "queued",
    });
  });

  it("rejects when busy by default, queues for follow_up and steer", async () => {
    const f = await setup();
    await newSession(f);
    const turn = await turnOf(f, "s1", "one");
    expect(await codeOf(f.service.send("s1", { content: "two" }))).toBe(
      "chat_busy",
    );
    expect(
      await codeOf(
        f.service.send("s1", { content: "two", whenBusy: "follow_up" }),
      ),
    ).toBe("invalid_input");

    const queued = await f.service.send("s1", {
      content: "two",
      inputId: "fu-1",
      whenBusy: "follow_up",
    });
    expect(queued.kind === "queued" && queued.input).toMatchObject({
      kind: "follow_up",
      status: "pending",
      content: { content: "two" },
    });
    const steered = await f.service.send("s1", {
      content: "adjust",
      inputId: "st-1",
      whenBusy: "steer",
    });
    expect(steered.kind === "queued" && steered.input).toMatchObject({
      kind: "steer",
      targetTaskId: turn.turnId,
    });
    expect((await f.service.getState("s1")).pending).toHaveLength(2);
  });

  it("replays a queued input and refuses a different one under the same id", async () => {
    const f = await setup();
    await newSession(f);
    await turnOf(f, "s1", "one");
    const a = await f.service.followUp("s1", { inputId: "fu-1", content: "x" });
    const b = await f.service.followUp("s1", { inputId: "fu-1", content: "x" });
    expect(b).toEqual(a);
    expect(
      await codeOf(f.service.followUp("s1", { inputId: "fu-1", content: "y" })),
    ).toBe("input_conflict");
  });

  it("starts a turn when a steer or follow-up arrives on an idle session", async () => {
    const f = await setup();
    await newSession(f);
    const steer = await f.service.steer("s1", { inputId: "st", content: "go" });
    expect(steer.kind).toBe("turn");
  });
});

describe("follow-up conversion", () => {
  it("runs the queued follow-up as the next turn once the first completes", async () => {
    const f = await setup();
    await newSession(f);
    f.mock.setScript([
      { steps: [{ kind: "text", content: "first" }] },
      { steps: [{ kind: "text", content: "second" }] },
    ]);
    const first = await turnOf(f, "s1", "q1");
    const queued = await f.service.followUp("s1", {
      inputId: "fu-1",
      content: "q2",
    });
    expect(queued.kind).toBe("queued");

    await drive(f, first.turnId);
    const state = await f.service.getState("s1");
    expect(state.pending).toEqual([]);
    const next = state.activeTurn;
    expect(next?.status).toBe("queued");
    expect(next?.turnId).not.toBe(first.turnId);

    // A retry of the same inputId now reports the turn it became.
    const replay = await f.service.followUp("s1", {
      inputId: "fu-1",
      content: "q2",
    });
    expect(replay).toMatchObject({ kind: "turn", turnId: next?.turnId });

    await claimAndDrive(f, next!.turnId);
    const done = await f.store.tasks.getTask(next!.turnId);
    expect(done?.status).toBe("completed");
    const payload = done?.payload as { assistantMessageId: string };
    expect(message(f, payload.assistantMessageId).content).toBe("second");
  });
});

describe("flushQueue", () => {
  it("converts the queued follow-up after a failed turn", async () => {
    const f = await setup();
    await newSession(f);
    f.mock.failing = true;
    const first = await turnOf(f, "s1", "q1");
    await f.service.followUp("s1", { inputId: "fu-1", content: "q2" });
    await drive(f, first.turnId).catch(() => undefined);
    expect((await f.store.tasks.getTask(first.turnId))?.status).toBe("failed");
    expect((await f.service.getState("s1")).pending).toHaveLength(1);

    const flushed = await f.service.flushQueue("s1");
    expect(flushed?.kind).toBe("turn");
    if (flushed?.kind !== "turn") return;
    expect(f.parkRunner.calls).toContain(flushed.turnId);
    expect(message(f, flushed.userMessageId).content).toBe("q2");
    expect(await f.inputs.getInput("fu-1")).toMatchObject({
      status: "converted",
      appliedTaskId: flushed.turnId,
    });
    expect(await f.service.flushQueue("s1")).toBeUndefined();
  });

  it("returns undefined for an empty queue and chat_busy while busy", async () => {
    const f = await setup();
    await newSession(f);
    expect(await f.service.flushQueue("s1")).toBeUndefined();
    await turnOf(f, "s1", "q1");
    await f.service.followUp("s1", { inputId: "fu-1", content: "q2" });
    expect(await codeOf(f.service.flushQueue("s1"))).toBe("chat_busy");
    expect((await f.inputs.getInput("fu-1"))?.status).toBe("pending");
  });
});

describe("human input", () => {
  async function parked(f: Fixture, args?: string) {
    await newSession(f);
    askScript(f, args);
    const turn = await turnOf(f, "s1", "ask me");
    await drive(f, turn.turnId);
    return turn;
  }

  it("parks, takes the answer, resumes and completes", async () => {
    const f = await setup();
    const turn = await parked(f);
    const requestId = humanRequestId(turn.turnId, "call-1");

    const state = await f.service.getState("s1");
    expect(state.activeTurn?.status).toBe("waiting_approval");
    expect(state.waiting?.inputId).toBe(requestId);
    expect(state.pending).toEqual([]);

    const enqueues = f.parkRunner.calls.length;
    const receipt = await f.service.answer({
      requestId,
      answerId: "a1",
      response: { choice: "b" },
    });
    expect(receipt).toMatchObject({
      requestId,
      answerId: "a1",
      turnId: turn.turnId,
      duplicate: false,
    });
    expect(f.parkRunner.calls.length).toBe(enqueues + 1);
    expect((await f.store.tasks.getTask(turn.turnId))?.status).toBe("queued");
    expect(await f.inputs.getInput(requestId)).toMatchObject({
      status: "answered",
      answerId: "a1",
      appliedMessageId: receipt.messageId,
    });

    await claimAndDrive(f, turn.turnId);
    const resumed = f.mock.requests.at(-1) ?? [];
    const toolMessage = resumed.at(-1);
    expect(toolMessage?.role).toBe("tool");
    expect(JSON.parse(String(toolMessage?.content))).toMatchObject({
      ok: true,
      data: { answer: { choice: "b" } },
    });
    expect((await f.store.tasks.getTask(turn.turnId))?.status).toBe(
      "completed",
    );
    const task = await f.store.tasks.getTask(turn.turnId);
    const placeholder = message(
      f,
      (task!.payload as { assistantMessageId: string }).assistantMessageId,
    );
    expect(placeholder.content).toBe("done");
    expect(placeholder.metadata["placeholder"]).toBe(false);
  });

  it("returns the original receipt for a repeat and refuses a different answer", async () => {
    const f = await setup();
    const turn = await parked(f);
    const requestId = humanRequestId(turn.turnId, "call-1");
    const first = await f.service.answer({
      requestId,
      answerId: "a1",
      response: { choice: "b" },
    });
    const again = await f.service.answer({
      requestId,
      answerId: "a1",
      response: { choice: "b" },
    });
    expect(again).toEqual({ ...first, duplicate: true });
    expect(
      await codeOf(
        f.service.answer({
          requestId,
          answerId: "a1",
          response: { choice: "c" },
        }),
      ),
    ).toBe("answer_conflict");
    expect(
      await codeOf(
        f.service.answer({
          requestId,
          answerId: "a2",
          response: { choice: "b" },
        }),
      ),
    ).toBe("answer_conflict");
    expect(
      await codeOf(
        f.service.answer({
          requestId: "hreq:none",
          answerId: "a",
          response: 1,
        }),
      ),
    ).toBe("not_found");
  });

  it("refuses an expired request", async () => {
    const f = await setup();
    const turn = await parked(f, '{"prompt":"Pick","expiresInMs":1000}');
    const requestId = humanRequestId(turn.turnId, "call-1");
    f.clock.advance(5000);
    expect(
      await codeOf(
        f.service.answer({ requestId, answerId: "a1", response: "late" }),
      ),
    ).toBe("answer_stale");
    expect((await f.store.tasks.getTask(turn.turnId))?.status).toBe(
      "waiting_approval",
    );
  });

  it("cancels a waiting turn and its request, then refuses the answer", async () => {
    const f = await setup();
    const turn = await parked(f);
    const requestId = humanRequestId(turn.turnId, "call-1");
    await f.service.cancel("s1", turn.turnId);
    expect((await f.store.tasks.getTask(turn.turnId))?.status).toBe(
      "cancelled",
    );
    expect((await f.inputs.getInput(requestId))?.status).toBe("cancelled");
    expect(
      await codeOf(
        f.service.answer({ requestId, answerId: "a1", response: "x" }),
      ),
    ).toBe("answer_stale");
    expect((await f.service.getState("s1")).waiting).toBeUndefined();
  });

  it("refuses to cancel a turn of another session", async () => {
    const f = await setup();
    const turn = await parked(f);
    await newSession(f, "s2");
    expect(await codeOf(f.service.cancel("s2", turn.turnId))).toBe("not_found");
  });
});

describe("setModel reasoning effort", () => {
  it("pins effort into later turns and null clears it", async () => {
    const f = await setup();
    await newSession(f);
    f.mock.setScript([{ steps: [{ kind: "text", content: "a" }] }]);
    const state = await f.service.setModel("s1", {
      model: "m-y",
      reasoningEffort: "high",
    });
    expect(state.descriptor.reasoningEffort).toBe("high");
    const first = await turnOf(f, "s1", "q1");
    expect(
      (await f.store.tasks.getTask(first.turnId))?.payload["reasoningEffort"],
    ).toBe("high");
    await drive(f, first.turnId);

    // Omitted keeps it; null clears it.
    const kept = await f.service.setModel("s1", { model: "m-y" });
    expect(kept.descriptor.reasoningEffort).toBe("high");
    const cleared = await f.service.setModel("s1", {
      model: "m-y",
      reasoningEffort: null,
    });
    expect(cleared.descriptor.reasoningEffort).toBeUndefined();
    f.mock.setScript([{ steps: [{ kind: "text", content: "b" }] }]);
    const second = await turnOf(f, "s1", "q2");
    expect(
      (await f.store.tasks.getTask(second.turnId))?.payload,
    ).not.toHaveProperty("reasoningEffort");
    expect(
      await codeOf(
        f.service.setModel("s1", {
          model: "m-y",
          reasoningEffort: "extreme" as never,
        }),
      ),
    ).toBe("invalid_input");
  });
});

describe("fork and setModel", () => {
  it("forks history without pending inputs", async () => {
    const f = await setup();
    await newSession(f);
    f.mock.setScript([{ steps: [{ kind: "text", content: "first" }] }]);
    const turn = await turnOf(f, "s1", "q1");
    await drive(f, turn.turnId);
    await f.inputs.createInput({
      inputId: "fu-src",
      sessionId: "s1",
      kind: "follow_up",
      content: { content: "later" },
      contentHash: "h",
    });

    const forked = await f.service.fork("s1", turn.assistantMessageId);
    expect(forked.sessionId).not.toBe("s1");
    expect(forked.pending).toEqual([]);
    expect(forked.descriptor).toMatchObject({
      profile: { id: "default" },
      model: "m-x",
    });
    const chat = await f.store.conversations.getChat(forked.sessionId);
    expect(
      (chat!.metadata[SESSION_METADATA_KEY] as { forkedFrom: unknown })
        .forkedFrom,
    ).toEqual({ sessionId: "s1", messageId: turn.assistantMessageId });
    const history = await f.store.conversations.listMessages(forked.sessionId);
    expect(history.map((m) => m.content)).toEqual(["q1", "first"]);
    expect((await f.service.getState("s1")).pending).toHaveLength(1);
  });

  it("applies a new model to future turns only", async () => {
    const f = await setup();
    await newSession(f);
    f.mock.setScript([{ steps: [{ kind: "text", content: "first" }] }]);
    const first = await turnOf(f, "s1", "q1");
    const state = await f.service.setModel("s1", { model: "m-y" });
    expect(state.descriptor).toMatchObject({ model: "m-y", providerId: "p1" });
    expect((await f.store.tasks.getTask(first.turnId))?.payload["model"]).toBe(
      "m-x",
    );

    await drive(f, first.turnId);
    const second = await turnOf(f, "s1", "q2");
    expect((await f.store.tasks.getTask(second.turnId))?.payload["model"]).toBe(
      "m-y",
    );
    expect(await codeOf(f.service.setModel("s1", { model: "" }))).toBe(
      "invalid_input",
    );
  });
});
