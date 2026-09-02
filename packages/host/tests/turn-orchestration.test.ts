/**
 * Phase 3 host orchestration: the recovery chain, the throw path, pass
 * boundaries, submit exclusivity and hook deadlines.
 *
 * Separate from `turn-runner.test.ts` — which is already 2k lines and covers
 * the happy path, branching, history assembly and attachments — because
 * everything here is about what a turn does when something goes WRONG: a
 * crashed attempt, an unexpected throw, a recovery pass, a second submit, a
 * hook that never answers.
 */
import { describe, expect, it } from "bun:test";
import type {
  AiContextBinding,
  AiProviderCapabilities,
  AiRunEvent,
} from "@agentkit/contracts";
import type { AiProviderClient, AiTool } from "@agentkit/core";
import { MockProviderClient } from "@agentkit/testing";
import {
  ChatBusyError,
  TurnRunner,
  type AttachmentResolver,
  type ContextProvider,
  type HookTimeouts,
  type MessageRecord,
  type ToolSetContributor,
  type UsageAuthorizer,
  type VerificationHook,
} from "../src/index.js";
import { createHarness, type TestHarness } from "./fakes.js";

/** A read tool with no side effects, so a turn can do "tool work" cheaply. */
const echoTool: AiTool<{ text?: string }, { echoed: string }> = {
  definition: {
    name: "echo",
    version: "1.0.0",
    effect: "read",
    capability: "echo",
    description: "Echo the input.",
    inputSchema: { type: "object", properties: { text: { type: "string" } } },
  },
  async execute(ctx, input) {
    return {
      ok: true,
      data: { echoed: input.text ?? "" },
      summary: "echoed",
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

/** One provider turn that calls `echo`, then one that answers. */
function toolThenText(text: string) {
  return [
    {
      steps: [
        {
          kind: "tool_call" as const,
          toolCallId: `call-${text}`,
          name: "echo",
          argumentsJson: '{"text":"hi"}',
        },
      ],
    },
    { steps: [{ kind: "text" as const, content: text }] },
  ];
}

interface Fixture extends TestHarness {
  runner: TurnRunner;
  mock: MockProviderClient;
  chatId: string;
}

interface SetupOptions {
  contributors?: ToolSetContributor[];
  verification?: VerificationHook;
  context?: ContextProvider;
  usage?: UsageAuthorizer;
  attachments?: AttachmentResolver;
  hookTimeoutsMs?: HookTimeouts;
  allowConcurrentSubmit?: boolean;
  capabilities?: AiProviderCapabilities;
  inner?: AiProviderClient;
}

async function setup(options: SetupOptions = {}): Promise<Fixture> {
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
  if (options.capabilities !== undefined) {
    await harness.store.providers.saveCapabilities("p1", options.capabilities);
  }
  await harness.store.settings.updateSettings({ defaultProviderId: "p1" });
  const chat = await harness.store.conversations.createChat({ id: "chat-1" });
  const client = options.inner ?? mock;
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
    ...(options.context === undefined ? {} : { context: options.context }),
    ...(options.usage === undefined ? {} : { usage: options.usage }),
    ...(options.attachments === undefined
      ? {}
      : { attachments: options.attachments }),
    ...(options.hookTimeoutsMs === undefined
      ? {}
      : { hookTimeoutsMs: options.hookTimeoutsMs }),
    ...(options.allowConcurrentSubmit === undefined
      ? {}
      : { allowConcurrentSubmit: options.allowConcurrentSubmit }),
  });
  return { ...harness, runner, mock, chatId: chat.id };
}

/** One attempt: create it, take the lease, execute. Never swallows. */
async function attempt(
  f: Fixture,
  taskId: string,
  opts: { abort?: boolean } = {},
): Promise<{ attemptId: string; leaseToken: string }> {
  const created = await f.store.tasks.createAttempt({
    attemptId: f.ids.attemptId(),
    taskId,
    ownerId: "worker-1",
  });
  const lease = await f.store.tasks.acquireLease({
    taskId,
    attemptId: created.attemptId,
    ownerId: "worker-1",
    ttlMs: 60_000,
  });
  const controller = new AbortController();
  if (opts.abort) controller.abort();
  await f.runner.execute({
    taskId,
    attemptId: created.attemptId,
    leaseToken: lease.leaseToken,
    signal: controller.signal,
  });
  return { attemptId: created.attemptId, leaseToken: lease.leaseToken };
}

/** The same, swallowing whatever the attempt threw, and reporting it. */
async function attemptQuietly(
  f: Fixture,
  taskId: string,
  opts: { abort?: boolean } = {},
): Promise<unknown> {
  try {
    await attempt(f, taskId, opts);
    return null;
  } catch (err) {
    return err;
  }
}

async function eventsOf(f: Fixture, taskId: string): Promise<AiRunEvent[]> {
  return (await f.store.tasks.listEvents(taskId)) as AiRunEvent[];
}

function messagesOf(f: Fixture): MessageRecord[] {
  return f.store.conversations.messages;
}

/**
 * A verifier that RELEASES THE LEASE and then throws — the shape of a real
 * crash, reproduced without killing the process.
 *
 * The lease is what every durable write in an attempt is fenced on, so once it
 * is gone this attempt cannot land the task: `failQuietly`'s fenced transition
 * raises `LeaseLostError` and stops, leaving the task `running` with a live
 * placeholder — exactly what a worker that died mid-turn leaves behind, and
 * exactly what the recovery pass hands to attempt 2.
 */
function crashingVerification(harness: TestHarness): VerificationHook {
  let crashed = false;
  return {
    verify: async (input) => {
      if (crashed) return { status: "pass", checks: [], deficiencies: [] };
      crashed = true;
      const lease = harness.store.tasks.leases.get(input.runId);
      if (lease) await harness.store.tasks.releaseLease(lease.leaseToken);
      throw new Error("worker died mid-turn");
    },
  };
}

// ───────────────────────────────────────────────────────────────────────────
// 3.1 — C1: the recovery chain
// ───────────────────────────────────────────────────────────────────────────

describe("TurnRunner — a second attempt continues the first one's chain (C1)", () => {
  it("lands attempt 2's records on the ACTIVE path, not on a dead branch", async () => {
    const f = await setup({ contributors: [echoContributor] });
    // The hook crashes the FIRST attempt and passes on every later one; it
    // needs the fixture, which is why it is wired after `setup` builds one.
    const verification = crashingVerification(f);
    const runner = new TurnRunner({
      store: f.store,
      taskRunner: f.taskRunner,
      providerFactory: () => f.mock,
      contributors: [echoContributor],
      clock: f.clock,
      ids: f.ids,
      verification,
    });
    const withCrash: Fixture = { ...f, runner };
    withCrash.mock.setScript([
      ...toolThenText("first"),
      ...toolThenText("second"),
    ]);
    const submitted = await withCrash.runner.submitMessage({
      chatId: f.chatId,
      content: "do the thing",
    });

    // Attempt 1 runs the tool, writes its internal assistant record and its
    // tool result, then dies.
    const thrown = await attemptQuietly(withCrash, submitted.runId);
    expect((thrown as Error).message).toBe("worker died mid-turn");

    // The crash left the task running with a live placeholder — the state a
    // recovery pass finds.
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "running",
    );
    const afterCrash = messagesOf(f).filter(
      (m) => m.runId === submitted.runId && m.metadata["internal"] === true,
    );
    expect(afterCrash.map((m) => m.role)).toEqual(["assistant", "tool"]);
    // A FENCED-OUT attempt records nothing — not even a failure. It could not
    // prove it still owns the task, so the placeholder is left for whoever
    // does; finalizing it here would blank a live answer under attempt 2.
    expect(
      messagesOf(f).find((m) => m.id === submitted.assistantMessageId)
        ?.metadata["placeholder"],
    ).toBe(true);

    // Attempt 2, in place, on the same task — exactly what the runner's
    // recovery pass does: the task stays `running`, a new attempt and a new
    // lease are taken, and the SAME `runTurn` executes again.
    await attempt(withCrash, submitted.runId);
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );

    // THE ASSERTION. Attempt 2 chained off attempt 1's deepest record, so its
    // internal assistant turn and its tool result are on the path every later
    // turn replays. Seeded from the placeholder instead — what this used to do
    // — the placeholder already had an active child, so everything attempt 2
    // wrote landed `active: false` and vanished from the conversation.
    const path = await f.store.conversations.listMessages(f.chatId);
    const internalOnPath = path.filter(
      (m) => m.metadata["internal"] === true && m.runId === submitted.runId,
    );
    expect(internalOnPath).toHaveLength(4);
    expect(internalOnPath.map((m) => m.role)).toEqual([
      "assistant",
      "tool",
      "assistant",
      "tool",
    ]);
    // Attempt 2's first record hangs off attempt 1's last one, not off the
    // placeholder.
    expect(internalOnPath[2]?.parentMessageId).toBe(internalOnPath[1]?.id);

    // And the history the NEXT turn replays is balanced: every tool call on
    // the path has its result on the path, so nothing is reconciled into a
    // synthetic failure for a tool that actually ran.
    f.mock.setScript([{ steps: [{ kind: "text", content: "ok" }] }]);
    const next = await withCrash.runner.submitMessage({
      chatId: f.chatId,
      content: "and again",
    });
    await attempt(withCrash, next.runId);
    const replayed = messagesOf(f).filter(
      (m) =>
        typeof m.content === "string" &&
        m.content.includes("tool_result_missing"),
    );
    expect(replayed).toEqual([]);
  });

  it("seeds from the placeholder on attempt 1, exactly as before", async () => {
    const f = await setup({ contributors: [echoContributor] });
    f.mock.setScript(toolThenText("answer"));
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);

    const internal = messagesOf(f).filter(
      (m) => m.metadata["internal"] === true,
    );
    expect(internal[0]?.parentMessageId).toBe(submitted.assistantMessageId);
    expect(internal.every((m) => m.active)).toBe(true);
  });
});

