import { describe, expect, it } from "bun:test";
import type {
  AiChatMessage,
  AiContentPart,
  AiProviderCapabilities,
  AiRunEvent,
} from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import {
  CompletedOnlyProviderClient,
  MockProviderClient,
} from "@agentkit/testing";
import {
  CHAT_TURN_TASK_KIND,
  DuplicateTaskError,
  LeaseLostError,
  MISSING_TOOL_RESULT_CODE,
  TOOL_GUARD_REFUSED_CODE,
  TurnRunner,
  type AssistantSettings,
  type AttachmentBudgets,
  type AttachmentResolver,
  type MessageRecord,
  type ResolvedAttachment,
  type ToolGuard,
  type ToolSetContributor,
  type VerificationHook,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/**
 * A provider client that records what each call was handed and can be told to
 * fail specific calls — the two things the retry behaviours are defined by.
 */
class TestClient implements AiProviderClient {
  readonly id = "test";
  readonly kind = "openai-compatible";
  readonly toolsPerCall: number[] = [];
  /** The tool NAMES each call advertised — what the model could actually see. */
  readonly toolNamesPerCall: string[][] = [];
  /** The history each call was handed — what a replay assertion reads. */
  readonly messagesPerCall: AiChatMessage[][] = [];
  readonly failCalls = new Set<number>();
  calls = 0;

  constructor(readonly inner: AiProviderClient) {}

  async capabilities() {
    return this.inner.capabilities();
  }

  async listModels() {
    return this.inner.listModels();
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.calls += 1;
    this.toolsPerCall.push(input.tools?.length ?? 0);
    this.toolNamesPerCall.push((input.tools ?? []).map((t) => t.name));
    this.messagesPerCall.push([...input.messages]);
    if (this.failCalls.has(this.calls)) {
      throw new Error("provider rejected the request");
    }
    yield* this.inner.streamChat(input);
  }
}

/** A tool whose model-facing payload is deliberately smaller than its data. */
const echoTool: AiTool<{ text?: string }, { echoed: string; verbose: string }> =
  {
    definition: {
      name: "echo",
      version: "1.0.0",
      effect: "read",
      capability: "echo",
      description: "Echo the input.",
      inputSchema: {
        type: "object",
        properties: { text: { type: "string" } },
      },
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

const echoContributor: ToolSetContributor = {
  namespace: "test",
  contribute: async () => [echoTool as AiTool],
};

interface RunnerFixture extends TestHarness {
  runner: TurnRunner;
  client: TestClient;
  mock: MockProviderClient;
  chatId: string;
}

async function setupRunner(
  options: {
    contributors?: ToolSetContributor[];
    verification?: VerificationHook;
    /** Provider client to drive instead of the default streaming mock. */
    inner?: AiProviderClient;
    historyLimit?: number;
    attachments?: AttachmentResolver;
    attachmentBudgets?: AttachmentBudgets;
    toolGuards?: ToolGuard[];
    /** Applied to the settings row before the turn runs. */
    settings?: Partial<AssistantSettings>;
    /** Stored as the provider's PROBED capabilities. */
    capabilities?: AiProviderCapabilities;
  } = {},
): Promise<RunnerFixture> {
  const harness = createHarness();
  const mock = new MockProviderClient();
  const client = new TestClient(options.inner ?? mock);
  await harness.store.providers.upsertProvider({
    id: "p1",
    label: "Mock",
    kind: "openai-compatible",
    baseUrl: "http://localhost:1234",
    defaultModel: "m1",
    enabled: true,
  });
  if (options.capabilities !== undefined) {
    await harness.store.providers.saveCapabilities("p1", options.capabilities);
  }
  await harness.store.settings.updateSettings({
    defaultProviderId: "p1",
    ...options.settings,
  });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => client,
    contributors: options.contributors ?? [],
    clock: harness.clock,
    ids: harness.ids,
    ...(options.toolGuards === undefined
      ? {}
      : { toolGuards: options.toolGuards }),
    ...(options.verification === undefined
      ? {}
      : { verification: options.verification }),
    ...(options.historyLimit === undefined
      ? {}
      : { historyLimit: options.historyLimit }),
    ...(options.attachments === undefined
      ? {}
      : { attachments: options.attachments }),
    ...(options.attachmentBudgets === undefined
      ? {}
      : { attachmentBudgets: options.attachmentBudgets }),
  });
  return { ...harness, runner, client, mock, chatId: chat.id };
}

/** Stand in for the task runner: create the attempt, take the lease, execute. */
async function drive(
  f: RunnerFixture,
  taskId: string,
  opts: { abort?: boolean; leaseToken?: string } = {},
): Promise<{ attemptId: string }> {
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
  const controller = new AbortController();
  if (opts.abort) controller.abort();
  await f.runner.execute({
    taskId,
    attemptId: attempt.attemptId,
    leaseToken: opts.leaseToken ?? lease.leaseToken,
    signal: controller.signal,
  });
  return { attemptId: attempt.attemptId };
}

/**
 * The task's event log, read back as the chat-turn vocabulary it holds.
 *
 * `TaskStore.listEvents` returns the kind-agnostic envelope — that is the whole
 * point of the generalization — so a consumer that knows which kind it is
 * looking at narrows on the way out, exactly as a host would.
 */
async function eventsOf(
  f: RunnerFixture,
  taskId: string,
): Promise<AiRunEvent[]> {
  return (await f.store.tasks.listEvents(taskId)) as AiRunEvent[];
}

/** The event log is one unbroken, deduplicated sequence for the turn. */
function expectOneSequence(events: AiRunEvent[], runId: string): void {
  events.forEach((event, index) => {
    expect(event.seq).toBe(index);
    expect(event.runId).toBe(runId);
  });
  expect(new Set(events.map((e) => e.eventId)).size).toBe(events.length);
}

function messagesOf(f: RunnerFixture): MessageRecord[] {
  return f.store.conversations.messages;
}

describe("TurnRunner.submitMessage", () => {
  it("records the turn durably and returns before the model is touched", async () => {
    const f = await setupRunner();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
      model: "m1",
    });

    // One transaction for both messages and the run: a crash cannot leave a
    // user message with no run, or a run with nothing to answer.
    expect(f.store.transactions).toBe(1);
    const task = await f.store.tasks.getTask(submitted.runId);
    expect(task?.status).toBe("queued");
    expect(task?.kind).toBe(CHAT_TURN_TASK_KIND);
    expect(task?.scopeId).toBe(f.chatId);
    // chatId moved into the payload when the record went generic.
    expect(task?.payload["chatId"]).toBe(f.chatId);
    expect(f.taskRunner.enqueued).toEqual([
      { taskId: submitted.runId, scopeId: f.chatId },
    ]);
    expect(f.client.calls).toBe(0);

    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.content).toBe("");
    expect(placeholder?.metadata["placeholder"]).toBe(true);
    expect(placeholder?.runId).toBe(submitted.runId);
  });
});

