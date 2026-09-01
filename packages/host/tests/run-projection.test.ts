/**
 * The projection seam, driven by an executor that is NOT `ChatTurnExecutor`.
 *
 * The point of every test here is a negative one: a host that registers its own
 * task kind — a chat turn delegated to a server, a replay, a bridge to a
 * provider this package has no client for — and maps whatever it receives into
 * `AiRunEvent`s must end up with a conversation indistinguishable from the one
 * `chat.turn` writes. Not "similar": the same placeholder filled the same way,
 * the same internal records chained off the run's own writes with
 * `activate: false`, the same gapless `seq`, and the same immunity to a user
 * switching branches mid-run.
 *
 * Nothing here touches `TurnRunner.executeTask`. The turn is submitted through
 * `submitMessage({ kind })`, dispatched by `createDispatchingWorker` on that
 * kind, and executed by the fixture's own executor below.
 */
import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import {
  createEventStamper,
  type AiRunEventDraft,
  type AiTool,
} from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  CHAT_TURN_TASK_KIND,
  ExecutorNotFoundError,
  ExecutorRegistry,
  TurnRunner,
  createDispatchingWorker,
  createRunEventFeed,
  createRunProjector,
  type MessageRecord,
  type RunProjectionState,
  type TaskExecutionContext,
  type TaskExecutor,
  type UsageAuthorizationDecision,
  type UsageAuthorizer,
  type UsageRecord,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/** The kind the host owns in these tests — anything outside `chat.*`. */
const CUSTOM_TURN_KIND = "custom.turn";

/** What a `chat.turn` payload carries, and what this executor reads too. */
interface TurnPayload {
  chatId: string;
  assistantMessageId: string;
}

/**
 * A `UsageAuthorizer` that says yes and remembers everything it was told.
 * `authorize` is never reached here — the projector only records — but the port
 * has two methods and a half-implemented fake would not typecheck.
 */
class RecordingUsage implements UsageAuthorizer {
  readonly recorded: UsageRecord[] = [];
  async authorize(): Promise<UsageAuthorizationDecision> {
    return { allowed: true };
  }
  async record(usage: UsageRecord): Promise<void> {
    this.recorded.push(usage);
  }
}

interface ProjectingExecutorOptions {
  harness: TestHarness;
  /** The drafts this "provider" produces, in order. */
  drafts: readonly AiRunEventDraft[];
  usage?: UsageAuthorizer;
  /** The id billed for `run.usage`; `undefined` exercises the unnamed case. */
  providerId?: string;
  /** Runs after the draft at this index has been projected. */
  after?: { index: number; run: () => Promise<void> };
  /** Feed the drafts through `createRunEventFeed` instead of stamping them. */
  useFeed?: boolean;
}

/**
 * The host-written executor under test: it produces events from nowhere in
 * particular and drives `RunProjector` with them.
 *
 * It owns exactly the three things the seam deliberately leaves to a host —
 * where the events come from, finalizing the placeholder, and settling the task
 * — and nothing else. The state that decides what goes in the placeholder is
 * the projector's, not a tally of its own.
 */
class ProjectingExecutor implements TaskExecutor {
  readonly kind = CUSTOM_TURN_KIND;
  /** The state of the last run, for assertions about what the seam accumulated. */
  lastState: RunProjectionState | undefined;

  constructor(private readonly options: ProjectingExecutorOptions) {}