// ───────────────────────────────────────────────────────────────────────────
// 3.2 — C2/C12/C7/C10: the throw path
// ───────────────────────────────────────────────────────────────────────────

describe("TurnRunner — an unexpected throw is bookkept in full (C2)", () => {
  it("writes run.failed, lands the task, and finalizes the placeholder keeping what streamed", async () => {
    const verification: VerificationHook = {
      verify: async () => {
        throw new Error("verifier exploded");
      },
    };
    const f = await setup({ contributors: [echoContributor], verification });
    f.mock.setScript(toolThenText("half an answer"));
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    const thrown = await attemptQuietly(f, submitted.runId);
    expect((thrown as Error).message).toBe("verifier exploded");

    // The task lands, as it always did…
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "failed",
    );
    // …and so do the two writes a client actually sees. Without the terminal
    // event an SSE consumer watches the stream stop with no explanation;
    // without the metadata write the UI spins on the message forever.
    const events = await eventsOf(f, submitted.runId);
    const terminal = events.at(-1);
    expect(terminal?.type).toBe("run.failed");
    expect(
      (terminal as { data: { errorMessage: string; errorCode?: string } }).data,
    ).toEqual({
      errorMessage: "verifier exploded",
      errorCode: "internal_error",
    });
    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.metadata["placeholder"]).toBe(false);
    // What streamed before the break is KEPT: a partial answer plus a terminal
    // event says more than a blank bubble.
    expect(placeholder?.content).toBe("half an answer");
  });

  it("reports a host error's own code rather than inventing one", async () => {
    const f = await setup();
    // No model anywhere: `runTurn` throws `AgentKitHostError("no_model")`
    // before a provider is ever built.
    await f.store.providers.upsertProvider({
      id: "p1",
      label: "Mock",
      kind: "openai-compatible",
      baseUrl: "http://localhost:1234",
      defaultModel: "",
      enabled: true,
    });
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attemptQuietly(f, submitted.runId);

    const events = await eventsOf(f, submitted.runId);
    expect(events.at(-1)?.type).toBe("run.failed");
    expect(
      (events.at(-1) as { data: { errorCode?: string } }).data.errorCode,
    ).toBe("no_model");
  });

  it("lands CANCELLED, not failed, when the run was aborted (C12)", async () => {
    // A context provider that honours the signal, as a real one does: the
    // abort surfaces as a throw out of the hook, before the first pass.
    const context: ContextProvider = {
      listBindings: async (_chatId, signal) => {
        if (signal?.aborted) throw new Error("aborted");
        return [];
      },
    };
    const f = await setup({ context });
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attemptQuietly(f, submitted.runId, { abort: true });

    // A user pressing stop is not an error, and every consumer downstream —
    // the retry decision, the UI, the run phase — reads this status.
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "cancelled",
    );
    const events = await eventsOf(f, submitted.runId);
    expect(events.at(-1)?.type).toBe("run.cancelled");
    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.metadata["placeholder"]).toBe(false);
  });

  it("does not double-report a usage refusal, which already wrote its own terminal", async () => {
    const usage: UsageAuthorizer = {
      authorize: async () => ({ allowed: false, reason: "out of budget" }),
      record: async () => {},
    };
    const f = await setup({ usage });
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attemptQuietly(f, submitted.runId);

    const failures = (await eventsOf(f, submitted.runId)).filter(
      (event) => event.type === "run.failed",
    );
    expect(failures).toHaveLength(1);
    expect(
      (failures[0] as { data: { errorCode?: string } }).data.errorCode,
    ).toBe("usage_denied");
  });

  it("survives a UsageAuthorizer.record that throws (C7)", async () => {
    const recorded: string[] = [];
    const usage: UsageAuthorizer = {
      authorize: async () => ({ allowed: true }),
      record: async (input) => {
        recorded.push(input.callId);
        throw new Error("metering service is down");
      },
    };
    const f = await setup({ usage });
    f.mock.emitUsage = true;
    f.mock.setScript([{ steps: [{ kind: "text", content: "answered" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);

    // The bookkeeping blip is a log line, not the end of a turn the provider
    // has already been paid for.
    expect(recorded.length).toBeGreaterThan(0);
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.content).toBe("answered");
  });

  it("keeps the turn's other tools when one contributor throws (C10)", async () => {
    const broken: ToolSetContributor = {
      namespace: "broken",
      contribute: async () => {
        throw new Error("mcp server is down");
      },
    };
    const f = await setup({ contributors: [broken, echoContributor] });
    f.mock.setScript(toolThenText("done"));
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);

    // One unreachable contributor used to fail every turn in every chat.
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
    const events = await eventsOf(f, submitted.runId);
    expect(events.some((event) => event.type === "run.tool.succeeded")).toBe(
      true,
    );
  });
});