describe("TurnRunner.submitMessage — branching", () => {
  it("hangs the placeholder off the user message even on a plain linear submit", async () => {
    const f = await setupRunner();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
    });

    const user = messagesOf(f).find((m) => m.id === submitted.userMessageId);
    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    // A root question with its answer under it — the degenerate tree, and the
    // shape every later turn extends.
    expect(user?.parentMessageId).toBe(undefined);
    expect(user?.depth).toBe(0);
    expect(placeholder?.parentMessageId).toBe(submitted.userMessageId);
    expect(placeholder?.depth).toBe(1);
    expect([user?.branchIndex, placeholder?.branchIndex]).toEqual([0, 0]);
    const path = await f.store.conversations.listMessages(f.chatId);
    expect(path.map((m) => m.id)).toEqual([
      submitted.userMessageId,
      submitted.assistantMessageId,
    ]);
  });

  it("branches the whole turn under parentMessageId and switches the path to it", async () => {
    const f = await setupRunner();
    f.mock.setScript([
      { steps: [{ kind: "text", content: "first answer" }] },
      { steps: [{ kind: "text", content: "second answer" }] },
      { steps: [{ kind: "text", content: "revised answer" }] },
    ]);
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "first question",
    });
    await drive(f, first.runId);
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "second question",
    });
    await drive(f, second.runId);

    // Edit-and-regenerate: the rewritten question hangs off the answer BEFORE
    // it, so it becomes a sibling of the question it replaces.
    const edited = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "second question, edited",
      parentMessageId: first.assistantMessageId,
    });

    const user = messagesOf(f).find((m) => m.id === edited.userMessageId);
    const placeholder = messagesOf(f).find(
      (m) => m.id === edited.assistantMessageId,
    );
    expect(user?.parentMessageId).toBe(first.assistantMessageId);
    expect(user?.branchIndex).toBe(1);
    expect(placeholder?.parentMessageId).toBe(edited.userMessageId);

    // The switch is already done, before the run has executed: the replaced
    // question and its answer are off the path.
    const path = await f.store.conversations.listMessages(f.chatId);
    expect(path.map((m) => m.id)).toEqual([
      first.userMessageId,
      first.assistantMessageId,
      edited.userMessageId,
      edited.assistantMessageId,
    ]);
  });

  it("replays the NEW branch to the provider, not the answer it replaced", async () => {
    const f = await setupRunner();
    f.mock.setScript([
      { steps: [{ kind: "text", content: "first answer" }] },
      { steps: [{ kind: "text", content: "second answer" }] },
      { steps: [{ kind: "text", content: "revised answer" }] },
    ]);
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "first question",
    });
    await drive(f, first.runId);
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "second question",
    });
    await drive(f, second.runId);

    const edited = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "second question, edited",
      parentMessageId: first.assistantMessageId,
    });
    // SNAPSHOT the contents at call time: `runChat` appends the model's reply to
    // the same array it was handed, so holding the reference and reading it
    // afterwards would show this turn's own answer as part of its prompt.
    const seen: string[][] = [];
    const inner = f.client.inner.streamChat.bind(f.client.inner);
    f.client.inner.streamChat = (input) => {
      seen.push(
        input.messages.map((m) =>
          // Content is `string | AiContentPart[]`; this suite scripts text only,
          // and a multimodal turn showing up here should read as odd, not crash.
          typeof m.content === "string" ? m.content : JSON.stringify(m.content),
        ),
      );
      return inner(input);
    };
    await drive(f, edited.runId);

    const replayed = seen[0]!;
    // The kept prefix, then the rewrite. The abandoned branch is absent in both
    // halves — the discarded question AND the answer that was given to it.
    expect(replayed).toEqual([
      "first question",
      "first answer",
      "second question, edited",
    ]);
    expect(
      messagesOf(f).find((m) => m.id === edited.assistantMessageId)?.content,
    ).toBe("revised answer");
    // The replaced turn is still stored — branching hides history, it does not
    // delete it.
    expect(
      messagesOf(f).find((m) => m.id === second.userMessageId)?.active,
    ).toBe(false);
    expect(
      messagesOf(f).find((m) => m.id === second.assistantMessageId)?.content,
    ).toBe("second answer");
  });
});

describe("TurnRunner.submitMessage — idempotency key", () => {
  /** Count every enqueue, including the ones the port dedupes internally. */
  function countEnqueues(f: RunnerFixture): () => number {
    let calls = 0;
    const inner = f.taskRunner.enqueue.bind(f.taskRunner);
    f.taskRunner.enqueue = async (input) => {
      calls += 1;
      await inner(input);
    };
    return () => calls;
  }

  it("resubmitting one key returns the first turn's ids and writes nothing new", async () => {
    const f = await setupRunner();
    const enqueues = countEnqueues(f);
    const input = { chatId: f.chatId, content: "hello", taskId: "turn-idem" };

    const first = await f.runner.submitMessage(input);
    const second = await f.runner.submitMessage({
      ...input,
      content: "hello again",
    });

    // Identical ids both times: the caller cannot tell which of its two
    // attempts landed, and does not have to.
    expect(second).toEqual(first);
    expect(first.runId).toBe("turn-idem");
    // One task, one user message, one placeholder. The second submit is a
    // redelivery, not a second turn — and its content never reached the chat.
    expect(f.store.tasks.tasks.size).toBe(1);
    expect(messagesOf(f).map((m) => m.content)).toEqual(["hello", ""]);
    // Re-poked, because the case this exists for is a first submit that
    // committed and then died before it could enqueue. The port dedupes, so
    // the queue still only ever heard about one task.
    expect(enqueues()).toBe(2);
    expect(f.taskRunner.enqueued).toEqual([
      { taskId: "turn-idem", scopeId: f.chatId },
    ]);
    expect(f.client.calls).toBe(0);
  });

  it("rethrows when the key already names a turn in a DIFFERENT chat", async () => {
    const f = await setupRunner();
    await f.store.conversations.createChat({ id: "chat-2" });
    await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
      taskId: "turn-clash",
    });

    // Two callers colliding on a key, not one caller retrying. Answering the
    // second with the first's ids would hand it someone else's conversation.
    await expect(
      f.runner.submitMessage({
        chatId: "chat-2",
        content: "hi",
        taskId: "turn-clash",
      }),
    ).rejects.toThrow(DuplicateTaskError);
    expect(messagesOf(f).filter((m) => m.chatId === "chat-2")).toEqual([]);
    expect(f.store.tasks.tasks.size).toBe(1);
  });

  it("starts a new turn every time when no key is supplied", async () => {
    const f = await setupRunner();
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    expect(second.runId).not.toBe(first.runId);
    expect(f.store.tasks.tasks.size).toBe(2);
    expect(messagesOf(f)).toHaveLength(4);
  });
});