  async execute(ctx: TaskExecutionContext): Promise<void> {
    const { harness, drafts } = this.options;
    const { store, clock, ids } = harness;
    const payload = ctx.task.payload as unknown as TurnPayload;
    const projector = createRunProjector({
      store,
      clock,
      ...(this.options.usage === undefined
        ? {}
        : { usage: this.options.usage }),
    });
    const state = projector.createState({
      chatId: payload.chatId,
      assistantMessageId: payload.assistantMessageId,
      ...(this.options.providerId === undefined
        ? {}
        : { providerId: this.options.providerId }),
    });
    this.lastState = state;

    const projectionCtx = {
      task: ctx.task,
      attemptId: ctx.attemptId,
      leaseToken: ctx.leaseToken,
    };
    const feed = this.options.useFeed
      ? createRunEventFeed({
          projector,
          ctx: projectionCtx,
          state,
          tasks: store.tasks,
          clock,
          ids,
        })
      : undefined;
    // The numbering is the producer's, exactly as it is `runChat`'s on the
    // `chat.turn` path: one stamper for the whole run, seeded from the log.
    const stamp = createEventStamper({
      firstSeq: await store.tasks.nextSeq(ctx.task.taskId),
      attemptId: ctx.attemptId,
    });

    for (const [index, draft] of drafts.entries()) {
      if (feed !== undefined) {
        await feed.emit(draft);
      } else {
        await projector.project(projectionCtx, state, stamp(draft));
      }
      const after = this.options.after;
      if (after !== undefined && after.index === index) await after.run();
    }

    // The host's own bookkeeping, deliberately outside the seam.
    await store.conversations.updateMessage(payload.assistantMessageId, {
      content: state.content,
      metadata: { placeholder: false },
    });
    await store.tasks.transitionTask(
      ctx.task.taskId,
      ["running"],
      "completed",
      {
        finishedAt: clock.nowIso(),
      },
    );
    await store.tasks.endAttempt({
      attemptId: ctx.attemptId,
      status: "completed",
    });
  }
}

/** The event script every test below shares: two deltas around a tool round trip. */
function turnDrafts(runId: string, timestamp: string): AiRunEventDraft[] {
  return [
    {
      type: "run.started",
      runId,
      timestamp,
      data: { model: "remote-1", toolCount: 1 },
    },
    {
      type: "run.message.completed",
      runId,
      timestamp,
      data: {
        content: "",
        toolCallCount: 1,
        toolCalls: [
          { id: "call-1", name: "lookup", argumentsJson: '{"q":"x"}' },
        ],
      },
    },
    {
      type: "run.tool.requested",
      runId,
      timestamp,
      data: {
        toolCallId: "call-1",
        toolName: "lookup",
        argumentsJson: '{"q":"x"}',
      },
    },
    {
      type: "run.tool.succeeded",
      runId,
      timestamp,
      data: {
        toolCallId: "call-1",
        toolName: "lookup",
        resultJson: '{"ok":true,"data":{"answer":42,"verbose":"VERBOSE"}}',
        modelResultJson: '{"ok":true,"data":{"answer":42}}',
        sources: [],
        truncated: false,
        warnings: [],
      },
    },
    {
      type: "run.message.delta",
      runId,
      timestamp,
      data: { delta: "The answer " },
    },
    { type: "run.message.delta", runId, timestamp, data: { delta: "is 42." } },
    {
      type: "run.message.completed",
      runId,
      timestamp,
      data: { content: "The answer is 42.", toolCallCount: 0 },
    },
    {
      type: "run.usage",
      runId,
      timestamp,
      data: {
        callId: "remote-call-1",
        attempt: 1,
        step: 0,
        model: "remote-1",
        promptTokens: 11,
        completionTokens: 7,
        totalTokens: 18,
        source: "response",
        finalForCall: true,
      },
    },
    { type: "run.completed", runId, timestamp, data: { iterations: 2 } },
  ];
}

interface Fixture extends TestHarness {
  runner: TurnRunner;
  chatId: string;
}

async function setup(): Promise<Fixture> {
  const harness = createHarness();
  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    // Never reached: no test here routes a turn to `chat.turn`.
    providerFactory: () => {
      throw new Error("the provider must not be touched on a custom kind");
    },
    contributors: [],
    clock: harness.clock,
    ids: harness.ids,
  });
  return { ...harness, runner, chatId: chat.id };
}

