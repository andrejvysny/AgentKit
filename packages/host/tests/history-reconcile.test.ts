import { describe, expect, it } from "bun:test";
import type { AiChatMessage, AiRunEvent } from "@agentkit/contracts";
import type { AiChatRequest, AiProviderClient } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  MISSING_TOOL_RESULT_CODE,
  reconcileOrphanToolCalls,
  TurnRunner,
  type MessageRecord,
} from "../src/index.js";
import { createHarness } from "./fakes.js";

function assistantWithCalls(...ids: string[]): AiChatMessage {
  return {
    role: "assistant",
    content: "",
    toolCalls: ids.map((id) => ({ id, name: "echo", argumentsJson: "{}" })),
  };
}

function toolResult(id: string): AiChatMessage {
  return { role: "tool", content: "{}", toolCallId: id, name: "echo" };
}

describe("reconcileOrphanToolCalls", () => {
  it("leaves a balanced history exactly as it was", () => {
    const messages: AiChatMessage[] = [
      { role: "user", content: "hi" },
      assistantWithCalls("call-1", "call-2"),
      toolResult("call-1"),
      toolResult("call-2"),
      { role: "assistant", content: "done" },
    ];
    expect(reconcileOrphanToolCalls(messages)).toEqual(messages);
  });

  it("leaves a history with no tool calls at all alone", () => {
    const messages: AiChatMessage[] = [
      { role: "user", content: "hi" },
      { role: "assistant", content: "hello" },
    ];
    expect(reconcileOrphanToolCalls(messages)).toEqual(messages);
  });

  it("answers each unanswered call in place, in declaration order", () => {
    const messages: AiChatMessage[] = [
      { role: "user", content: "hi" },
      assistantWithCalls("call-1", "call-2"),
      toolResult("call-2"),
      { role: "user", content: "still there?" },
    ];
    const reconciled = reconcileOrphanToolCalls(messages);
    expect(
      reconciled.map((m) => [m.role, m.toolCallId ?? ""] as const),
    ).toEqual([
      ["user", ""],
      ["assistant", ""],
      ["tool", "call-1"],
      ["tool", "call-2"],
      ["user", ""],
    ]);
    const synthetic = JSON.parse(reconciled[2]!.content as string) as {
      ok: boolean;
      status: string;
      data: { errorCode: string };
    };
    // Same slim-envelope shape TurnRunner persists for a real tool failure, so
    // the model cannot tell a reconciled hole from a tool that genuinely failed.
    expect(synthetic.ok).toBe(false);
    expect(synthetic.status).toBe("error");
    expect(synthetic.data.errorCode).toBe(MISSING_TOOL_RESULT_CODE);
    expect(reconciled[2]?.name).toBe("echo");
  });

  it("answers a duplicated call id exactly once", () => {
    const reconciled = reconcileOrphanToolCalls([
      assistantWithCalls("call-1", "call-1"),
    ]);
    expect(reconciled.filter((m) => m.role === "tool")).toHaveLength(1);
  });

  it("does not answer calls that a LATER message already answered", () => {
    const reconciled = reconcileOrphanToolCalls([
      assistantWithCalls("call-1"),
      { role: "assistant", content: "interleaved" },
      toolResult("call-1"),
    ]);
    expect(reconciled.filter((m) => m.role === "tool")).toHaveLength(1);
  });
});

/** Records the messages every provider call was handed. */
class RecordingClient implements AiProviderClient {
  readonly id = "test";
  readonly kind = "openai-compatible";
  readonly seen: AiChatMessage[][] = [];

  constructor(private readonly inner: AiProviderClient) {}

  async capabilities() {
    return this.inner.capabilities();
  }

  async listModels() {
    return this.inner.listModels();
  }

  async *streamChat(input: AiChatRequest): AsyncIterable<AiRunEvent> {
    this.seen.push([...input.messages]);
    yield* this.inner.streamChat(input);
  }
}

describe("TurnRunner history assembly", () => {
  /**
   * The durable shape a turn that died between two writes leaves behind:
   * `run.message.completed` persisted the assistant turn with its `toolCalls`,
   * and the `run.tool.*` events that would have persisted the answers never ran.
   */
  async function seedCrashedTurn(): Promise<{
    runner: TurnRunner;
    client: RecordingClient;
    chatId: string;
    records(): MessageRecord[];
    drive(taskId: string): Promise<void>;
  }> {
    const harness = createHarness();
    const client = new RecordingClient(new MockProviderClient());
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

    await harness.store.conversations.appendMessage({
      chatId: chat.id,
      role: "user",
      content: "what is the weather?",
    });
    await harness.store.conversations.appendMessage({
      chatId: chat.id,
      runId: "run-dead",
      role: "assistant",
      content: "",
      toolCalls: [
        { id: "call-answered", name: "echo", argumentsJson: "{}" },
        { id: "call-lost", name: "echo", argumentsJson: "{}" },
      ],
      metadata: { internal: true },
    });
    await harness.store.conversations.appendMessage({
      chatId: chat.id,
      runId: "run-dead",
      role: "tool",
      content: '{"ok":true}',
      toolCallId: "call-answered",
      metadata: { internal: true, toolName: "echo" },
    });

    const runner = new TurnRunner({
      store: harness.store,
      taskRunner: harness.taskRunner,
      providerFactory: () => client,
      contributors: [],
      clock: harness.clock,
      ids: harness.ids,
    });

    return {
      runner,
      client,
      chatId: chat.id,
      records: () => harness.store.conversations.messages,
      async drive(taskId: string): Promise<void> {
        const attempt = await harness.store.tasks.createAttempt({
          attemptId: harness.ids.attemptId(),
          taskId,
          ownerId: "worker-1",
        });
        const lease = await harness.store.tasks.acquireLease({
          taskId,
          attemptId: attempt.attemptId,
          ownerId: "worker-1",
          ttlMs: 60_000,
        });
        await runner.execute({
          taskId,
          attemptId: attempt.attemptId,
          leaseToken: lease.leaseToken,
          signal: new AbortController().signal,
        });
      },
    };
  }

  it("balances an orphaned tool call left by a previous turn", async () => {
    const f = await seedCrashedTurn();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "and now?",
      model: "m1",
    });
    await f.drive(submitted.runId);

    const messages = f.client.seen[0];
    expect(messages).toBeDefined();
    const declared = messages!
      .flatMap((m) => m.toolCalls ?? [])
      .map((c) => c.id);
    const answered = messages!
      .filter((m) => m.role === "tool")
      .map((m) => m.toolCallId);
    expect(declared.sort()).toEqual(["call-answered", "call-lost"]);
    expect(answered.sort()).toEqual(["call-answered", "call-lost"]);

    const synthetic = messages!.find((m) => m.toolCallId === "call-lost");
    expect(JSON.parse(synthetic!.content as string)).toMatchObject({
      ok: false,
      data: { errorCode: MISSING_TOOL_RESULT_CODE },
    });
  });

  it("does not persist the synthetic result — the records stay truthful", async () => {
    const f = await seedCrashedTurn();
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "and now?",
      model: "m1",
    });
    await f.drive(submitted.runId);

    // The provider saw the reconciled result...
    expect(
      (f.client.seen[0] ?? []).some((m) => m.toolCallId === "call-lost"),
    ).toBe(true);
    // ...and the store still says what actually happened: nothing answered it.
    expect(
      f.records().some((record) => record.toolCallId === "call-lost"),
    ).toBe(false);
  });
});