describe("TurnRunner.regenerate", () => {
  /** Ask once, answer once — the shape every regenerate below branches on. */
  async function askOnce(
    f: RunnerFixture,
  ): Promise<{ userMessageId: string; assistantMessageId: string }> {
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "why is the sky blue?",
    });
    await drive(f, first.runId);
    return first;
  }

  it("appends a NEW BRANCH under the same question and leaves the old answer in the tree", async () => {
    const f = await setupRunner();
    f.mock.setScript([
      { steps: [{ kind: "text", content: "first answer" }] },
      { steps: [{ kind: "text", content: "second answer" }] },
    ]);
    const first = await askOnce(f);

    const again = await f.runner.regenerate({
      chatId: f.chatId,
      messageId: first.assistantMessageId,
    });

    // The question is not rewritten and not copied: the new run answers the
    // message that was already there, which is what the result reports.
    expect(again.userMessageId).toBe(first.userMessageId);
    expect(again.assistantMessageId).not.toBe(first.assistantMessageId);

    const placeholder = messagesOf(f).find(
      (m) => m.id === again.assistantMessageId,
    );
    expect(placeholder?.parentMessageId).toBe(first.userMessageId);
    expect(placeholder?.metadata["placeholder"]).toBe(true);
    expect(placeholder?.runId).toBe(again.runId);
    // The next index under that parent, and the live one.
    expect(placeholder?.branchIndex).toBe(1);
    expect(placeholder?.active).toBe(true);

    // The answer it replaces is untouched — same id, same index, same text —
    // and simply off the path, so activating it back is a branch switch.
    const old = messagesOf(f).find((m) => m.id === first.assistantMessageId);
    expect(old?.branchIndex).toBe(0);
    expect(old?.content).toBe("first answer");
    expect(old?.active).toBe(false);

    const path = await f.store.conversations.listMessages(f.chatId);
    expect(path.map((m) => m.id)).toEqual([
      first.userMessageId,
      again.assistantMessageId,
    ]);
    const siblings = await f.store.conversations.listSiblings(
      first.assistantMessageId,
    );
    expect(siblings.map((m) => m.id)).toEqual([
      first.assistantMessageId,
      again.assistantMessageId,
    ]);
  });

  it("runs the new branch to completion, replaying the SAME history the first answer saw", async () => {
    const f = await setupRunner();
    f.mock.setScript([
      { steps: [{ kind: "text", content: "first answer" }] },
      { steps: [{ kind: "text", content: "second answer" }] },
    ]);
    const first = await askOnce(f);
    const again = await f.runner.regenerate({
      chatId: f.chatId,
      messageId: first.assistantMessageId,
    });
    await drive(f, again.runId);

    const fresh = messagesOf(f).find((m) => m.id === again.assistantMessageId);
    expect(fresh?.content).toBe("second answer");
    expect(fresh?.metadata["placeholder"]).toBe(false);
    expect((await f.store.tasks.getTask(again.runId))?.status).toBe(
      "completed",
    );

    // The pass was handed the question and nothing else: the old answer is
    // off-path, so it never reaches the provider — otherwise the model would
    // be asked to improve on an answer it is supposed to be replacing.
    const replayed = f.client.messagesPerCall[1] ?? [];
    expect(replayed.map((m) => m.role)).toEqual(["user"]);
    expect(replayed[0]?.content).toBe("why is the sky blue?");
  });

  it("is idempotent per taskId: the second call writes nothing and answers with the first's ids", async () => {
    const f = await setupRunner();
    const first = await askOnce(f);
    const input = {
      chatId: f.chatId,
      messageId: first.assistantMessageId,
      taskId: "regen-idem",
    };

    const once = await f.runner.regenerate(input);
    const twice = await f.runner.regenerate(input);

    expect(twice).toEqual(once);
    expect(once.runId).toBe("regen-idem");
    // Two tasks in the store: the original turn and exactly one regenerate.
    expect(f.store.tasks.tasks.size).toBe(2);
    // Three messages: question, first answer, one new placeholder.
    expect(messagesOf(f)).toHaveLength(3);
    expect(
      (await f.store.conversations.listSiblings(first.assistantMessageId))
        .length,
    ).toBe(2);
  });

  it("refuses a target that answered no question", async () => {
    const f = await setupRunner();
    const first = await askOnce(f);

    // A question, not an answer.
    await expect(
      f.runner.regenerate({
        chatId: f.chatId,
        messageId: first.userMessageId,
      }),
    ).rejects.toMatchObject({ code: "invalid_regenerate" });

    // A replay-only assistant record: branching under its parent would strand
    // the tool results answering it on a path nothing replays.
    const internal = await f.store.conversations.appendMessage({
      chatId: f.chatId,
      role: "assistant",
      content: "",
      parentMessageId: first.assistantMessageId,
      activate: false,
      metadata: { internal: true },
    });
    await expect(
      f.runner.regenerate({ chatId: f.chatId, messageId: internal.id }),
    ).rejects.toMatchObject({ code: "invalid_regenerate" });

    // A root assistant message has nothing above it to ask again.
    const orphan = await f.store.conversations.appendMessage({
      chatId: "chat-orphan",
      role: "assistant",
      content: "unprompted",
    });
    expect(orphan.parentMessageId).toBe(undefined);
    await expect(
      f.runner.regenerate({ chatId: "chat-orphan", messageId: orphan.id }),
    ).rejects.toMatchObject({ code: "invalid_regenerate" });

    // Nothing above wrote a task or a placeholder.
    expect(f.store.tasks.tasks.size).toBe(1);
  });

  it("refuses an unknown message, and one belonging to another chat", async () => {
    const f = await setupRunner();
    const first = await askOnce(f);
    await f.store.conversations.createChat({ id: "chat-2" });

    await expect(
      f.runner.regenerate({ chatId: f.chatId, messageId: "no-such-message" }),
    ).rejects.toMatchObject({ code: "not_found" });

    // The id exists, but not in the chat the caller named — a mismatched pair,
    // and running it would write this chat's answer into that one's history.
    await expect(
      f.runner.regenerate({
        chatId: "chat-2",
        messageId: first.assistantMessageId,
      }),
    ).rejects.toMatchObject({ code: "not_found" });
    expect(f.store.tasks.tasks.size).toBe(1);
  });
});