/**
 * Stand in for the task runner: create the attempt, take the lease, and
 * dispatch BY KIND — the same path `SingleProcessTaskRunner` drives.
 */
async function dispatch(
  f: Fixture,
  registry: ExecutorRegistry,
  taskId: string,
): Promise<string> {
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
  const worker = createDispatchingWorker(registry, {
    store: f.store,
    clock: f.clock,
  });
  await worker.execute({
    taskId,
    attemptId: attempt.attemptId,
    leaseToken: lease.leaseToken,
    signal: new AbortController().signal,
  });
  return attempt.attemptId;
}

function registryFor(executor: TaskExecutor): ExecutorRegistry {
  const registry = new ExecutorRegistry();
  registry.register(executor);
  return registry;
}

function messagesOf(f: Fixture): MessageRecord[] {
  return f.store.conversations.messages;
}

async function eventsOf(f: Fixture, taskId: string): Promise<AiRunEvent[]> {
  return (await f.store.tasks.listEvents(taskId)) as AiRunEvent[];
}

describe("submitMessage — kind", () => {
  it("defaults to chat.turn", async () => {
    const f = await setup();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    expect((await f.store.tasks.getTask(submitted.runId))?.kind).toBe(
      CHAT_TURN_TASK_KIND,
    );
  });

  it("creates the task under the host's own kind, changing nothing else", async () => {
    const f = await setup();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
      kind: CUSTOM_TURN_KIND,
    });

    const task = await f.store.tasks.getTask(submitted.runId);
    expect(task?.kind).toBe(CUSTOM_TURN_KIND);
    // Everything a chat.turn submit does, still done: one transaction, the
    // chat as the scope, both messages written, the queue poked.
    expect(f.store.transactions).toBe(1);
    expect(task?.scopeId).toBe(f.chatId);
    expect(task?.payload["chatId"]).toBe(f.chatId);
    expect(f.taskRunner.enqueued).toEqual([
      { taskId: submitted.runId, scopeId: f.chatId },
    ]);
    const user = messagesOf(f).find((m) => m.id === submitted.userMessageId);
    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(user?.content).toBe("hi");
    expect(placeholder?.content).toBe("");
    expect(placeholder?.metadata["placeholder"]).toBe(true);
    expect(placeholder?.parentMessageId).toBe(submitted.userMessageId);
  });

  it("is idempotent per taskId under the host's kind too", async () => {
    const f = await setup();
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
      kind: CUSTOM_TURN_KIND,
      taskId: "idem-1",
    });
    const before = messagesOf(f).length;
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi again",
      kind: CUSTOM_TURN_KIND,
      taskId: "idem-1",
    });
    expect(second).toEqual(first);
    expect(messagesOf(f).length).toBe(before);
  });

  it("branches under parentMessageId exactly as chat.turn does", async () => {
    const f = await setup();
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "first question",
      kind: CUSTOM_TURN_KIND,
    });
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "second question",
      kind: CUSTOM_TURN_KIND,
    });
    // Edit-and-regenerate: the rewritten question hangs off the answer BEFORE
    // it, so it becomes a sibling of the question it replaces — and the whole
    // path switches to it in the same write, before anything has executed.
    const edited = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "second question, edited",
      kind: CUSTOM_TURN_KIND,
      parentMessageId: first.assistantMessageId,
    });

    const user = messagesOf(f).find((m) => m.id === edited.userMessageId);
    expect(user?.parentMessageId).toBe(first.assistantMessageId);
    expect(user?.branchIndex).toBe(1);
    const path = await f.store.conversations.listMessages(f.chatId);
    expect(path.map((m) => m.id)).toEqual([
      first.userMessageId,
      first.assistantMessageId,
      edited.userMessageId,
      edited.assistantMessageId,
    ]);
    expect(path.some((m) => m.id === second.userMessageId)).toBe(false);
  });

  it("leaves an unregistered kind to the dispatcher", async () => {
    const f = await setup();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
      kind: "nobody.registered.this",
    });
    // The submit itself said nothing about it — the task is durable and queued.
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "queued",
    );
    const registry = registryFor(
      new ProjectingExecutor({ harness: f, drafts: [] }),
    );
    await expect(dispatch(f, registry, submitted.runId)).rejects.toThrow(
      ExecutorNotFoundError,
    );
  });
});