// ───────────────────────────────────────────────────────────────────────────
// 3.3 — F-OWN-1 (host half): the pass boundary
// ───────────────────────────────────────────────────────────────────────────

/** Every `retry_pass` warning on the log, in order. */
function retryPasses(
  events: readonly AiRunEvent[],
): { pass?: number; reason?: string }[] {
  return events
    .filter(
      (event): event is Extract<AiRunEvent, { type: "run.warning" }> =>
        event.type === "run.warning" && event.data.code === "retry_pass",
    )
    .map((event) => ({ pass: event.data.pass, reason: event.data.reason }));
}

describe("TurnRunner — every recovery pass announces itself (3.3)", () => {
  it("marks the chat-only retry, before the retry's own events", async () => {
    // The endpoint that accepts a plain chat request and rejects the same one
    // with a `tools` array attached — the case the chat-only retry exists for.
    const inner = new MockProviderClient();
    inner.setScript([{ steps: [{ kind: "text", content: "plain answer" }] }]);
    let calls = 0;
    const client: AiProviderClient = {
      id: "failing",
      kind: "openai-compatible",
      capabilities: async () => ({
        streaming: true,
        toolCalling: true,
        modelList: false,
      }),
      listModels: async () => [],
      async *streamChat(input) {
        calls += 1;
        if (calls === 1) throw new Error("tools not supported here");
        yield* inner.streamChat(input);
      },
    };
    const f = await setup({ contributors: [echoContributor], inner: client });
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);

    const events = await eventsOf(f, submitted.runId);
    expect(retryPasses(events)).toEqual([{ pass: 2, reason: "chat_only" }]);
    // ORDER IS THE CONTRACT. The pass-1 terminal a consumer would otherwise
    // read as the run's answer comes first, then the boundary telling it the
    // run is live again, then the retry's own events. A consumer that resets
    // its streamed text on the boundary is in step with the host, which resets
    // the stored placeholder at the same point.
    const types = events.map((event) => event.type);
    const boundary = events.findIndex(
      (event) =>
        event.type === "run.warning" && event.data.code === "retry_pass",
    );
    expect(types.indexOf("run.failed")).toBeLessThan(boundary);
    expect(types.lastIndexOf("run.started")).toBeGreaterThan(boundary);
    expect(types.lastIndexOf("run.completed")).toBeGreaterThan(boundary);
    // …and the retry's answer is the one that survives.
    const placeholder = messagesOf(f).find(
      (m) => m.id === submitted.assistantMessageId,
    );
    expect(placeholder?.content).toBe("plain answer");
  });

  it("marks the empty-response retry", async () => {
    const f = await setup();
    f.mock.setScript([
      { steps: [] },
      { steps: [{ kind: "text", content: "second time lucky" }] },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);

    expect(retryPasses(await eventsOf(f, submitted.runId))).toEqual([
      { pass: 2, reason: "empty_response" },
    ]);
  });

  it("marks each correction pass, numbering on from the run's own passes", async () => {
    let verifications = 0;
    const verification: VerificationHook = {
      verify: async () => {
        verifications += 1;
        return verifications === 1
          ? {
              status: "partial",
              checks: [],
              deficiencies: ["one thing left"],
            }
          : { status: "pass", checks: [], deficiencies: [] };
      },
    };
    const harness = await setup({
      contributors: [echoContributor],
      verification,
    });
    const runner = new TurnRunner({
      store: harness.store,
      taskRunner: harness.taskRunner,
      providerFactory: () => harness.mock,
      contributors: [echoContributor],
      clock: harness.clock,
      ids: harness.ids,
      verification,
      correction: { maxPasses: 2 },
    });
    const f: Fixture = { ...harness, runner };
    f.mock.setScript([
      ...toolThenText("first answer"),
      { steps: [{ kind: "text", content: "corrected" }] },
    ]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);

    // The run's own answer was pass 1; the correction is pass 2.
    expect(retryPasses(await eventsOf(f, submitted.runId))).toEqual([
      { pass: 2, reason: "correction" },
    ]);
  });

  it("says nothing on a turn that needed no recovery", async () => {
    const f = await setup();
    f.mock.setScript([{ steps: [{ kind: "text", content: "fine" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);
    expect(retryPasses(await eventsOf(f, submitted.runId))).toEqual([]);
  });
});

// ───────────────────────────────────────────────────────────────────────────
// 3.4 — C5: submit exclusivity
// ───────────────────────────────────────────────────────────────────────────

describe("TurnRunner — one live turn per chat (C5)", () => {
  it("refuses a second submit with chat_busy, and writes nothing", async () => {
    const f = await setup();
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "one",
    });
    const before = messagesOf(f).length;

    let caught: unknown;
    try {
      await f.runner.submitMessage({ chatId: f.chatId, content: "two" });
    } catch (err) {
      caught = err;
    }
    expect(caught).toBeInstanceOf(ChatBusyError);
    expect((caught as ChatBusyError).code).toBe("chat_busy");
    expect((caught as ChatBusyError).details?.["taskIds"]).toEqual([
      first.runId,
    ]);
    // The task goes in FIRST inside the transaction, so the refusal lands
    // before the user message or the placeholder: a store whose `transaction`
    // is only a logical grouping would otherwise leak two orphan messages.
    expect(messagesOf(f).length).toBe(before);
    expect(f.store.tasks.tasks.size).toBe(1);
  });

  it("refuses a regenerate while the chat is still answering", async () => {
    const f = await setup();
    f.mock.setScript([{ steps: [{ kind: "text", content: "blue sky" }] }]);
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "why is the sky blue?",
    });
    await attempt(f, first.runId);
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "and the sea?",
    });

    let caught: unknown;
    try {
      await f.runner.regenerate({
        chatId: f.chatId,
        messageId: first.assistantMessageId,
      });
    } catch (err) {
      caught = err;
    }
    expect(caught).toBeInstanceOf(ChatBusyError);
    expect((caught as ChatBusyError).details?.["taskIds"]).toEqual([
      second.runId,
    ]);
  });

  it("still answers a REDELIVERED submit from the record it already has", async () => {
    const f = await setup();
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "one",
      taskId: "idem-1",
    });
    // The first turn is still queued. A retry of the SAME key is a redelivery,
    // not a second turn — reporting `chat_busy` here would make an idempotent
    // caller retry forever instead of reading back the run it already has.
    const replay = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "one",
      taskId: "idem-1",
    });
    expect(replay).toEqual(first);
    expect(f.store.tasks.tasks.size).toBe(1);
  });

  it("accepts the second turn once the first has landed", async () => {
    const f = await setup();
    f.mock.setScript([
      { steps: [{ kind: "text", content: "a" }] },
      { steps: [{ kind: "text", content: "b" }] },
    ]);
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "one",
    });
    await attempt(f, first.runId);
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "two",
    });
    expect(second.runId).not.toBe(first.runId);
  });

  it("queues instead, for a host that opted into concurrent submits", async () => {
    const f = await setup({ allowConcurrentSubmit: true });
    const first = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "one",
    });
    const second = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "two",
    });
    expect(second.runId).not.toBe(first.runId);
    expect(f.store.tasks.tasks.size).toBe(2);
  });

  it("does not refuse a submit into a DIFFERENT chat", async () => {
    const f = await setup();
    await f.store.conversations.createChat({ id: "chat-2" });
    await f.runner.submitMessage({ chatId: f.chatId, content: "one" });
    const other = await f.runner.submitMessage({
      chatId: "chat-2",
      content: "one",
    });
    expect(other.chatId).toBe("chat-2");
  });
});

