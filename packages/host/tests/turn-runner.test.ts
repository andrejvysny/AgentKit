import { describe, expect, it } from "bun:test";
import type { AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient, AiTool } from "@agentkit/core";
import {
  CompletedOnlyProviderClient,
  MockProviderClient,
} from "@agentkit/testing";
import {
  LeaseLostError,
  TurnRunner,
  type MessageRecord,
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
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });
  const runner = new TurnRunner({
    store: harness.store,
    taskRunner: harness.taskRunner,
    providerFactory: () => client,
    contributors: options.contributors ?? [],
    clock: harness.clock,
    ids: harness.ids,
    ...(options.verification === undefined
      ? {}
      : { verification: options.verification }),
    ...(options.historyLimit === undefined
      ? {}
      : { historyLimit: options.historyLimit }),
  });
  return { ...harness, runner, client, mock, chatId: chat.id };
}

/** Stand in for the task runner: create the attempt, take the lease, execute. */
async function drive(
  f: RunnerFixture,
  runId: string,
  opts: { abort?: boolean; leaseToken?: string } = {},
): Promise<{ attemptId: string }> {
  const attempt = await f.store.runs.createAttempt({
    attemptId: f.ids.attemptId(),
    runId,
    ownerId: "worker-1",
  });
  const lease = await f.store.runs.acquireLease({
    runId,
    attemptId: attempt.attemptId,
    ownerId: "worker-1",
    ttlMs: 60_000,
  });
  const controller = new AbortController();
  if (opts.abort) controller.abort();
  await f.runner.execute({
    runId,
    attemptId: attempt.attemptId,
    leaseToken: opts.leaseToken ?? lease.leaseToken,
    signal: controller.signal,
  });
  return { attemptId: attempt.attemptId };
}

/** The event log is one unbroken, deduplicated sequence for the run. */
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
    const run = await f.store.runs.getRun(submitted.runId);
    expect(run?.status).toBe("queued");
    expect(run?.scopeId).toBe(f.chatId);
    expect(f.taskRunner.enqueued).toEqual([
      { runId: submitted.runId, scopeId: f.chatId },
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

describe("TurnRunner.execute — text only", () => {
  it("streams into the placeholder and completes the run", async () => {
    const f = await setupRunner();
    f.mock.setScript([
      { steps: [{ kind: "text", content: "Hello " }, { kind: "text", content: "world" }] },
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

    const run = await f.store.runs.getRun(submitted.runId);
    expect(run?.status).toBe("completed");
    expect(run?.startedAt).toBeDefined();
    expect(run?.finishedAt).toBeDefined();
    expect(f.store.runs.attempts.get(attemptId)?.status).toBe("completed");

    const events = await f.store.runs.listEvents(submitted.runId);
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
    expect((await f.store.runs.getRun(submitted.runId))?.status).toBe(
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
    const envelope = JSON.parse(toolRecord?.content ?? "{}") as {
      data: Record<string, unknown>;
    };
    expect(envelope.data).toEqual({ echoed: "hi" });
    expect(toolRecord?.content).not.toContain("VERBOSE_PAYLOAD");

    const events = await f.store.runs.listEvents(submitted.runId);
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
    expect(
      replayed.some((m) => (m.toolCalls?.length ?? 0) > 0),
    ).toBe(false);
    expect((await f.store.runs.getRun(second.runId))?.status).toBe("completed");
    expect(
      messagesOf(f).find((m) => m.id === second.assistantMessageId)?.content,
    ).toBe("second answer");
  });
});

describe("TurnRunner.execute — retries", () => {
  it("retries chat-only after a provider failure with tools staged", async () => {
    const f = await setupRunner({ contributors: [echoContributor] });
    f.client.failCalls.add(1);
    f.mock.setScript([{ steps: [{ kind: "text", content: "chat only answer" }] }]);
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
    const run = await f.store.runs.getRun(submitted.runId);
    expect(run?.status).toBe("completed");

    const events = await f.store.runs.listEvents(submitted.runId);
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
    expect((await f.store.runs.getRun(submitted.runId))?.status).toBe("failed");
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
    const events = await f.store.runs.listEvents(submitted.runId);
    expectOneSequence(events, submitted.runId);
    const warning = events.find(
      (e) => e.type === "run.warning" && e.data.code === "empty_response",
    );
    expect(warning).toBeDefined();
    expect(warning?.attemptId).toBeDefined();
    expect((await f.store.runs.getRun(submitted.runId))?.status).toBe(
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
    const events = await f.store.runs.listEvents(submitted.runId);
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

    const events = await f.store.runs.listEvents(submitted.runId);
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
            content: 'Here is what I sent:\n```json\n{"name":"echo","arguments":{}}\n```',
          },
        ],
      },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await drive(f, submitted.runId);

    const events = await f.store.runs.listEvents(submitted.runId);
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

    const run = await f.store.runs.getRun(submitted.runId);
    expect(run?.status).toBe("cancelled");
    expect(f.store.runs.attempts.get(attemptId)?.status).toBe("cancelled");

    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.metadata["placeholder"]).toBe(false);
    expect(placeholder?.content).toBe("");

    const events = await f.store.runs.listEvents(submitted.runId);
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

    const run = await f.store.runs.getRun(submitted.runId);
    expect(run?.status).toBe("failed");
    expect(run?.error).toContain("lease");
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