describe("RunProjector — a host executor on its own kind", () => {
  it("leaves the conversation a chat.turn would have left", async () => {
    const f = await setup();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "what is the answer?",
      kind: CUSTOM_TURN_KIND,
    });
    const executor = new ProjectingExecutor({
      harness: f,
      drafts: turnDrafts(submitted.runId, "2026-01-01T00:00:00.000Z"),
    });
    await dispatch(f, registryFor(executor), submitted.runId);

    // (i) The placeholder is the visible answer, and no longer a placeholder.
    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.content).toBe("The answer is 42.");
    expect(placeholder?.metadata["placeholder"]).toBe(false);

    // (ii) The replay-only records are chained off the run's OWN writes, in
    // order — the placeholder, then the internal assistant turn, then the tool
    // result answering it. Never appended at whatever the chat's active leaf
    // happened to be; see the branch-switch test below for why that matters.
    const internal = messagesOf(f).find(
      (m) => m.metadata["internal"] === true && m.role === "assistant",
    );
    const toolRecord = messagesOf(f).find((m) => m.role === "tool");
    expect(internal?.parentMessageId).toBe(submitted.assistantMessageId);
    expect(internal?.toolCalls?.map((c) => c.id)).toEqual(["call-1"]);
    expect(toolRecord?.parentMessageId).toBe(internal?.id);
    expect(toolRecord?.toolCallId).toBe("call-1");
    expect(toolRecord?.metadata["toolName"]).toBe("lookup");

    // (iii) The SLIM envelope is what got persisted; the full payload stayed on
    // the event, exactly as on the chat.turn path.
    // A tool record's body is a serialized envelope — a STRING, never parts.
    expect(typeof toolRecord?.content).toBe("string");
    expect(toolRecord?.content).toBe('{"ok":true,"data":{"answer":42}}');
    expect(toolRecord?.modelResultJson).toBe(
      '{"ok":true,"data":{"answer":42}}',
    );

    // (iv) With nobody switching branches, the run's chain IS the active path —
    // which is exactly what `activate: false` buys: the records land in replay
    // order without the run ever having to ask what the leaf was.
    const path = await f.store.conversations.listMessages(f.chatId);
    expect(path.map((m) => m.id)).toEqual([
      submitted.userMessageId,
      submitted.assistantMessageId,
      internal?.id as string,
      toolRecord?.id as string,
    ]);

    // (v) One unbroken, deduplicated sequence for the whole turn.
    const events = await eventsOf(f, submitted.runId);
    expect(events.length).toBe(9);
    expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => i));
    expect(new Set(events.map((e) => e.eventId)).size).toBe(events.length);
    expect(events.every((e) => e.runId === submitted.runId)).toBe(true);

    // (vi) And the seam's own tally is what the host reads back.
    expect(executor.lastState?.content).toBe("The answer is 42.");
    expect([...(executor.lastState?.toolCallIds ?? [])]).toEqual(["call-1"]);
  });

  it("keeps the run's records on the run's own branch when the user switches mid-run", async () => {
    const f = await setup();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "q1",
      kind: CUSTOM_TURN_KIND,
    });
    // A finished answer beside the one about to run, then back onto the run's
    // branch so the switch below is a switch AWAY from it.
    const alt = await f.store.conversations.appendMessage({
      chatId: f.chatId,
      role: "assistant",
      content: "an answer from another branch",
      parentMessageId: submitted.userMessageId,
    });
    await f.store.conversations.activatePath(submitted.assistantMessageId);

    const drafts = turnDrafts(submitted.runId, "2026-01-01T00:00:00.000Z");
    await dispatch(
      f,
      registryFor(
        new ProjectingExecutor({
          harness: f,
          drafts,
          // Right after the internal assistant record lands, and before the
          // tool result that answers it.
          after: {
            index: 1,
            run: async () => {
              await f.store.conversations.activatePath(alt.id);
            },
          },
        }),
      ),
      submitted.runId,
    );

    // The branch the user chose is the branch they get: no tool record migrated
    // onto it.
    const path = await f.store.conversations.listMessages(f.chatId);
    expect(path.map((m) => m.id)).toEqual([submitted.userMessageId, alt.id]);
    expect(path.some((m) => m.role === "tool")).toBe(false);

    // And the run's own records still form one chain under its placeholder.
    const internal = messagesOf(f).find(
      (m) => m.metadata["internal"] === true && m.role === "assistant",
    );
    const toolRecord = messagesOf(f).find((m) => m.role === "tool");
    expect(internal?.parentMessageId).toBe(submitted.assistantMessageId);
    expect(toolRecord?.parentMessageId).toBe(internal?.id);
    expect([internal?.active, toolRecord?.active]).toEqual([false, false]);
  });

  it("records every run.usage event against the projector's usage port", async () => {
    const f = await setup();
    const usage = new RecordingUsage();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
      kind: CUSTOM_TURN_KIND,
    });
    await dispatch(
      f,
      registryFor(
        new ProjectingExecutor({
          harness: f,
          drafts: turnDrafts(submitted.runId, "2026-01-01T00:00:00.000Z"),
          usage,
          providerId: "cloud-1",
        }),
      ),
      submitted.runId,
    );

    expect(usage.recorded.length).toBe(1);
    expect(usage.recorded[0]).toEqual({
      runId: submitted.runId,
      callId: "remote-call-1",
      attempt: 1,
      providerId: "cloud-1",
      model: "remote-1",
      finalForCall: true,
      source: "response",
      step: 0,
      promptTokens: 11,
      completionTokens: 7,
      totalTokens: 18,
      at: "2026-01-01T00:00:00.000Z",
    });
  });

  it("still records usage when the host named no provider", async () => {
    const f = await setup();
    const usage = new RecordingUsage();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
      kind: CUSTOM_TURN_KIND,
    });
    await dispatch(
      f,
      registryFor(
        new ProjectingExecutor({
          harness: f,
          drafts: turnDrafts(submitted.runId, "2026-01-01T00:00:00.000Z"),
          usage,
        }),
      ),
      submitted.runId,
    );
    // Unattributed, not dropped: a budget that never hears about a call cannot
    // refuse the next one.
    expect(usage.recorded.map((r) => r.providerId)).toEqual([""]);
  });

  it("records nothing when no usage port is wired", async () => {
    const f = await setup();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
      kind: CUSTOM_TURN_KIND,
    });
    // The only assertion available is that the run survives a usage event with
    // nobody listening — which is the behaviour a chat.turn without the port has.
    await dispatch(
      f,
      registryFor(
        new ProjectingExecutor({
          harness: f,
          drafts: turnDrafts(submitted.runId, "2026-01-01T00:00:00.000Z"),
        }),
      ),
      submitted.runId,
    );
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
  });
});