// ───────────────────────────────────────────────────────────────────────────
// 3.5 — C6: hook deadlines
// ───────────────────────────────────────────────────────────────────────────

/** A promise that never settles — the hook this whole section is about. */
function never<T>(): Promise<T> {
  return new Promise<T>(() => {});
}

describe("TurnRunner — host hooks run under a deadline (C6)", () => {
  it("runs the turn with no bindings when the context provider hangs", async () => {
    const context: ContextProvider = {
      listBindings: () => never<AiContextBinding[]>(),
    };
    const f = await setup({ context, hookTimeoutsMs: { context: 15 } });
    f.mock.setScript([{ steps: [{ kind: "text", content: "answered" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);

    // Degraded, not failed: a hung provider used to hold the lease forever and
    // leave the chat undeletable.
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
    const warnings = (await eventsOf(f, submitted.runId)).filter(
      (event) => event.type === "run.warning",
    );
    expect(
      warnings.map((event) => (event as { data: { code: string } }).data.code),
    ).toContain("hook_timeout");
  });

  it("runs the turn without a system prompt when that hook hangs", async () => {
    const context: ContextProvider = {
      listBindings: async () => [],
      systemPrompt: () => never<string | null>(),
    };
    const f = await setup({ context, hookTimeoutsMs: { context: 15 } });
    f.mock.setScript([{ steps: [{ kind: "text", content: "answered" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
  });

  it("stages the other contributors' tools when one hangs", async () => {
    const hanging: ToolSetContributor = {
      namespace: "hanging",
      contribute: () => never<AiTool[]>(),
    };
    const f = await setup({
      contributors: [hanging, echoContributor],
      hookTimeoutsMs: { contribute: 15 },
    });
    f.mock.setScript(toolThenText("done"));
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    await attempt(f, submitted.runId);

    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
    const events = await eventsOf(f, submitted.runId);
    // `echo` was still staged and still ran…
    expect(events.some((event) => event.type === "run.tool.succeeded")).toBe(
      true,
    );
    // …and the loss is on the log, so a shrunken tool set is not mistaken for
    // a model that stopped using its tools.
    expect(
      events.some(
        (event) =>
          event.type === "run.warning" && event.data.code === "hook_timeout",
      ),
    ).toBe(true);
  });

  it("fails the turn — bounded — when the single-shot verifier hangs", async () => {
    const verification: VerificationHook = { verify: () => never() };
    const f = await setup({
      contributors: [echoContributor],
      verification,
      hookTimeoutsMs: { verify: 15 },
    });
    f.mock.setScript(toolThenText("done"));
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: "go",
    });
    const thrown = await attemptQuietly(f, submitted.runId);

    // UNCHANGED SEMANTICS: a single-shot `verify()` that throws has always
    // failed the turn. What changed is that it now stops.
    expect((thrown as { code?: string }).code).toBe("hook_timeout");
    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "failed",
    );
  });

  it("drops the attachment when the resolver hangs", async () => {
    const attachments: AttachmentResolver = { resolve: () => never() };
    const f = await setup({
      attachments,
      hookTimeoutsMs: { attachments: 15 },
    });
    f.mock.setScript([{ steps: [{ kind: "text", content: "answered" }] }]);
    const submitted = await f.runner.submitMessage({
      chatId: f.chatId,
      content: [
        { type: "text", text: "what is this?" },
        { type: "image", source: { kind: "ref", ref: "blob:abc" } },
      ],
    });
    await attempt(f, submitted.runId);

    expect((await f.store.tasks.getTask(submitted.runId))?.status).toBe(
      "completed",
    );
    expect(
      (await eventsOf(f, submitted.runId)).some(
        (event) =>
          event.type === "run.warning" &&
          event.data.code === "attachment_unresolved",
      ),
    ).toBe(true);
  });
});