describe("TurnRunner.execute — text only", () => {
  it("streams into the placeholder and completes the run", async () => {
    const f = await setupRunner();
    f.mock.setScript([
      {
        steps: [
          { kind: "text", content: "Hello " },
          { kind: "text", content: "world" },
        ],
      },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    const { attemptId } = await drive(f, submitted.runId);

    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.content).toBe("Hello world");
    expect(placeholder?.metadata["placeholder"]).toBe(false);

    const run = await f.store.tasks.getTask(submitted.runId);
    expect(run?.status).toBe("completed");
    expect(run?.startedAt).toBeDefined();
    expect(run?.finishedAt).toBeDefined();
    expect(f.store.tasks.attempts.get(attemptId)?.status).toBe("completed");

    const events = await eventsOf(f, submitted.runId);
    expectOneSequence(events, submitted.runId);
    expect(events.every((e) => e.attemptId === attemptId)).toBe(true);
    expect(events.at(-1)?.type).toBe("run.completed");
  });

  it("fills the answer from a completed-only provider that streams no deltas", async () => {
    // A non-streaming provider carries the whole answer on
    // `run.message.completed`; keying the persisted answer on deltas alone would
    // leave the user with a blank bubble and trigger a pointless retry.
    const completedOnly = new CompletedOnlyProviderClient();
    completedOnly.setScript([{ content: "answered in one shot" }]);
    const f = await setupRunner({ inner: completedOnly });
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    await drive(f, submitted.runId);

    expect(f.client.calls).toBe(1);
    expect(
      messagesOf(f).find((m) => m.id === submitted.assistantMessageId)?.content,
    ).toBe("answered in one shot");
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
  });
});

describe("TurnRunner.execute — tool round trip", () => {
  it("persists the slim envelope as the tool message and the full payload on the event", async () => {
    const f = await setupRunner({ contributors: [echoContributor] });
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
      { steps: [{ kind: "text", content: "done" }] },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "use the tool",
    });
    await drive(f, submitted.runId);

    const records = messagesOf(f);
    const internal = records.find(
      (m) => m.role === "assistant" && m.metadata["internal"] === true,
    );
    // The mock reports the COUNT on `completed` and the calls themselves on the
    // `run.tool.requested` events that follow; the record must end up with them
    // either way, or the tool result below is an orphan on replay.
    expect(internal?.toolCalls).toEqual([
      { id: "call-1", name: "echo", argumentsJson: '{"text":"hi"}' },
    ]);

    const toolRecord = records.find((m) => m.role === "tool");
    expect(toolRecord?.toolCallId).toBe("call-1");
    expect(toolRecord?.metadata["toolName"]).toBe("echo");
    // A tool record's body is a serialized envelope — a STRING, never parts.
    // Asserting that before parsing is the point: `MessageRecord.content` is a
    // union now, and a tool result that arrived as parts would be a bug this
    // test must not paper over with a cast.
    expect(typeof toolRecord?.content).toBe("string");
    const envelope = JSON.parse(toolRecord?.content as string) as {
      data: Record<string, unknown>;
    };
    expect(envelope.data).toEqual({ echoed: "hi" });
    expect(toolRecord?.content).not.toContain("VERBOSE_PAYLOAD");

    const events = await eventsOf(f, submitted.runId);
    const succeeded = events.find((e) => e.type === "run.tool.succeeded") as
      | (AiRunEvent & { data: { resultJson: string } })
      | undefined;
    expect(succeeded?.data.resultJson).toContain("VERBOSE_PAYLOAD");
    expectOneSequence(events, submitted.runId);

    expect(
      messagesOf(f).find((m) => m.id === submitted.assistantMessageId)?.content,
    ).toBe("done");
  });

  it("replays the stored turn to the provider in provider-legal order", async () => {
    const f = await setupRunner({ contributors: [echoContributor] });
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
      { steps: [{ kind: "text", content: "first answer" }] },
      { steps: [{ kind: "text", content: "second answer" }] },
    ]);
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "use the tool",
    });
    await drive(f, first.runId);

    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "follow up",
    });
    const seen: AiChatRequest[] = [];
    const inner = f.client.inner.streamChat.bind(f.client.inner);
    f.client.inner.streamChat = (input) => {
      seen.push(input);
      return inner(input);
    };
    await drive(f, second.runId);

    const replayed = seen[0]!.messages;
    // The visible answer of run 1 must come AFTER its internal assistant turn
    // and the tool result that answered it.
    const internalIndex = replayed.findIndex(
      (m) => m.role === "assistant" && (m.toolCalls?.length ?? 0) > 0,
    );
    const toolIndex = replayed.findIndex((m) => m.role === "tool");
    const visibleIndex = replayed.findIndex(
      (m) => m.role === "assistant" && m.content === "first answer",
    );
    // All three must actually be present, or the ordering assertion below would
    // hold vacuously on -1.
    expect(internalIndex).toBeGreaterThanOrEqual(0);
    expect(toolIndex).toBeGreaterThanOrEqual(0);
    expect(visibleIndex).toBeGreaterThanOrEqual(0);
    expect(internalIndex).toBeLessThan(toolIndex);
    expect(toolIndex).toBeLessThan(visibleIndex);
    // The placeholder of the run being executed is never replayed to the model:
    // no empty assistant turn without tool calls. (The internal turn IS empty,
    // but carries the tool_calls the tool result answers.)
    expect(
      replayed.some(
        (m) =>
          m.role === "assistant" &&
          m.content === "" &&
          (m.toolCalls?.length ?? 0) === 0,
      ),
    ).toBe(false);
  });

  it("never replays a tool result whose assistant turn fell outside the history window", async () => {
    // Window of 3 messages: the tool result survives, the assistant turn that
    // requested it does not. Sending it anyway is an orphan tool_call_id, which
    // the provider rejects — the whole conversation would stop working.
    const f = await setupRunner({
      contributors: [echoContributor],
      historyLimit: 3,
    });
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
      { steps: [{ kind: "text", content: "first answer" }] },
      { steps: [{ kind: "text", content: "second answer" }] },
    ]);
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "use the tool",
    });
    await drive(f, first.runId);

    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "follow up",
    });
    const seen: AiChatRequest[] = [];
    const inner = f.client.inner.streamChat.bind(f.client.inner);
    f.client.inner.streamChat = (input) => {
      seen.push(input);
      return inner(input);
    };
    await drive(f, second.runId);

    const replayed = seen[0]!.messages;
    // The window holds the tool result but not the assistant turn that asked
    // for it, so the result is dropped rather than sent as an orphan.
    expect(replayed.some((m) => m.role === "tool")).toBe(false);
    expect(replayed.some((m) => (m.toolCalls?.length ?? 0) > 0)).toBe(false);
    expect((await f.store.tasks.getTask(second.runId))?.status).toBe(
      "completed",
    );
    expect(
      messagesOf(f).find((m) => m.id === second.assistantMessageId)?.content,
    ).toBe("second answer");
  });
});