/**
 * The tool a real `chat.turn` calls in the equivalence test below, with a
 * model-facing payload deliberately smaller than its data — so a projection
 * that persisted the wrong one of the two would show up as a content mismatch.
 */
const echoTool: AiTool<{ text?: string }, { echoed: string; verbose: string }> =
  {
    definition: {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "Echo the input.",
      inputSchema: { type: "object", properties: { text: { type: "string" } } },
    },
    async execute(ctx, input) {
      const echoed = input.text ?? "";
      return {
        ok: true,
        data: { echoed, verbose: "VERBOSE_PAYLOAD" },
        modelData: { echoed },
        summary: `echoed ${echoed}`,
        sources: [],
        warnings: [],
        truncated: false,
        limits: ctx.limits,
      };
    },
  };

/** A real `chat.turn` fixture: mock provider, one tool, `ChatTurnExecutor`. */
async function setupChatTurn(): Promise<
  Fixture & { mock: MockProviderClient }
> {
  const harness = createHarness();
  const mock = new MockProviderClient();
  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => mock,
    contributors: [
      { namespace: "test", contribute: async () => [echoTool as AiTool] },
    ],
    clock: harness.clock,
    ids: harness.ids,
  });
  return { ...harness, runner, chatId: chat.id, mock };
}

/**
 * A conversation's SHAPE, with every id replaced by its position — so two
 * chats built by two different code paths, with different minted ids, compare
 * as the same tree or not at all.
 */
function shapeOf(f: Fixture): unknown[] {
  const records = messagesOf(f);
  const position = new Map(records.map((record, index) => [record.id, index]));
  return records.map((record) => ({
    role: record.role,
    parent:
      record.parentMessageId === undefined
        ? null
        : (position.get(record.parentMessageId) ?? "MISSING"),
    depth: record.depth,
    branchIndex: record.branchIndex,
    active: record.active,
    content: record.content,
    internal: record.metadata["internal"] === true,
    placeholder: record.metadata["placeholder"],
    toolName: record.metadata["toolName"] ?? null,
    toolCalls: record.toolCalls ?? null,
    toolCallId: record.toolCallId ?? null,
    modelResultJson: record.modelResultJson ?? null,
  }));
}

/** An appended event back to the draft it was stamped from. */
function unstamp(event: AiRunEvent, runId: string): AiRunEventDraft {
  const {
    contractVersion: _v,
    eventId: _e,
    seq: _s,
    attemptId: _a,
    ...draft
  } = event;
  return { ...draft, runId } as AiRunEventDraft;
}

describe("RunProjector — equivalence with chat.turn", () => {
  it("rebuilds the identical conversation from a real chat.turn's own event log", async () => {
    // Arm A: the real thing — TurnRunner driving runChat against a mock
    // provider that makes a tool call and then answers.
    const a = await setupChatTurn();
    a.mock.setScript([
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
      { steps: [{ kind: "text", content: "done" }] },
    ]);
    const chatTurn = await a.runner.submitMessage({
      chatId: a.chatId,
      content: "use the tool",
    });
    const attempt = await a.store.tasks.createAttempt({
      attemptId: a.ids.attemptId(),
      taskId: chatTurn.runId,
      ownerId: "worker-1",
    });
    const lease = await a.store.tasks.acquireLease({
      taskId: chatTurn.runId,
      attemptId: attempt.attemptId,
      ownerId: "worker-1",
      ttlMs: 60_000,
    });
    await a.runner.execute({
      taskId: chatTurn.runId,
      attemptId: attempt.attemptId,
      leaseToken: lease.leaseToken,
      signal: new AbortController().signal,
    });
    const log = await eventsOf(a, chatTurn.runId);
    expect(log.some((e) => e.type === "run.tool.succeeded")).toBe(true);

    // Arm B: a second, identical chat whose turn is a host kind, handed THE
    // SAME EVENTS — the ones arm A's provider and run loop produced. Same
    // input to the seam, so any divergence in the conversation is the seam
    // being driven differently, not the two runs having seen different things.
    const b = await setup();
    const custom = await b.runner.submitMessage({
      chatId: b.chatId,
      content: "use the tool",
      kind: CUSTOM_TURN_KIND,
    });
    await dispatch(
      b,
      registryFor(
        new ProjectingExecutor({
          harness: b,
          drafts: log.map((event) => unstamp(event, custom.runId)),
        }),
      ),
      custom.runId,
    );

    expect(shapeOf(b)).toEqual(shapeOf(a));
    // Not a vacuous match: the shape carries the tool round trip and the
    // finished answer.
    expect(
      messagesOf(b).find((m) => m.id === custom.assistantMessageId)?.content,
    ).toBe("done");
    expect(messagesOf(b).filter((m) => m.role === "tool").length).toBe(1);
  });
});