describe("TurnRunner.execute — retries", () => {
  it("retries chat-only after a provider failure with tools staged", async () => {
    const f = await setupRunner({ contributors: [echoContributor] });
    f.client.failCalls.add(1);
    f.mock.setScript([
      { steps: [{ kind: "text", content: "chat only answer" }] },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    const { attemptId } = await drive(f, submitted.runId);

    // First call carried the tool, the retry carried none.
    expect(f.client.toolsPerCall).toEqual([1, 0]);
    expect(
      messagesOf(f).find((m) => m.id === submitted.assistantMessageId)?.content,
    ).toBe("chat only answer");
    const run = await f.store.tasks.getTask(submitted.runId);
    expect(run?.status).toBe("completed");

    const events = await eventsOf(f, submitted.runId);
    // One run id, one attempt, one uninterrupted sequence across both passes.
    expectOneSequence(events, submitted.runId);
    expect(events.every((e) => e.attemptId === attemptId)).toBe(true);
    expect(events.some((e) => e.type === "run.failed")).toBe(true);
    expect(events.some((e) => e.type === "run.completed")).toBe(true);
  });

  it("does not retry chat-only when no tools were staged", async () => {
    const f = await setupRunner();
    f.client.failCalls.add(1);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    await drive(f, submitted.runId);

    expect(f.client.calls).toBe(1);
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "failed",
    );
  });

  it("retries once on an empty answer, then warns", async () => {
    const f = await setupRunner();
    // Empty script: every turn completes with no content and no tool calls.
    f.mock.setScript([]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    await drive(f, submitted.runId);

    expect(f.client.calls).toBe(2);
    const events = await eventsOf(f, submitted.runId);
    expectOneSequence(events, submitted.runId);
    const warning = events.find(
      (e) => e.type === "run.warning" && e.data.code === "empty_response",
    );
    expect(warning).toBeDefined();
    expect(warning?.attemptId).toBeDefined();
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
  });

  it("does not warn when the retry produces an answer", async () => {
    const f = await setupRunner();
    f.mock.setScript([
      { steps: [] },
      { steps: [{ kind: "text", content: "second time lucky" }] },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    await drive(f, submitted.runId);

    expect(f.client.calls).toBe(2);
    const events = await eventsOf(f, submitted.runId);
    expect(
      events.some(
        (e) => e.type === "run.warning" && e.data.code === "empty_response",
      ),
    ).toBe(false);
    expect(
      messagesOf(f).find((m) => m.id === submitted.assistantMessageId)?.content,
    ).toBe("second time lucky");
  });
});

describe("TurnRunner.execute — emulated tool calls", () => {
  it("warns and posts a banner when the model wrote a tool call as text", async () => {
    const f = await setupRunner({ contributors: [echoContributor] });
    f.mock.setScript([
      {
        steps: [
          {
            kind: "text",
            content:
              'Calling it now:\n```json\n{"name":"echo","arguments":{"text":"hi"}}\n```',
          },
        ],
      },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "use the tool",
    });
    await drive(f, submitted.runId);

    const events = await eventsOf(f, submitted.runId);
    expect(
      events.some(
        (e) => e.type === "run.warning" && e.data.code === "emulated_tool_call",
      ),
    ).toBe(true);
    expectOneSequence(events, submitted.runId);

    const banner = messagesOf(f).find((m) => m.role === "system");
    expect(banner?.metadata["banner"]).toBe("emulated_tool_call");
    expect(banner?.runId).toBe(submitted.runId);
  });

  it("stays quiet when the run made real tool calls", async () => {
    const f = await setupRunner({ contributors: [echoContributor] });
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
      {
        steps: [
          {
            kind: "text",
            content:
              'Here is what I sent:\n```json\n{"name":"echo","arguments":{}}\n```',
          },
        ],
      },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await drive(f, submitted.runId);

    const events = await eventsOf(f, submitted.runId);
    expect(
      events.some(
        (e) => e.type === "run.warning" && e.data.code === "emulated_tool_call",
      ),
    ).toBe(false);
    expect(messagesOf(f).some((m) => m.role === "system")).toBe(false);
  });
});

describe("TurnRunner.execute — verification", () => {
  it("posts the deficiencies when a verified run did not finish the job", async () => {
    const verification: VerificationHook = {
      verify: async () => ({
        status: "partial",
        checks: [{ id: "c1", ok: false, message: "half done" }],
        deficiencies: ["two items still unlinked"],
      }),
    };
    const f = await setupRunner({
      contributors: [echoContributor],
      verification,
    });
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
      { steps: [{ kind: "text", content: "all set" }] },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await drive(f, submitted.runId);

    const banner = messagesOf(f).find((m) => m.role === "system");
    expect(banner?.metadata["banner"]).toBe("verification");
    expect(banner?.content).toContain("two items still unlinked");
  });

  it("skips verification for a chat-only answer", async () => {
    let calls = 0;
    const verification: VerificationHook = {
      verify: async () => {
        calls += 1;
        return null;
      },
    };
    const f = await setupRunner({ verification });
    f.mock.setScript([{ steps: [{ kind: "text", content: "just talking" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    await drive(f, submitted.runId);
    expect(calls).toBe(0);
  });
});

describe("TurnRunner.execute — cancellation and failure", () => {
  it("lands a pre-aborted run as cancelled with the placeholder finalized", async () => {
    const f = await setupRunner();
    f.mock.setScript([{ steps: [{ kind: "text", content: "never sent" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    const { attemptId } = await drive(f, submitted.runId, { abort: true });

    const run = await f.store.tasks.getTask(submitted.runId);
    expect(run?.status).toBe("cancelled");
    expect(f.store.tasks.attempts.get(attemptId)?.status).toBe("cancelled");

    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.metadata["placeholder"]).toBe(false);
    expect(placeholder?.content).toBe("");

    const events = await eventsOf(f, submitted.runId);
    expect(events.at(-1)?.type).toBe("run.cancelled");
    // A cancelled run is not an "empty answer" — it was stopped.
    expect(
      events.some(
        (e) => e.type === "run.warning" && e.data.code === "empty_response",
      ),
    ).toBe(false);
  });

  it("forwards cancel to the task runner", async () => {
    const f = await setupRunner();
    await f.runner.cancel("run-42");
    expect(f.taskRunner.cancelled).toEqual(["run-42"]);
  });

  it("fails the run when the lease is not ours, and rethrows", async () => {
    const f = await setupRunner();
    f.mock.setScript([{ steps: [{ kind: "text", content: "hi" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    await expect(
      drive(f, submitted.runId, { leaseToken: "someone-elses-lease" }),
    ).rejects.toThrow(LeaseLostError);

    const run = await f.store.tasks.getTask(submitted.runId);
    expect(run?.status).toBe("failed");
    expect(run?.error).toContain("lease");
  });

  it("fails terminally when the task payload has no chatId", async () => {
    // Only submitMessage may create a chat.turn task; a row from anywhere else
    // is unexecutable now and on every retry, so it must land failed rather
    // than burn the attempt budget re-reading the same payload.
    const f = await setupRunner();
    await f.store.tasks.createTask({
      taskId: "task-no-chat",
      kind: CHAT_TURN_TASK_KIND,
      scopeId: f.chatId,
      payload: { assistantMessageId: "msg-1" },
    });
    await expect(drive(f, "task-no-chat")).rejects.toThrow(/no chatId/);

    const task = await f.store.tasks.getTask("task-no-chat");
    expect(task?.status).toBe("failed");
    expect(task?.error).toContain("no chatId");
    expect(f.client.calls).toBe(0);
  });

  it("fails terminally when the task payload has no assistantMessageId", async () => {
    // Same terminal guard, other half: a chat with no placeholder to stream
    // into would fail on the first `updateMessage` — after the model had
    // already been called and paid for.
    const f = await setupRunner();
    await f.store.tasks.createTask({
      taskId: "task-no-placeholder",
      kind: CHAT_TURN_TASK_KIND,
      scopeId: f.chatId,
      payload: { chatId: f.chatId },
    });
    await expect(drive(f, "task-no-placeholder")).rejects.toThrow(
      /no assistantMessageId/,
    );

    const task = await f.store.tasks.getTask("task-no-placeholder");
    expect(task?.status).toBe("failed");
    expect(task?.error).toContain("no assistantMessageId");
    expect(f.client.calls).toBe(0);
  });

  it("refuses to execute a run that already finished", async () => {
    const f = await setupRunner();
    f.mock.setScript([{ steps: [{ kind: "text", content: "hi" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hi",
    });
    await drive(f, submitted.runId);
    await expect(drive(f, submitted.runId)).rejects.toThrow(
      /only queued or running/,
    );
  });
});

/**
 * A branch switch that lands in the MIDDLE of a running turn.
 *
 * The window is real and not narrow: a turn writes its internal assistant
 * record when the model asks for tools and each tool result when that tool
 * returns, seconds or minutes apart, and nothing stops the user reading a
 * different branch in between. The tool below performs the switch from inside
 * `execute`, which puts it exactly between those two writes — deterministically,
 * where a timer would be a race.
 */
describe("TurnRunner — a branch switch mid-run", () => {
  /** A tool that switches the chat's active path, then answers normally. */
  function switchingContributor(
    f: () => RunnerFixture,
    target: () => string,
  ): ToolSetContributor {
    const tool: AiTool<Record<string, never>, { switched: string }> = {
      definition: {
        name: "switcher",
        version: "1.0.0",
        effect: "read",
        capability: "switch",
        description: "Switch the chat's active branch mid-run.",
        inputSchema: { type: "object", properties: {} },
      },
      async execute(ctx) {
        const switched = target();
        await f().store.conversations.activatePath(switched);
        return {
          ok: true,
          data: { switched },
          summary: `switched to ${switched}`,
          sources: [],
          warnings: [],
          truncated: false,
          limits: ctx.limits,
        };
      },
    };
    return { namespace: "test", contribute: async () => [tool as AiTool] };
  }

  /**
   * `u1` with two answers under it: the placeholder of a turn about to run, and
   * an already-finished alternative the user can switch to. The run is left
   * live on its own branch.
   */
  async function setupSwitchScenario() {
    let fixture: RunnerFixture | undefined;
    let altId = "";
    const f = await setupRunner({
      contributors: [
        switchingContributor(
          () => fixture as RunnerFixture,
          () => altId,
        ),
      ],
    });
    fixture = f;
    f.mock.setScript([
      {
        steps: [
          {
            kind: "tool_call",
            toolCallId: "call-1",
            name: "switcher",
            argumentsJson: "{}",
          },
        ],
      },
      { steps: [{ kind: "text", content: "done" }] },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "q1",
    });
    const alt = await f.store.conversations.appendMessage({
      chatId: f.chatId,
      role: "assistant",
      content: "an answer from another branch",
      parentMessageId: submitted.userMessageId,
    });
    altId = alt.id;
    // Back onto the branch the turn is going to run on, so the switch the tool
    // performs is a switch AWAY from it.
    await f.store.conversations.activatePath(submitted.assistantMessageId);
    return { f, submitted, alt };
  }

  it("leaves the run's records on the run's own branch, not on the one the user switched to", async () => {
    const { f, submitted, alt } = await setupSwitchScenario();
    await drive(f, submitted.runId);

    // (i) The branch the user is reading is the branch they chose. A tool
    // result from a turn this conversation never ran must not appear in it.
    const path = await f.store.conversations.listMessages(f.chatId);
    expect(path.map((m) => m.id)).toEqual([submitted.userMessageId, alt.id]);
    expect(path.some((m) => m.role === "tool")).toBe(false);

    // The run's records went where the run is: chained under its placeholder,
    // and inactive because that whole branch is.
    const internal = messagesOf(f).find(
      (m) => m.metadata["internal"] === true && m.role === "assistant",
    );
    const toolResult = messagesOf(f).find((m) => m.role === "tool");
    expect(internal?.parentMessageId).toBe(submitted.assistantMessageId);
    expect(toolResult?.parentMessageId).toBe(internal?.id);
    expect([internal?.active, toolResult?.active]).toEqual([false, false]);
  });

  it("replays the run's own branch balanced, with no synthetic tool failure", async () => {
    const { f, submitted } = await setupSwitchScenario();
    await drive(f, submitted.runId);

    // (ii) Back to the branch the run wrote. Its assistant turn declared
    // `call-1` and its tool result answered it; if the result had been migrated
    // onto the other branch, `reconcileOrphanToolCalls` would have to invent a
    // failure here for a tool that actually succeeded.
    await f.store.conversations.activatePath(submitted.assistantMessageId);
    f.mock.setScript([{ steps: [{ kind: "text", content: "and again" }] }]);
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "q2",
    });
    await drive(f, second.runId);

    const replayed = f.client.messagesPerCall.at(-1) ?? [];
    const toolMessages = replayed.filter((m) => m.role === "tool");
    expect(toolMessages.map((m) => m.toolCallId)).toEqual(["call-1"]);
    expect(
      toolMessages.some((m) =>
        JSON.stringify(m.content).includes(MISSING_TOOL_RESULT_CODE),
      ),
    ).toBe(false);
    // And the call still precedes the result it belongs to.
    const declaring = replayed.findIndex((m) =>
      (m.toolCalls ?? []).some((call) => call.id === "call-1"),
    );
    expect(declaring).toBeGreaterThan(-1);
    expect(replayed.indexOf(toolMessages[0] as AiChatMessage)).toBeGreaterThan(
      declaring,
    );
  });
});

/**
 * A resolver that counts its calls and answers from a fixed table — the two
 * things every attachment behaviour below is defined by. `null` for an unknown
 * ref is the port's own "there are no bytes for this", not an error path.
 */
class TestAttachmentResolver implements AttachmentResolver {
  readonly calls: string[] = [];
  constructor(
    private readonly table: Record<string, ResolvedAttachment> = {},
  ) {}
  async resolve(ref: string): Promise<ResolvedAttachment | null> {
    this.calls.push(ref);
    return this.table[ref] ?? null;
  }
}

/** Base64 whose DECODED length is `bytes` (four characters per three bytes). */
function base64OfBytes(bytes: number): string {
  return "A".repeat(Math.ceil((bytes * 4) / 3));
}

function png(base64: string): ResolvedAttachment {
  return { mediaType: "image/png", base64 };
}

/** A user body: some words, then one image part per ref, in order. */
function bodyWithRefs(...refs: string[]): AiContentPart[] {
  return [
    { type: "text", text: "what is in these?" },
    ...refs.map(
      (ref): AiContentPart => ({
        type: "image",
        source: { kind: "ref", ref },
      }),
    ),
  ];
}

/** The user message as the provider was handed it on the last call. */
function userContentSeenByProvider(f: RunnerFixture): AiChatMessage["content"] {
  const sent = f.client.messagesPerCall.at(-1) ?? [];
  const user = sent.find((message) => message.role === "user");
  expect(user).toBeDefined();
  return user?.content ?? "";
}

/** The image parts of what the provider was shown, refs already resolved. */
function imagePartsSeenByProvider(f: RunnerFixture): AiContentPart[] {
  const content = userContentSeenByProvider(f);
  return typeof content === "string"
    ? []
    : content.filter((part) => part.type === "image");
}

/** The messages of every `run.warning` on the log carrying `code`. */
async function warningsOf(
  f: RunnerFixture,
  runId: string,
  code: string,
): Promise<string[]> {
  const events = await eventsOf(f, runId);
  return events
    .filter((event) => event.type === "run.warning" && event.data.code === code)
    .map((event) => (event.type === "run.warning" ? event.data.message : ""));
}

describe("TurnRunner — attachment resolution", () => {
  it("resolves a ref image for the provider while the stored message keeps the ref", async () => {
    const attachments = new TestAttachmentResolver({
      "blob:cat": png("aGVsbG8="),
    });
    const f = await setupRunner({ attachments });
    f.mock.setScript([{ steps: [{ kind: "text", content: "a cat" }] }]);
    const body = bodyWithRefs("blob:cat");
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: body,
    });
    await drive(f, submitted.runId);

    // What the PROVIDER saw: inline data, in the same position, with the text
    // part around it untouched.
    expect(userContentSeenByProvider(f)).toEqual([
      { type: "text", text: "what is in these?" },
      {
        type: "image",
        source: { kind: "data", base64: "aGVsbG8=", mediaType: "image/png" },
      },
    ]);
    expect(attachments.calls).toEqual(["blob:cat"]);

    // What the STORE holds: the ref, unchanged. Re-read from the store rather
    // than trusted from the submit — a runner that rewrote the record mid-pass
    // would leave the object already in hand looking correct.
    const stored = messagesOf(f).find((m) => m.id === submitted.userMessageId);
    expect(stored?.content).toEqual(body);
    expect(
      await warningsOf(f, submitted.runId, "attachment_unresolved"),
    ).toEqual([]);
  });

  it("drops an unresolvable ref, warns attachment_unresolved, and answers anyway", async () => {
    const attachments = new TestAttachmentResolver({
      "blob:here": png("aGVsbG8="),
    });
    const f = await setupRunner({ attachments });
    f.mock.setScript([{ steps: [{ kind: "text", content: "one of two" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: bodyWithRefs("blob:here", "blob:gone"),
    });
    await drive(f, submitted.runId);

    // The surviving image is still there; only the missing one is gone.
    expect(imagePartsSeenByProvider(f)).toEqual([
      {
        type: "image",
        source: { kind: "data", base64: "aGVsbG8=", mediaType: "image/png" },
      },
    ]);
    const warnings = await warningsOf(
      f,
      submitted.runId,
      "attachment_unresolved",
    );
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain("blob:gone");

    // Dropped for the provider, kept in the store — a later turn can try again.
    const stored = messagesOf(f).find((m) => m.id === submitted.userMessageId);
    expect(stored?.content).toEqual(bodyWithRefs("blob:here", "blob:gone"));
    const task = await f.store.tasks.getTask(submitted.runId);
    expect(task?.status).toBe("completed");
  });

  it("drops a ref over the PER-IMAGE byte budget and warns attachment_budget_exceeded", async () => {
    const attachments = new TestAttachmentResolver({
      "blob:small": png(base64OfBytes(90)),
      "blob:huge": png(base64OfBytes(400)),
    });
    // The per-image cap is the only one in play: the two images together are
    // far inside the total, and two is far inside the count.
    const f = await setupRunner({
      attachments,
      attachmentBudgets: {
        maxBytesPerImage: 100,
        maxTotalBytes: 100_000,
        maxImages: 10,
      },
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: bodyWithRefs("blob:small", "blob:huge"),
    });
    await drive(f, submitted.runId);

    expect(imagePartsSeenByProvider(f)).toHaveLength(1);
    const warnings = await warningsOf(
      f,
      submitted.runId,
      "attachment_budget_exceeded",
    );
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain("blob:huge");
    expect(warnings[0]).toContain("per-image");
  });

  it("drops a ref that would exceed the AGGREGATE byte budget and warns attachment_budget_exceeded", async () => {
    const attachments = new TestAttachmentResolver({
      "blob:a": png(base64OfBytes(60)),
      "blob:b": png(base64OfBytes(60)),
    });
    // Each image is under the per-image cap and the pair is inside the count
    // budget; only their SUM breaks a rule.
    const f = await setupRunner({
      attachments,
      attachmentBudgets: {
        maxBytesPerImage: 100,
        maxTotalBytes: 100,
        maxImages: 10,
      },
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: bodyWithRefs("blob:a", "blob:b"),
    });
    await drive(f, submitted.runId);

    // The first fits, the second does not — spend is charged in the MESSAGE's
    // order, not in whatever order the resolver happens to answer.
    expect(imagePartsSeenByProvider(f)).toHaveLength(1);
    const warnings = await warningsOf(
      f,
      submitted.runId,
      "attachment_budget_exceeded",
    );
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain("blob:b");
    expect(warnings[0]).toContain("total budget");
  });

  it("drops a ref past the IMAGE-COUNT budget and warns attachment_budget_exceeded", async () => {
    const attachments = new TestAttachmentResolver({
      "blob:a": png(base64OfBytes(9)),
      "blob:b": png(base64OfBytes(9)),
    });
    // Tiny images, generous byte budgets: the count is the only cap that bites.
    const f = await setupRunner({
      attachments,
      attachmentBudgets: {
        maxBytesPerImage: 100_000,
        maxTotalBytes: 100_000,
        maxImages: 1,
      },
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: bodyWithRefs("blob:a", "blob:b"),
    });
    await drive(f, submitted.runId);

    expect(imagePartsSeenByProvider(f)).toHaveLength(1);
    const warnings = await warningsOf(
      f,
      submitted.runId,
      "attachment_budget_exceeded",
    );
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain("blob:b");
    expect(warnings[0]).toContain("1 image(s)");
  });

  it("resolves a ref appearing twice in one pass exactly once", async () => {
    const attachments = new TestAttachmentResolver({
      "blob:same": png("aGVsbG8="),
    });
    const f = await setupRunner({ attachments });
    f.mock.setScript([{ steps: [{ kind: "text", content: "twice" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: bodyWithRefs("blob:same", "blob:same"),
    });
    await drive(f, submitted.runId);

    // One fetch, two images: it is a cache, not a deduplicator — the model
    // still sees the picture in both positions the message put it in.
    expect(attachments.calls).toEqual(["blob:same"]);
    expect(imagePartsSeenByProvider(f)).toHaveLength(2);
  });

  it("warns and still completes when a refs message meets no resolver at all", async () => {
    const f = await setupRunner();
    f.mock.setScript([{ steps: [{ kind: "text", content: "no pictures" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: bodyWithRefs("blob:orphan"),
    });
    await drive(f, submitted.runId);

    // Every image gone, the words kept, the turn finished.
    expect(userContentSeenByProvider(f)).toEqual([
      { type: "text", text: "what is in these?" },
    ]);
    const warnings = await warningsOf(
      f,
      submitted.runId,
      "attachment_unresolved",
    );
    expect(warnings).toHaveLength(1);
    expect(warnings[0]).toContain("blob:orphan");
    const task = await f.store.tasks.getTask(submitted.runId);
    expect(task?.status).toBe("completed");

    // The warning joined the run's ONE sequence. It is stamped between passes
    // by the same stamper the passes use, so a gap or a repeat here would break
    // every consumer that resumes a stream on `seq`.
    expectOneSequence(await eventsOf(f, submitted.runId), submitted.runId);
  });

  it("leaves a history with no refs completely alone", async () => {
    const attachments = new TestAttachmentResolver({});
    const f = await setupRunner({ attachments });
    f.mock.setScript([{ steps: [{ kind: "text", content: "plain" }] }]);
    const inlined: AiContentPart[] = [
      { type: "text", text: "look" },
      {
        type: "image",
        source: { kind: "data", base64: "aGVsbG8=", mediaType: "image/png" },
      },
    ];
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: inlined,
    });
    await drive(f, submitted.runId);

    // A caller's own inline images are NOT this port's business: untouched,
    // unbudgeted, and the resolver is never asked anything.
    expect(userContentSeenByProvider(f)).toEqual(inlined);
    expect(attachments.calls).toEqual([]);
  });
});

describe("TurnRunner — tool guards", () => {
  /** `echo` plus a second tool, so "hidden" is distinguishable from "empty". */
  const twoToolContributor: ToolSetContributor = {
    namespace: "test",
    contribute: async () => [
      echoTool as AiTool,
      {
        ...(echoTool as AiTool),
        definition: { ...echoTool.definition, name: "secret" },
      },
    ],
  };

  it("never advertises a tool a guard hides", async () => {
    const f = await setupRunner({
      contributors: [twoToolContributor],
      toolGuards: [{ isVisible: (ctx) => ctx.tool.name !== "secret" }],
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "hi" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
    });
    await drive(f, submitted.runId);

    // The provider's own view: what the model was actually shown.
    expect(f.client.toolNamesPerCall[0]).toEqual(["echo"]);
  });

  it("sees the contributor's namespace on the guard context", async () => {
    const namespaces: string[] = [];
    const f = await setupRunner({
      contributors: [echoContributor],
      toolGuards: [
        {
          isVisible: (ctx) => {
            namespaces.push(ctx.namespace);
            return true;
          },
        },
      ],
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "hi" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
    });
    await drive(f, submitted.runId);
    expect(namespaces).toEqual(["test"]);
  });

  it("fails one call — not the run — when a guard refuses at execution", async () => {
    const f = await setupRunner({
      contributors: [echoContributor],
      toolGuards: [
        {
          canExecute: () => ({ allowed: false, reason: "writes are paused" }),
        },
      ],
    });
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
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "use the tool",
    });
    await drive(f, submitted.runId);

    // The tool WAS advertised (canExecute is not isVisible) and was called.
    expect(f.client.toolNamesPerCall[0]).toEqual(["echo"]);

    const events = await eventsOf(f, submitted.runId);
    const failed = events.find((e) => e.type === "run.tool.failed") as
      | (AiRunEvent & { data: { errorMessage: string } })
      | undefined;
    expect(failed?.data.errorMessage).toBe("writes are paused");
    expect(events.some((e) => e.type === "run.tool.succeeded")).toBe(false);

    // The run completed, and the tool message is there to balance the call.
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
    const toolRecord = messagesOf(f).find((m) => m.role === "tool");
    expect(toolRecord?.toolCallId).toBe("call-1");
    const envelope = JSON.parse(toolRecord?.content as string) as {
      ok: boolean;
      status: string;
      data: Record<string, unknown>;
    };
    expect(envelope.ok).toBe(false);
    expect(envelope.status).toBe("error");
    expect(envelope.data).toEqual({
      errorCode: TOOL_GUARD_REFUSED_CODE,
      errorMessage: "writes are paused",
      phase: "guard",
      retryable: false,
    });
    expectOneSequence(events, submitted.runId);
    expect(
      messagesOf(f).find((m) => m.id === submitted.assistantMessageId)?.content,
    ).toBe("understood");
  });
});

describe("TurnRunner — the toolCalling override", () => {
  const CHAT_ONLY: AiProviderCapabilities = {
    streaming: true,
    toolCalling: false,
    modelList: true,
  };

  async function runOneTurn(f: RunnerFixture): Promise<void> {
    f.mock.setScript([{ steps: [{ kind: "text", content: "hi" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "hello",
    });
    await drive(f, submitted.runId);
  }

  it("auto follows the probe: tools when it says yes", async () => {
    const f = await setupRunner({
      contributors: [echoContributor],
      settings: { toolCalling: "auto" },
    });
    await runOneTurn(f);
    expect(f.client.toolNamesPerCall[0]).toEqual(["echo"]);
  });

  it("auto follows the probe: no tools when it says no", async () => {
    const f = await setupRunner({
      contributors: [echoContributor],
      settings: { toolCalling: "auto" },
      capabilities: CHAT_ONLY,
    });
    await runOneTurn(f);
    expect(f.client.toolNamesPerCall[0]).toEqual([]);
  });

  it("defaults to auto when the setting was never written", async () => {
    const f = await setupRunner({ contributors: [echoContributor] });
    await runOneTurn(f);
    expect(f.client.toolNamesPerCall[0]).toEqual(["echo"]);
  });

  it("off stages nothing even on a tool-capable provider", async () => {
    const f = await setupRunner({
      contributors: [echoContributor],
      settings: { toolCalling: "off" },
    });
    await runOneTurn(f);
    expect(f.client.toolNamesPerCall[0]).toEqual([]);
  });

  it("on stages tools despite a probe that says unsupported", async () => {
    const f = await setupRunner({
      contributors: [echoContributor],
      settings: { toolCalling: "on" },
      capabilities: CHAT_ONLY,
    });
    await runOneTurn(f);
    expect(f.client.toolNamesPerCall[0]).toEqual(["echo"]);
  });

  it("off does not ask the contributors at all", async () => {
    let asked = 0;
    const counting: ToolSetContributor = {
      namespace: "test",
      contribute: async () => {
        asked += 1;
        return [echoTool as AiTool];
      },
    };
    const f = await setupRunner({
      contributors: [counting],
      settings: { toolCalling: "off" },
    });
    await runOneTurn(f);
    expect(asked).toBe(0);
  });
});

describe("TurnRunner.disposeContributors", () => {
  function disposable(namespace: string, log: string[]): ToolSetContributor {
    return {
      namespace,
      contribute: async () => [],
      dispose: () => {
        log.push(namespace);
      },
    };
  }

  it("disposes every contributor exactly once, and is idempotent", async () => {
    const log: string[] = [];
    const f = await setupRunner({
      contributors: [disposable("one", log), disposable("two", log)],
    });
    await f.runner.disposeContributors();
    expect(log).toEqual(["one", "two"]);
    // A second signal must not close anything twice.
    await f.runner.disposeContributors();
    expect(log).toEqual(["one", "two"]);
  });

  it("skips a contributor with no dispose hook", async () => {
    const log: string[] = [];
    const f = await setupRunner({
      contributors: [echoContributor, disposable("two", log)],
    });
    await f.runner.disposeContributors();
    expect(log).toEqual(["two"]);
  });

  it("keeps going — and logs — when one contributor's dispose throws", async () => {
    const log: string[] = [];
    const warnings: string[] = [];
    const harness = createHarness();
    const runner = new TurnRunner({
      store: harness.store,
      taskRunner: harness.taskRunner,
      providerFactory: () => new MockProviderClient(),
      contributors: [
        {
          namespace: "bad",
          contribute: async () => [],
          dispose: () => {
            throw new Error("socket already gone");
          },
        },
        disposable("good", log),
      ],
      clock: harness.clock,
      ids: harness.ids,
      logger: {
        debug: () => {},
        info: () => {},
        warn: (message) => warnings.push(message),
        error: () => {},
      },
    });
    await runner.disposeContributors();
    expect(log).toEqual(["good"]);
    expect(warnings).toEqual(["tool contributor dispose failed"]);
  });
});