describe("createRunEventFeed", () => {
  it("stamps, appends and projects a raw draft, once each", async () => {
    const f = await setup();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
      kind: CUSTOM_TURN_KIND,
    });
    const attemptId = await dispatch(
      f,
      registryFor(
        new ProjectingExecutor({
          harness: f,
          drafts: turnDrafts(submitted.runId, "2026-01-01T00:00:00.000Z"),
          useFeed: true,
        }),
      ),
      submitted.runId,
    );

    // One append per draft — the feed must not both write and hand the event to
    // `project`, which would append it twice and collide on `seq`.
    const events = await eventsOf(f, submitted.runId);
    expect(events.length).toBe(9);
    expect(events.map((e) => e.seq)).toEqual(events.map((_, i) => i));
    expect(events.every((e) => e.attemptId === attemptId)).toBe(true);
    expect(events.every((e) => typeof e.contractVersion === "string")).toBe(
      true,
    );

    // And the projection ran: same conversation as the stamped path.
    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.content).toBe("The answer is 42.");
    expect(messagesOf(f).some((m) => m.role === "tool")).toBe(true);
  });
});

describe("regenerate — kind", () => {
  it("routes the new branch to the host's executor and projects into it", async () => {
    const f = await setup();
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "what is the answer?",
      kind: CUSTOM_TURN_KIND,
    });
    await dispatch(
      f,
      registryFor(
        new ProjectingExecutor({
          harness: f,
          drafts: turnDrafts(first.runId, "2026-01-01T00:00:00.000Z"),
        }),
      ),
      first.runId,
    );

    const again = await f.runner.regenerate({
      chatId: f.chatId,
      messageId: first.assistantMessageId,
      kind: CUSTOM_TURN_KIND,
    });
    const task = await f.store.tasks.getTask(again.runId);
    expect(task?.kind).toBe(CUSTOM_TURN_KIND);
    // The new branch hangs off the SAME question, and the old answer is still
    // in the tree at its own index.
    expect(again.userMessageId).toBe(first.userMessageId);

    await dispatch(
      f,
      registryFor(
        new ProjectingExecutor({
          harness: f,
          drafts: [
            {
              type: "run.message.completed",
              runId: again.runId,
              timestamp: "2026-01-01T00:01:00.000Z",
              data: { content: "Still 42.", toolCallCount: 0 },
            },
          ],
        }),
      ),
      again.runId,
    );

    const regenerated = messagesOf(f).find(
      (m) => m.id === again.assistantMessageId,
    );
    expect(regenerated?.content).toBe("Still 42.");
    expect(regenerated?.metadata["placeholder"]).toBe(false);
    const path = await f.store.conversations.listMessages(f.chatId);
    expect(path.map((m) => m.id)).toEqual([
      first.userMessageId,
      again.assistantMessageId,
    ]);
    const siblings = await f.store.conversations.listSiblings(
      first.assistantMessageId,
    );
    expect(siblings.map((m) => m.id).sort()).toEqual(
      [first.assistantMessageId, again.assistantMessageId].sort(),
    );
  });

  it("is idempotent per taskId under the host's kind", async () => {
    const f = await setup();
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "q",
      kind: CUSTOM_TURN_KIND,
    });
    const a = await f.runner.regenerate({
      chatId: f.chatId,
      messageId: first.assistantMessageId,
      kind: CUSTOM_TURN_KIND,
      taskId: "regen-1",
    });
    const b = await f.runner.regenerate({
      chatId: f.chatId,
      messageId: first.assistantMessageId,
      kind: CUSTOM_TURN_KIND,
      taskId: "regen-1",
    });
    expect(b).toEqual(a);
  });
});
